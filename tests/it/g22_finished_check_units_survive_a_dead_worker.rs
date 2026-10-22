//! G22. Finished check units survive a dead worker.
//!
//! The worker dies after five of six units: five receipts stay valid, the sixth is
//! `unknown`, the aggregate is incomplete and publication stays blocked; resume runs only
//! the sixth.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;

use bay::evidence::{self, CheckId, Combined};
use bay::git::Git;
use support::{EXECUTOR, PRODUCER, TestBox, passing_result, result_json};

/// The policy and adapter of six required units: one adapter, `scripts/bay-check <unit>`,
/// which logs the unit it ran to `runs` and passes.
fn six_units(test_box: &TestBox, runs: &Path) -> Vec<(String, String)> {
    let base = support::check_files(
        &test_box.relay(&format!("echo \"$1\" >> '{}'", runs.display())),
        &passing_result(1),
        1,
        60,
    );
    let script = base
        .iter()
        .find(|(path, _)| path == "scripts/bay-check")
        .unwrap()
        .1
        .clone();
    let mut policy = "revision = 1\n".to_owned();
    for unit in 1..=6 {
        use std::fmt::Write;
        let _ = write!(
            policy,
            "\n[[check]]\nid = \"unit-{unit}\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check unit-{unit}\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n"
        );
    }
    vec![
        ("scripts/bay-check".to_owned(), script),
        (".bay/policy.toml".to_owned(), policy),
    ]
}

#[test]
fn a_worker_that_dies_after_five_of_six_units_resumes_with_the_sixth() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let runs = test_box.path().join("runs");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &six_units(&test_box, &runs),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0);

    let executor = test_box.clone_of(&remote, "executor");
    let died = test_box
        .bay(&executor)
        .env("BAY_TEST_KILL_AT", "after-check-5")
        .args(["runner", "run", "--once"])
        .output()
        .unwrap();
    assert_eq!(died.status.code(), Some(137));
    let ran: Vec<String> = std::fs::read_to_string(&runs)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(ran, ["unit-1", "unit-2", "unit-3", "unit-4", "unit-5"]);
    // Five receipts are kept on the candidate tree; nothing is published.
    let record = test_box.git_ok(
        &executor,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    assert!(!record.contains("\"published\""), "{record}");
    assert_eq!(test_box.trunk_change_ids(&remote), Vec::<String>::new());
    let notes = test_box.git_ok(
        &executor,
        &["notes", "--ref=refs/notes/bay/evidence/local", "list"],
    );
    let tree = notes.split_whitespace().nth(1).unwrap().to_owned();
    let kept = test_box.git_ok(
        &executor,
        &[
            "notes",
            "--ref=refs/notes/bay/evidence/local",
            "show",
            &tree,
        ],
    );
    let kept: Vec<serde_json::Value> = serde_json::from_str(&kept).unwrap();
    assert_eq!(kept.len(), 5);
    assert!(kept.iter().all(|record| record["verdict"] == "pass"));
    assert!(kept.iter().all(|record| record["check"]["id"] != "unit-6"));

    // Resume runs only the sixth unit, and then the change publishes.
    let resumed = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    assert_eq!(
        resumed["detail"]["ran"][0]["decision"], "merged",
        "{resumed}"
    );
    let ran: Vec<String> = std::fs::read_to_string(&runs)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(ran.len(), 6, "{ran:?}");
    assert_eq!(ran[5], "unit-6");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}

