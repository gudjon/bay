//! G25. An operation acts only as its actor.
//!
//! A run under another actor's key is refused.

mod support;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_request_signed_by_another_actors_key_is_refused() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let trunk = test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let keys = test_box.keys();
    test_box.enroll_with(&repo, &keys, &keys.executor, &[("agent-b", &keys.rogue)]);
    test_box.use_keys(&repo, &keys);
    let (_, view) = test_box.start_and_edit(&repo);

    // Another enrolled agent tries to land this actor's change with its own key.
    test_box.git_ok(
        &repo,
        &["config", "bay.agentKey", keys.rogue.to_str().unwrap()],
    );
    let (code, refused) = result_json(
        test_box
            .bay(&view)
            .env("BAY_ACTOR", "agent-b")
            .args(["land", "--wait"]),
    );
    assert_eq!(code, 20, "{refused}");
    assert!(
        refused["detail"]
            .as_str()
            .unwrap()
            .contains("not signed by an agent key of the change's actor agent"),
        "{refused}"
    );
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), trunk);
}
