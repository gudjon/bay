//! G22. Finished check units survive a dead worker.
//!
//! The worker dies after five of six units: five receipts stay valid, the sixth is
//! `unknown`, the aggregate is incomplete and publication stays blocked; resume runs only
//! the sixth.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

mod support;

use support::{TestBox, passing_result, result_json};

#[test]
fn a_worker_that_dies_after_five_of_six_units_resumes_with_the_sixth() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let runs = test_box.path().join("runs");
    // One adapter, six units: `scripts/bay-check <unit>` logs the unit it ran.
    let base = support::check_files(
        &format!("echo \"$1\" >> '{}'", runs.display()),
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
    let files = vec![
        ("scripts/bay-check".to_owned(), script),
        (".bay/policy.toml".to_owned(), policy),
    ];
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0);

    let executor = test_box.clone_of(&remote, "executor");
    let died = test_box
        .bay(&executor)
        .env("BAY_TEST_KILL_AT", "after-check-5")
        .args(["executor", "run", "--once"])
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
    let resumed = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(
        resumed["detail"]["ran"][0]["decision"], "landed",
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
