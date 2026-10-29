//! G573 (a fleet finding, 2026-10-29: scripts-fast's fix waited for its run, one worker did not
//! answer that run, and ten changes on its paths waited a whole hour before the runner tried the
//! check again). A post check no worker answers waits before its box tries it again (G449); a
//! check that holds changes, a red one or one whose fix waits for its run, waits five minutes,
//! and every other check still waits the hour.
//!
//! `suite` turns red, then neither post check is answered on the next tip. On the tip after it,
//! with the short wait over and the hour not, the runner runs the red `suite`, and its pass
//! clears the red; the green `quiet` still waits.
//!
//! Guards to mutate, in `src/bisect.rs`: in `tips`, the `red_of(entry).is_some()` given to
//! `waits_for_a_worker` (`false`: the red check waits the hour and stays red; `true`: the green
//! one runs too); in `unavailable_wait`, the shorter wait of a check that holds.

// Each case runs shell scripts (the adapter), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::{TestBox, result_json};

/// A pre check `tests` that always passes, a post check `suite` on `src/**` and a post check
/// `quiet` on `conf/**`.
const POLICY: &str = "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n\n[[check]]\nid = \"quiet\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"conf/**\"]\n";

/// The adapter. The post checks read `conf/state`: `away`, no worker answers them; `fail`,
/// `suite` fails; else they pass. The pre check always passes.
const ADAPTER: &str = r#"#!/bin/sh
req=$(cat)
commit=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\1/p')
tree=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\2/p')
verdict=pass
case "$req" in
*'"plan":{"id":"suite"'*|*'"plan":{"id":"quiet"'*)
  state=$(cat conf/state)
  if [ "$state" = away ]; then
    printf '%s\n' '{"schema":"bay.check.result/1","unavailable":"no worker can start the ticket"}'
    exit 0
  fi
  case "$req" in *'"plan":{"id":"suite"'*) [ "$state" = fail ] && verdict=product_failure ;; esac
  ;;
esac
printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"selected":1,"completed":1},"checks":[{"id":"t1","verdict":"%s"}]}' "$commit" "$tree" "$verdict"
"#;

/// The records of post check `check` on the remote about `tip`.
fn records(test_box: &TestBox, remote: &Path, check: &str, tip: &str) -> Vec<Value> {
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
                record["check"]["id"] == check && record["subject"]["commit"] == tip
            }));
        }
    }
    out
}

/// Lands a change on `paths` that writes `state` to `conf/state` (and `src/<name>.rs` when
/// `paths` holds `src`), and returns main's new tip.
fn land(
    test_box: &TestBox,
    (repo, remote): (&Path, &Path),
    name: &str,
    paths: &[&str],
    state: &str,
) -> String {
    let mut args = vec!["start", name, "--paths"];
    args.extend(paths);
    let started = test_box.bay_ok(repo, &args);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("conf/state"), format!("{state}\n")).unwrap();
    if paths.contains(&"src") {
        std::fs::write(view.join(format!("src/{name}.rs")), "x\n").unwrap();
    }
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    assert!(
        test_box.trunk_change_ids(remote).contains(&id),
        "{name} lands: {merged}"
    );
    test_box
        .git_bare(remote, &["rev-parse", "main"])
        .trim()
        .to_owned()
}

#[test]
fn a_red_check_no_worker_answered_is_tried_again_sooner_than_a_green_one() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let files = vec![
        ("scripts/bay-check".to_owned(), ADAPTER.to_owned()),
        (".bay/policy.toml".to_owned(), POLICY.to_owned()),
        (
            ".bay/config.toml".to_owned(),
            "[runner]\npost = true\nbisect_post = false\n".to_owned(),
        ),
    ];
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("conf/state", "pass\n")],
        &files,
    );
    let pass = |env: &[(&str, &str)]| {
        let mut cmd = test_box.bay(&repo);
        for (key, value) in env {
            cmd.env(key, value);
        }
        let (code, ran) = result_json(cmd.args(["runner", "run", "--once"]));
        assert_eq!(code, 0, "{ran}");
        ran
    };
    let red = || test_box.bay_ok(&repo, &["status"])["detail"]["red"].clone();

    // GIVEN `one` on src/ and conf/, which fails `suite`: suite is red, and quiet passes.
    let one = land(&test_box, (&repo, &remote), "one", &["src", "conf"], "fail");
    let mut ran = Vec::new();
    for _ in 0..3 {
        if !records(&test_box, &remote, "suite", &one).is_empty()
            && !records(&test_box, &remote, "quiet", &one).is_empty()
        {
            break;
        }
        ran.push(pass(&[]));
    }
    assert_eq!(red()[0]["check"], "suite", "suite is red: {ran:?}");
    assert_eq!(
        records(&test_box, &remote, "quiet", &one)[0]["verdict"],
        "pass",
        "{ran:?}"
    );

    // AND on the next tip no worker answers either post check: each pass tries one, and the
    // box notes both.
    let away = land(&test_box, (&repo, &remote), "away", &["conf"], "away");
    let ran = [pass(&[]), pass(&[])];
    let noted: Value = serde_json::from_str(
        &std::fs::read_to_string(repo.join(".git/bay/post-unavailable.json")).unwrap(),
    )
    .unwrap();
    assert!(
        noted.get("suite@local").is_some() && noted.get("quiet@local").is_some(),
        "both post checks were tried and noted: {noted} {ran:?}"
    );
    assert_eq!(
        records(&test_box, &remote, "suite", &away),
        Vec::<Value>::new()
    );
    assert_eq!(
        records(&test_box, &remote, "quiet", &away),
        Vec::<Value>::new()
    );

    // WHEN workers answer again on the tip after it, and the short wait is over while the hour
    // is not.
    let back = land(&test_box, (&repo, &remote), "back", &["conf"], "pass");
    let short = [("BAY_TEST_HOLDING_UNAVAILABLE_WAIT_S", "0")];
    let ran = [pass(&short), pass(&short)];

    // THEN the red `suite` runs, and its pass clears the red; the green `quiet` still waits.
    let suite = records(&test_box, &remote, "suite", &back);
    assert_eq!(suite.len(), 1, "the red check ran again: {ran:?}");
    assert_eq!(suite[0]["verdict"], "pass", "{}", suite[0]);
    assert_eq!(red(), json!([]), "its pass clears the red: {ran:?}");
    assert!(
        records(&test_box, &remote, "quiet", &back).is_empty(),
        "the green check still waits the hour: {ran:?}"
    );
}
