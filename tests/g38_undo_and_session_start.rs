//! G38. Undo and session start.
//!
//! Built here: the session-start case (ruling 8a moves it into increment 1). `bay hook
//! session-start` prints the compact state of the view's change every time, also when nothing
//! changed, and the open refusal an outcome left on it, with what to do next (`DESIGN` § 11,
//! `INTERFACES` § 2); a later session of the same actor is told of its open refusals wherever
//! it starts; the hook reads the local cache only. `bay undo` is a later item.

mod support;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use support::{TestBox, check_files, passing_result, result_json};

/// Runs `bay hook session-start` as Claude Code does: the payload on stdin, from `cwd`.
fn session_start(test_box: &TestBox, cwd: &Path, session: &str) -> (Option<i32>, String) {
    session_start_as(test_box, cwd, session, None)
}

/// [`session_start`] for the actor `actor` (`BAY_ACTOR`); `None` is the tests' default actor.
fn session_start_as(
    test_box: &TestBox,
    cwd: &Path,
    session: &str,
    actor: Option<&str>,
) -> (Option<i32>, String) {
    let payload = serde_json::json!({
        "session_id": session,
        "hook_event_name": "SessionStart",
        "source": "startup",
        "cwd": cwd,
    });
    let mut process = test_box.bay_process(cwd);
    if let Some(actor) = actor {
        process.env("BAY_ACTOR", actor);
    }
    let mut child = process
        .args(["hook", "session-start"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn session_start_prints_the_changes_state_every_time() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);

    let (code, first) = session_start(&test_box, &view, "s-first");
    assert_eq!(code, Some(0), "{first}");
    assert!(first.contains(&format!("change {id}")), "{first}");
    assert!(first.contains("open"), "{first}");

    // Nothing changed: a new session still learns the state.
    let (code, again) = session_start(&test_box, &view, "s-second");
    assert_eq!(code, Some(0), "{again}");
    assert_eq!(first, again);
}

#[test]
fn session_start_reads_the_local_cache_and_fetches_nothing() {
    // DESIGN § 11: hook latency is a measured interface, and a local cache is allowed when its
    // synchronization point is shown. The hook does not fetch the remote trunk.
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let tracked = test_box.git_ok(&repo, &["rev-parse", "refs/remotes/origin/main"]);

    // Trunk moves on the remote after this repository last synchronized.
    let other = test_box.clone_of(&test_box.path().join("repo-remote.git"), "other");
    std::fs::write(other.join("src/b.rs"), "fn b() {}\n").unwrap();
    test_box.git_ok(&other, &["add", "--all"]);
    test_box.git_ok(&other, &["commit", "-q", "-m", "elsewhere"]);
    test_box.git_ok(&other, &["push", "-q", "origin", "main"]);

    let (code, out) = session_start(&test_box, &view, "s-cached");
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains(&format!("change {id}")), "{out}");
    assert!(out.contains(&format!("trunk {}", &tracked[..12])), "{out}");
    assert!(out.contains("local cache"), "{out}");
    assert_eq!(
        test_box.git_ok(&repo, &["rev-parse", "refs/remotes/origin/main"]),
        tracked,
        "session-start fetched the remote trunk"
    );
}

#[test]
fn session_start_prints_an_open_refusal_with_what_to_do_next() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &failing, 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED");

    // The session that ran land is gone; the next one in the view is told.
    let (code, out) = session_start(&test_box, &view, "s-later");
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains(&format!("change {id}")), "{out}");
    let line = out
        .lines()
        .find(|line| line.contains("refused"))
        .unwrap_or_else(|| panic!("no refusal line: {out}"));
    assert!(line.contains("CHECK_FAILED"), "{line}");
    assert!(line.contains(&format!("{id}.1")), "{line}");
    assert!(line.contains("repair"), "{line}");
    assert!(line.contains("candidate_changed"), "{line}");
}

#[test]
fn a_landed_change_has_no_open_refusal() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");

    let (code, out) = session_start(&test_box, &view, "s-after");
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains(&format!("change {id}")), "{out}");
    assert!(out.contains("landed"), "{out}");
    assert!(!out.contains("refused"), "{out}");
}

#[test]
fn a_refusal_reaches_a_later_session_of_the_same_actor_outside_the_view() {
    // INTERFACES § 2: a refusal reached after the agent left goes "to a later session of the
    // same actor at `SessionStart`", wherever that session starts.
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &failing, 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{refused}");

    let (code, out) = session_start(&test_box, &repo, "s-root");
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains(&format!("change {id}")), "{out}");
    assert!(
        out.contains(&format!("bay: refused CHECK_FAILED on {id}.1")),
        "{out}"
    );

    // Another actor's session is not told of this actor's refusal.
    let (code, out) = session_start_as(&test_box, &repo, "s-other", Some("someone-else"));
    assert_eq!(code, Some(0), "{out}");
    assert_eq!(out, "", "another actor's refusal: {out}");
}

#[test]
fn session_start_outside_a_view_prints_no_change() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    let (code, out) = session_start(&test_box, &repo, "s-root");
    assert_eq!(code, Some(0), "{out}");
    assert!(!out.contains("change "), "{out}");
}
