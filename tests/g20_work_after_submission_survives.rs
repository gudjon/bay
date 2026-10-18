//! G20. Work after submission survives.
//!
//! The agent edits after `land`; the candidate publishes; cleanup leaves the newer work intact.

mod support;

use support::{TestBox, check_files, passing_result, result_json};

fn leftover<'a>(status: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    status["detail"]["leftovers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|leftover| leftover["change"] == id)
        .unwrap()
}

#[test]
fn edits_made_after_land_survive_the_publication_and_cleanup() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0);

    // The agent keeps working after submitting.
    std::fs::write(view.join("src/later.rs"), "fn later() {}\n").unwrap();
    std::fs::write(view.join("src/new.rs"), "fn new() { changed later }\n").unwrap();

    let executor = test_box.clone_of(&remote, "executor");
    test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    // What published is the submitted revision, not the later edits.
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:src/new.rs"]),
        "fn new() {}"
    );
    let later = test_box
        .git(test_box.path())
        .args([
            &format!("--git-dir={}", remote.display()),
            "cat-file",
            "-e",
            "main:src/later.rs",
        ])
        .status()
        .unwrap();
    assert!(!later.success());

    // Cleanup keeps the newer work: files intact, view not releasable.
    let status = test_box.bay_ok(&repo, &["status"]);
    let kept = leftover(&status, &id);
    assert_eq!(kept["state"], "landed");
    assert_eq!(kept["newer_work"], true, "{status}");
    assert_eq!(kept["journal"], "published");
    assert_eq!(
        std::fs::read_to_string(view.join("src/later.rs")).unwrap(),
        "fn later() {}\n"
    );
    assert_eq!(
        std::fs::read_to_string(view.join("src/new.rs")).unwrap(),
        "fn new() { changed later }\n"
    );

    // More work on a landed change is a new change.
    let (code, refused) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHANGE_ALREADY_LANDED");
}

#[test]
fn a_clean_view_becomes_releasable_after_its_landing() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0);
    let status = test_box.bay_ok(&repo, &["status"]);
    let clean = leftover(&status, &id);
    assert_eq!(clean["newer_work"], false, "{status}");
    assert_eq!(clean["journal"], "releasable");
}
