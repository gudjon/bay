//! G25. An operation acts only as its actor.
//!
//! A run under another actor's key is refused. A landing names its actor in the actor
//! trailer and its session in `Bay-Session`: one actor, split across two trailers.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};

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

/// Lands one change as actor `agent` in session `s-trailers-1` under `config`. Returns the
/// box, the remote, and the message of the trunk commit it published.
fn land_as_one_session(config: Option<&str>) -> (TestBox, PathBuf, String) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    if let Some(config) = config {
        files.push((".bay/config.toml".to_owned(), config.to_owned()));
    }
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (code, started) = result_json(
        test_box
            .bay(&repo)
            .env("BAY_ACTOR", "agent")
            .env("BAY_SESSION", "s-trailers-1")
            .args(["start", "CARD-1", "--paths", "src/"]),
    );
    assert_eq!(code, 0, "{started}");
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    let (code, landed) = result_json(
        test_box
            .bay(&view)
            .env("BAY_ACTOR", "agent")
            .env("BAY_SESSION", "s-trailers-1")
            .args(["land", "--wait"]),
    );
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "landed", "{landed}");
    let message = test_box.git_bare(&remote, &["log", "-1", "--format=%B", "main"]);
    (test_box, remote, message)
}

/// The value of the `key` trailer on the remote's trunk tip.
fn trailer(test_box: &TestBox, remote: &Path, key: &str) -> String {
    test_box.git_bare(
        remote,
        &[
            "log",
            "-1",
            &format!("--format=%(trailers:key={key},valueonly)"),
            "main",
        ],
    )
}

#[test]
fn the_actor_trailer_names_the_actor_and_bay_session_names_its_session() {
    let (test_box, remote, message) = land_as_one_session(None);
    assert_eq!(trailer(&test_box, &remote, "Actor"), "agent", "{message}");
    assert_eq!(
        trailer(&test_box, &remote, "Bay-Session"),
        "s-trailers-1",
        "{message}"
    );
    assert!(
        message.lines().any(|line| line == "Actor: agent"),
        "{message}"
    );
    assert!(
        message
            .lines()
            .any(|line| line == "Bay-Session: s-trailers-1"),
        "{message}"
    );
}

#[test]
fn a_configured_actor_key_carries_the_bare_name_and_the_session_key_stays_fixed() {
    let (test_box, remote, message) = land_as_one_session(Some("[trailers]\nactor = \"Who\"\n"));
    assert_eq!(trailer(&test_box, &remote, "Who"), "agent", "{message}");
    assert_eq!(trailer(&test_box, &remote, "Actor"), "", "{message}");
    assert_eq!(
        trailer(&test_box, &remote, "Bay-Session"),
        "s-trailers-1",
        "{message}"
    );
    assert!(!message.contains("agent/"), "{message}");
}
