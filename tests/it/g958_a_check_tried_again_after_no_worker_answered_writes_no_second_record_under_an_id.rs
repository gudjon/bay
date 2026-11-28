//! G958 (BACKLOG 367; a fleet finding, 2026-11-27: a candidate whose check no worker answered was
//! tried again by the same runner, and the try failed "two different records under one evidence
//! id"; the change merged on another runner). An attempt that ends `CHECK_UNAVAILABLE` pushes
//! nothing, but the records it kept stay in this clone. The next try runs again each check those
//! records do not answer (`unknown`, `infra_failure`), and that run takes an id of its own: no two
//! runs share an id (G29).
//!
//! The policy has two checks of one adapter, both reading `PROBE`. While it is `away`, `far`
//! answers `unavailable`, and in the first case `tests` gives `infra_failure`.
//!
//! Guards to mutate: in `src/evidence.rs`, `unused_id`'s free id (the id as given: the try fails
//! as the fleet saw); in `src/publish.rs`, the keep hook's call of it (the same).

// Each case runs shell scripts (the adapter), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use support::{TestBox, result_json};

/// Two pre-merge checks of one adapter, each given `PROBE`.
const POLICY: &str = "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nenv = [\"PROBE\"]\nexpected_minimum = 1\ntimeout_s = 60\n\n[[check]]\nid = \"far\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nenv = [\"PROBE\"]\nexpected_minimum = 1\ntimeout_s = 60\n";

/// The adapter. While `PROBE` is `away`, `far` answers `unavailable`, and `tests` gives
/// `infra_failure` when `src/flaky.rs` is in the candidate. Every other run passes.
const ADAPTER: &str = r#"#!/bin/sh
req=$(cat)
commit=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\1/p')
tree=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\2/p')
t1='{"id":"t1","verdict":"pass"}'
case "$req" in
*'"plan":{"id":"far"'*)
  if [ "$PROBE" = away ]; then
    printf '%s\n' '{"schema":"bay.check.result/1","unavailable":"no worker for far"}'
    exit 0
  fi
  ;;
*)
  if [ "$PROBE" = away ] && [ -f src/flaky.rs ]; then
    t1='{"id":"t1","verdict":"infra_failure","reason":"no key on this box"}'
  fi
  ;;
esac
printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"selected":1,"completed":1},"checks":[%s]}' "$commit" "$tree" "$t1"
"#;

/// One runner pass in `repo` with `PROBE` set to `probe`.
fn pass(test_box: &TestBox, repo: &Path, probe: &str) -> Value {
    let (code, ran) = result_json(
        test_box
            .bay(repo)
            .env("PROBE", probe)
            .args(["runner", "run", "--once"]),
    );
    assert_eq!(code, 0, "{ran}");
    ran
}

/// The `tests` records on the remote about `tree`.
fn tests_records(test_box: &TestBox, remote: &Path, tree: &str) -> Vec<Value> {
    let refs = test_box.git_bare(
        remote,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/notes/bay/evidence/",
        ],
    );
    let mut out = Vec::new();
    for notes_ref in refs.lines() {
        let blobs = test_box.git_bare(
            remote,
            &["ls-tree", "-r", "--format=%(objectname)", notes_ref],
        );
        for blob in blobs.lines() {
            let text = test_box.git_bare(remote, &["cat-file", "blob", blob]);
            let records: Vec<Value> = serde_json::from_str(&text).unwrap();
            out.extend(records.into_iter().filter(|record| {
                record["check"]["id"] == "tests"
                    && record["kind"] == "check"
                    && record["subject"]["tree"] == tree
            }));
        }
    }
    out
}

/// Seeds a repository, queues a change that writes `file`, has a runner try it while `far` has
/// no worker, then tries it again until it lands. Returns the passes after the first and the
/// `tests` records about the landed tree.
fn tried_again(file: &str) -> (String, Vec<Value>) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let files = vec![
        ("scripts/bay-check".to_owned(), ADAPTER.to_owned()),
        (".bay/policy.toml".to_owned(), POLICY.to_owned()),
    ];
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let started = test_box.bay_ok(&repo, &["start", "again", "--paths", "src"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(file), "fn b() {}\n").unwrap();
    let (code, queued) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    let first = pass(&test_box, &repo, "away");
    assert_eq!(
        first["detail"]["ran"][0]["reason"], "CHECK_UNAVAILABLE",
        "{first}"
    );
    let mut passes = Vec::new();
    for _ in 0..8 {
        std::thread::sleep(Duration::from_secs(2));
        passes.push(pass(&test_box, &repo, "back"));
        if test_box.trunk_change_ids(&remote).contains(&id) {
            break;
        }
    }
    let text = serde_json::to_string(&passes).unwrap();
    assert!(test_box.trunk_change_ids(&remote).contains(&id), "{text}");
    let tree = test_box
        .git_bare(&remote, &["rev-parse", "main^{tree}"])
        .trim()
        .to_owned();
    (text, tests_records(&test_box, &remote, &tree))
}

#[test]
fn a_check_that_gave_no_answer_before_the_outage_runs_again_under_an_id_of_its_own() {
    // GIVEN a change on which `tests` gives `infra_failure` while `far` has no worker
    // WHEN the same runner tries it again once `far` has one
    let (passes, records) = tried_again("src/flaky.rs");

    // THEN it lands, both runs of `tests` are on the tree under two ids, and the pass answers
    assert!(!passes.contains("two different records"), "{passes}");
    let verdicts: Vec<&str> = records
        .iter()
        .map(|record| record["verdict"].as_str().unwrap())
        .collect();
    assert_eq!(verdicts, ["infra_failure", "pass"], "{records:?}");
    assert_ne!(records[0]["id"], records[1]["id"], "{records:?}");
}

#[test]
fn a_check_answered_before_the_outage_is_not_run_again() {
    // GIVEN a change on which `tests` passes while `far` has no worker
    // WHEN the same runner tries it again once `far` has one
    let (passes, records) = tried_again("src/b.rs");

    // THEN it lands on the first try's `tests` record alone
    assert!(!passes.contains("two different records"), "{passes}");
    let verdicts: Vec<&str> = records
        .iter()
        .map(|record| record["verdict"].as_str().unwrap())
        .collect();
    assert_eq!(verdicts, ["pass"], "{records:?}");
}
