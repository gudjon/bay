//! G724, BACKLOG 237 (a fleet finding, 2026-11-14, bay 0.6.70: numtmd6o showed all six of its post
//! checks `pending` for hours, and jvfmplp5, phrphray and u52n5nc2 one each, while a run on a
//! newer tip had answered each one). A runner runs a post check on main's newest tip only, and
//! that run answers every landing since the check's previous run (G439), but a landing's status
//! row read its own tree alone, which no run ever visits once a newer landing is on main. The
//! row reads forward along main to the first landing with a record of the check: a pass there
//! is its answer, and the row names that landing. A red there names a range, never this
//! landing, so the row stays pending until a run narrowed to it says so.
//!
//! Guard to mutate: `src/publish.rs` `post_checks_on_trunk` reading no landing after its own
//! (the older landing's row stays `pending` after the newer one's pass).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::{TestBox, check_files, passing_result, result_json};

/// A trunk whose policy runs `tests` before publication and `suite` after it.
fn trunk() -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let entry = |id: &str, extra: &str| {
        format!(
            "\n[[check]]\nid = \"{id}\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check {id}\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n{extra}"
        )
    };
    let policy = format!(
        "revision = 1\n{}{}",
        entry("tests", ""),
        entry("suite", "stage = \"post\"\n")
    );
    let script = check_files("true", &passing_result(1), 1, 60)
        .into_iter()
        .find(|(path, _)| path == "scripts/bay-check")
        .unwrap()
        .1;
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[
            ("scripts/bay-check".to_owned(), script),
            (".bay/policy.toml".to_owned(), policy),
        ],
    );
    (test_box, repo, remote)
}

/// Lands a change that adds `file`: its id, commit and tree.
fn land(test_box: &TestBox, repo: &Path, card: &str, file: &str) -> (String, String, String) {
    let started = test_box.bay_ok(repo, &["start", card, "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(file), format!("// {card}\n")).unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    let commit = landed["detail"]["commit"].as_str().unwrap().to_owned();
    let remote = test_box.path().join("repo-remote.git");
    let tree = test_box.git_bare(&remote, &["rev-parse", &format!("{commit}^{{tree}}")]);
    (id, commit, tree)
}

/// A runner's record of `suite` with `verdict` on `tree`, made from the landing's own record
/// of `tests` there and pushed under producer `tip`.
fn record_suite(test_box: &TestBox, runner: &Path, tree: &str, verdict: &str) {
    let notes = test_box.git_ok(
        runner,
        &["notes", "--ref=refs/notes/bay/evidence/local", "show", tree],
    );
    let records: Vec<Value> = serde_json::from_str(&notes).unwrap();
    let mut record = records[0].clone();
    record["id"] = json!(format!("tip/{tree}"));
    record["producer"]["name"] = json!("tip");
    record["check"]["id"] = json!("suite");
    record["verdict"] = json!(verdict);
    let text = serde_json::to_string(&json!([record])).unwrap();
    test_box.git_ok(
        runner,
        &[
            "notes",
            "--ref=refs/notes/bay/evidence/tip",
            "add",
            "-f",
            "-m",
            &text,
            tree,
        ],
    );
    test_box.git_ok(
        runner,
        &[
            "push",
            "-q",
            "origin",
            "refs/notes/bay/evidence/tip:refs/notes/bay/evidence/tip",
        ],
    );
}

/// A clone that reads the remote's evidence, as a runner's does.
fn runner_of(test_box: &TestBox, remote: &Path) -> PathBuf {
    let runner = test_box.clone_of(remote, "runner");
    test_box.git_ok(
        &runner,
        &[
            "fetch",
            "-q",
            "origin",
            "refs/notes/bay/evidence/local:refs/notes/bay/evidence/local",
        ],
    );
    runner
}

fn post(test_box: &TestBox, repo: &Path, id: &str) -> Value {
    let status = test_box.bay_ok(repo, &["status", id]);
    status["detail"]["changes"][0]["post_checks"].clone()
}

#[test]
fn a_pass_on_a_newer_landing_answers_the_post_check_of_an_older_one() {
    let (test_box, repo, remote) = trunk();
    let (older, _, _) = land(&test_box, &repo, "CARD-237-A", "src/older.rs");
    let (newer, newer_commit, newer_tree) = land(&test_box, &repo, "CARD-237-B", "src/newer.rs");
    let pending = json!([{"check": "suite", "environment": "local", "state": "pending"}]);
    assert_eq!(post(&test_box, &repo, &older), pending);

    // WHEN a runner's run on the newer tip passes, as G439 runs it: once, on main's newest tip
    let runner = runner_of(&test_box, &remote);
    record_suite(&test_box, &runner, &newer_tree, "pass");

    // THEN the newer landing passes on its own record
    assert_eq!(
        post(&test_box, &repo, &newer),
        json!([{"check": "suite", "environment": "local", "state": "pass"}])
    );
    // AND the older one passes on that run, and names the landing that answered it.
    assert_eq!(
        post(&test_box, &repo, &older),
        json!([{"check": "suite", "environment": "local", "state": "pass",
            "answered_on": newer_commit}])
    );
}

#[test]
fn a_red_on_a_newer_landing_leaves_an_older_landing_pending_until_its_own_run() {
    let (test_box, repo, remote) = trunk();
    let (older, _, older_tree) = land(&test_box, &repo, "CARD-237-C", "src/older.rs");
    let (_, _, newer_tree) = land(&test_box, &repo, "CARD-237-D", "src/newer.rs");
    let runner = runner_of(&test_box, &remote);

    // WHEN the run on the newer tip fails
    record_suite(&test_box, &runner, &newer_tree, "product_failure");

    // THEN the older landing is not charged with it: a red names a range, never one landing
    let pending = json!([{"check": "suite", "environment": "local", "state": "pending"}]);
    assert_eq!(post(&test_box, &repo, &older), pending);

    // AND a run narrowed to the older landing decides it.
    record_suite(&test_box, &runner, &older_tree, "pass");
    assert_eq!(
        post(&test_box, &repo, &older),
        json!([{"check": "suite", "environment": "local", "state": "pass"}])
    );
}
