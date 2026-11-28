//! G957 (BACKLOG 376; a fleet finding, 2026-11-28: a unit that reads the host's fonts failed on a
//! machine that has none, on the candidate and on its base, while the base's record of the check
//! came from a machine that has them; the failure counted as new, and the change was refused
//! `CHECK_FAILED`). A check with `baseline = "base"` that failed is judged against a base record
//! another producer made only when that record fails each unit the run failed. A record that
//! passes one of them may pass it only on its own machine: the base is run here first, and the
//! candidate's record names the record it set aside (`baseline.passed_elsewhere`).
//!
//! The adapter's unit `fonts` fails, on any tree, while the declared `PROBE` variable says
//! `nofonts`; its unit `t1` fails on a tree that holds `src/broken.rs`.
//!
//! Guards to mutate, in `src/check.rs`: `base_answer` reusing any record that answers (make the
//! producer test `true`: the first case is refused), `fails_each` (make it `false`: the third
//! case runs the base again), and `run_all` stating `passed_elsewhere` (drop it: the first and
//! second cases name no set-aside record).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, result_json};

/// The policy: one check `tests` of two units, judged against its base, which declares `PROBE`.
const POLICY: &str = "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 2\ntimeout_s = 60\nenv = [\"PROBE\"]\nbaseline = \"base\"\n";

/// The adapter: `fonts` fails while `PROBE` says `nofonts`, `t1` fails on a tree with
/// `src/broken.rs`.
const ADAPTER: &str = r#"#!/bin/sh
req=$(cat)
commit=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\1/p')
tree=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\2/p')
t1='{"id":"t1","verdict":"pass"}'
if [ -f src/broken.rs ]; then t1='{"id":"t1","verdict":"product_failure","reason":"t1 broke"}'; fi
fonts='{"id":"fonts","verdict":"pass"}'
if [ "$PROBE" = nofonts ]; then fonts='{"id":"fonts","verdict":"product_failure","reason":"no fonts here"}'; fi
printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"discovered":2,"selected":2,"completed":2},"checks":[%s,%s]}' "$commit" "$tree" "$t1" "$fonts"
"#;

/// A repository with the check, and its remote.
fn setup(test_box: &TestBox) -> (PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let files = vec![
        ("scripts/bay-check".to_owned(), ADAPTER.to_owned()),
        (".bay/policy.toml".to_owned(), POLICY.to_owned()),
    ];
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    (repo, remote)
}

/// A runner's clone of `remote` named `name` on machine `machine`; its producer is
/// `<name>-checks`.
fn runner(test_box: &TestBox, remote: &Path, name: &str, machine: &str) -> PathBuf {
    let clone = test_box.clone_of(remote, name);
    test_box.git_ok(&clone, &["config", "bay.runnerName", name]);
    test_box.git_ok(
        &clone,
        &["config", "bay.producerName", &format!("{name}-checks")],
    );
    test_box.git_ok(&clone, &["config", "bay.machine", machine]);
    clone
}

/// One pass of the runner in `clone`, with `PROBE` set to `probe`.
fn pass(test_box: &TestBox, clone: &Path, probe: &str) -> Value {
    result_json(
        test_box
            .bay(clone)
            .args(["runner", "run", "--once", "--interval-s", "300"])
            .env("PROBE", probe),
    )
    .1
}

/// A change that writes `path`, in a view of `repo`, submitted and left to the runners.
/// Returns its id.
fn submitted(test_box: &TestBox, repo: &Path, path: &str) -> String {
    let (id, view) = test_box.start_and_edit(repo);
    std::fs::write(view.join(path), "fn x() {}\n").unwrap();
    let (code, queued) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    id
}

/// The `tests` records `producer` keeps on `tree` in `remote`; none when it keeps no note there.
fn records_of(test_box: &TestBox, remote: &Path, producer: &str, tree: &str) -> Vec<Value> {
    let out = test_box
        .git(test_box.path())
        .arg(format!("--git-dir={}", remote.display()))
        .args([
            "notes",
            &format!("--ref=refs/notes/bay/evidence/{producer}"),
            "show",
            tree,
        ])
        .output()
        .unwrap();
    if !out.status.success() {
        return Vec::new();
    }
    let records: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
    records
        .into_iter()
        .filter(|record| record["check"]["id"] == "tests")
        .collect()
}

/// Every `tests` record `producer` keeps in `remote`, on any tree.
fn every_record_of(test_box: &TestBox, remote: &Path, producer: &str) -> Vec<Value> {
    let listed = test_box.git_bare(
        remote,
        &[
            "notes",
            &format!("--ref=refs/notes/bay/evidence/{producer}"),
            "list",
        ],
    );
    listed
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .flat_map(|tree| records_of(test_box, remote, producer, tree))
        .collect()
}

/// The tree of main on `remote`.
fn main_tree(test_box: &TestBox, remote: &Path) -> String {
    test_box.git_bare(remote, &["rev-parse", "main^{tree}"])
}

/// Lands a first change through the runner in `clone` with `probe`, so main's tip has its record
/// of `tests`. Returns the tree of that tip: the next change's base.
fn first_landed(
    test_box: &TestBox,
    repo: &Path,
    remote: &Path,
    clone: &Path,
    probe: &str,
) -> String {
    let id = submitted(test_box, repo, "src/first.rs");
    let landed = pass(test_box, clone, probe);
    assert!(
        test_box.trunk_change_ids(remote).contains(&id),
        "the first change did not land: {landed}"
    );
    main_tree(test_box, remote)
}