/// The worker dies after five of six units on a bay-hosted trunk. Judged over the five kept
/// producer-signed receipts under the frozen manifest, the aggregate is incomplete and names
/// one gap only: the sixth unit, `unknown`. The trunk hook judges a publication of that
/// candidate the same way and refuses it, naming the sixth unit, so publication stays
/// blocked. A resume then runs only the sixth and lands.
#[test]
fn the_sixth_unit_is_unknown_and_the_aggregate_incomplete() {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    let runs = test_box.path().join("runs");
    let trunk = test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &six_units(&test_box, &runs),
    );
    let keys = test_box.keys();
    let registry = test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, submitted) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");

    let executor = test_box.clone_of(&host, "executor");
    test_box.use_keys(&executor, &keys);
    let died = test_box
        .bay(&executor)
        .env("BAY_TEST_KILL_AT", "after-check-5")
        .args(["runner", "run", "--once"])
        .output()
        .unwrap();
    assert_eq!(died.status.code(), Some(137), "{died:?}");

    // The receipts the dead worker kept, on its candidate tree.
    let notes_ref = format!("refs/notes/bay/evidence/{PRODUCER}");
    let notes = test_box.git_ok(&executor, &["notes", "--ref", &notes_ref, "list"]);
    let tree = notes.split_whitespace().nth(1).unwrap().to_owned();
    let git = Git::new(&executor);
    let current = bay::registry::Registry::current(&git).unwrap();
    assert!(current.is_some(), "the executor reads the registry");
    let records = evidence::counted(
        &git,
        evidence::records_for(&git, &tree).unwrap(),
        current.as_ref(),
    )
    .unwrap();
    assert_eq!(records.len(), 5, "five producer-signed receipts count");
    let policy = bay::policy::load_at(&git, &trunk).unwrap().unwrap();
    let gaps = evidence::aggregate(
        &records,
        &policy.manifest,
        &policy.manifest_digest,
        &|check| policy.commit_keyed(check),
        "",
    );
    assert_eq!(
        gaps,
        vec![(CheckId::new("unit-6", "local"), Combined::Unknown)],
        "the aggregate is incomplete, and only the sixth unit is unknown"
    );

    // A publication of that candidate, with everything the hook asks for but the sixth
    // unit's evidence: the receipts on the host, executor-signed, its landed marker in the
    // same push.
    test_box.git_ok(&executor, &["push", "-q", "origin", &notes_ref]);
    let (request, revision) = latest_request(&test_box, &executor, &id);
    let publication = executor_commit(
        &test_box,
        &executor,
        &keys.executor,
        &tree,
        &trunk,
        &format!(
            "publish\n\nChange-Id: {id}\nRevision-Id: {revision}\nRequest-Id: {request}\nRegistry-Id: {registry}\n"
        ),
    );
    let out = test_box
        .git(&executor)
        .args([
            "push",
            "--atomic",
            "origin",
            &format!("{publication}:refs/heads/main"),
            &format!("{publication}:refs/bay/landed/{id}"),
        ])
        .output()
        .unwrap();
    let why = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "the publication was accepted: {why}");
    assert!(
        why.contains("no passing producer-signed evidence for unit-6"),
        "{why}"
    );
    assert!(!why.contains("unit-5"), "a finished unit is a gap: {why}");
    assert_eq!(test_box.git_bare(&host, &["rev-parse", "main"]), trunk);

    // Resume runs only the sixth unit, and then the change publishes.
    let resumed = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    assert_eq!(
        resumed["detail"]["ran"][0]["decision"], "merged",
        "{resumed}"
    );
    let ran: Vec<String> = std::fs::read_to_string(&runs)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        ran,
        ["unit-1", "unit-2", "unit-3", "unit-4", "unit-5", "unit-6"]
    );
    assert_eq!(test_box.trunk_change_ids(&host), vec![id]);
}

/// The latest request of change `id` on `repo`'s origin, fetched: (request commit, revision).
fn latest_request(test_box: &TestBox, repo: &Path, id: &str) -> (String, String) {
    let name = format!("refs/bay/requests/{id}");
    test_box.git_ok(repo, &["fetch", "-q", "origin", &format!("+{name}:{name}")]);
    let request = test_box.git_ok(repo, &["rev-parse", &name]);
    let text = test_box.git_ok(
        repo,
        &["cat-file", "blob", &format!("{request}:request.json")],
    );
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let revision = json["revision"].as_str().unwrap().to_owned();
    (request, revision)
}

/// A commit of `tree` on `parent`, signed with `key` and committed by the executor.
fn executor_commit(
    test_box: &TestBox,
    repo: &Path,
    key: &Path,
    tree: &str,
    parent: &str,
    message: &str,
) -> String {
    let signing = format!("user.signingkey={}", key.display());
    let user = format!("user.name={EXECUTOR}");
    let out = test_box
        .git(repo)
        .args([
            "-c",
            "gpg.format=ssh",
            "-c",
            signing.as_str(),
            "-c",
            user.as_str(),
            "-c",
            "user.email=x@bay.invalid",
            "commit-tree",
            "-S",
            tree,
            "-p",
            parent,
            "-m",
            message,
        ])
        .env("GIT_AUTHOR_NAME", EXECUTOR)
        .env("GIT_COMMITTER_NAME", EXECUTOR)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}