#[test]
fn a_unit_that_fails_here_on_the_base_too_is_not_new_whatever_another_machine_saw() {
    let test_box = TestBox::new();
    let (repo, remote) = setup(&test_box);
    let bad = runner(&test_box, &remote, "exec-a", "bad-box");
    let good = runner(&test_box, &remote, "exec-b", "good-box");
    // GIVEN a base whose record of `tests` the runner on good-box made: `fonts` passes there
    let base = first_landed(&test_box, &repo, &remote, &good, "ok");
    let theirs = records_of(&test_box, &remote, "exec-b-checks", &base);
    assert_eq!(theirs.len(), 1, "{theirs:#?}");

    // WHEN the runner on bad-box, where `fonts` fails on every tree, checks a change on it
    let id = submitted(&test_box, &repo, "src/second.rs");
    let ran = pass(&test_box, &bad, "nofonts");

    // THEN it ran the base here, found `fonts` failing there too, and the change landed
    assert_eq!(ran["detail"]["ran"][0]["decision"], "merged", "{ran}");
    assert!(test_box.trunk_change_ids(&remote).contains(&id), "{ran}");
    let ours = records_of(&test_box, &remote, "exec-a-checks", &base);
    assert_eq!(ours.len(), 1, "one run of the base here: {ours:#?}");
    assert_eq!(ours[0]["verdict"], "product_failure", "{ours:#?}");
    // AND the change's record names the base record it was judged against, and the one it set
    // aside
    let landed = records_of(
        &test_box,
        &remote,
        "exec-a-checks",
        &main_tree(&test_box, &remote),
    );
    let baseline = &landed[0]["baseline"];
    assert_eq!(baseline["record"], ours[0]["id"], "{landed:#?}");
    assert_eq!(baseline["passed_elsewhere"], theirs[0]["id"], "{landed:#?}");
    assert_eq!(
        baseline["failures"],
        serde_json::json!(["fonts"]),
        "{landed:#?}"
    );
}

#[test]
fn a_unit_that_passes_on_the_base_here_too_still_refuses_the_change() {
    let test_box = TestBox::new();
    let (repo, remote) = setup(&test_box);
    let bad = runner(&test_box, &remote, "exec-a", "bad-box");
    let good = runner(&test_box, &remote, "exec-b", "good-box");
    // GIVEN a base whose record the runner on good-box made
    let base = first_landed(&test_box, &repo, &remote, &good, "ok");
    let theirs = records_of(&test_box, &remote, "exec-b-checks", &base);

    // WHEN the runner on bad-box checks a change that breaks `t1`
    let id = submitted(&test_box, &repo, "src/broken.rs");
    let ran = pass(&test_box, &bad, "nofonts");

    // THEN the base ran here, `t1` passed on it, and the change is refused for `t1` alone
    let row = &ran["detail"]["ran"][0];
    assert_eq!(row["reason"], "CHECK_FAILED", "{ran}");
    assert!(!test_box.trunk_change_ids(&remote).contains(&id), "{ran}");
    let ours = records_of(&test_box, &remote, "exec-a-checks", &base);
    assert_eq!(ours.len(), 1, "one run of the base here: {ours:#?}");
    assert!(row.to_string().contains("t1 broke"), "{ran}");
    assert_eq!(
        row["detail"]["checks"][0]["baseline_failures"],
        serde_json::json!(["fonts"]),
        "{ran}"
    );
    // AND the refused run's record names the base record it set aside
    let judged: Vec<Value> = every_record_of(&test_box, &remote, "exec-a-checks")
        .into_iter()
        .filter(|record| record.get("baseline").is_some())
        .collect();
    assert_eq!(judged.len(), 1, "{judged:#?}");
    assert_eq!(
        judged[0]["baseline"]["record"], ours[0]["id"],
        "{judged:#?}"
    );
    assert_eq!(
        judged[0]["baseline"]["passed_elsewhere"], theirs[0]["id"],
        "{judged:#?}"
    );
}

#[test]
fn a_base_record_from_another_machine_that_fails_the_same_units_is_reused() {
    let test_box = TestBox::new();
    let (repo, remote) = setup(&test_box);
    let bad = runner(&test_box, &remote, "exec-a", "bad-box");
    let other = runner(&test_box, &remote, "exec-c", "bad-box-2");
    // GIVEN a base whose record the runner on bad-box-2 made, where `fonts` failed too
    let base = first_landed(&test_box, &repo, &remote, &other, "nofonts");
    let theirs = records_of(&test_box, &remote, "exec-c-checks", &base);
    assert_eq!(theirs.len(), 1, "{theirs:#?}");

    // WHEN the runner on bad-box checks a change on it, `fonts` failing again
    let id = submitted(&test_box, &repo, "src/second.rs");
    let ran = pass(&test_box, &bad, "nofonts");

    // THEN it judged the change against that record, ran no base here, and the change landed
    assert!(test_box.trunk_change_ids(&remote).contains(&id), "{ran}");
    let ours = records_of(&test_box, &remote, "exec-a-checks", &base);
    assert!(ours.is_empty(), "the base ran again here: {ours:#?}");
    let landed = records_of(
        &test_box,
        &remote,
        "exec-a-checks",
        &main_tree(&test_box, &remote),
    );
    assert_eq!(
        landed[0]["baseline"]["record"], theirs[0]["id"],
        "{landed:#?}"
    );
    assert!(
        landed[0]["baseline"].get("passed_elsewhere").is_none(),
        "{landed:#?}"
    );
}
