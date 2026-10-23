//! G38. Undo and session start.
//!
//! `bay undo <change>` (`DESIGN` § 4, `INTERFACES` § 1, ruling R10) opens a new change on the
//! intent `undo of <Change-Id>` whose tree reverts the landed commit onto current trunk, and lands
//! it through the normal publication: trunk's first-parent history before it is unchanged, the
//! publication carries `Reverts: <Change-Id>`, and the undone intent's claims are untouched. An
//! unlanded change is `CHANGE_NOT_MERGED`; an undo can itself be undone; a second undo of one
//! change claims generation 2 of its undo intent, and only after the first undo landed; a revert
//! that does not merge is `MERGE_CONFLICT` with the view left open for repair. A remote that
//! cannot be read is a failure, never `CHANGE_NOT_MERGED`; an undo whose effect is already gone
//! from trunk opens no change and says there is nothing to undo.
//!
//! The session-start case (ruling 8a moves it into increment 1): `bay hook session.start` prints
//! the compact state of the view's change every time, also when nothing changed, and the open
//! refusal an outcome left on it, with what to do next (`DESIGN` § 11, `INTERFACES` § 2); a later
//! session of the same actor is told of its open refusals wherever it starts; the hook reads the
//! local cache only.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use support::{TestBox, check_files, passing_result, result_json};

/// Runs `bay hook session.start` as Claude Code does: the payload on stdin, from `cwd`.
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
        .args(["hook", "session.start"])
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
    assert!(out.contains(&format!("main {}", &tracked[..12])), "{out}");
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
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
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
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");

    let (code, out) = session_start(&test_box, &view, "s-after");
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains(&format!("change {id}")), "{out}");
    assert!(out.contains("merged"), "{out}");
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
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
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

/// A trunk with a passing check, and one landed change that adds `src/new.rs`.
fn landed_change() -> (TestBox, PathBuf, PathBuf, String) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    (test_box, repo, remote, id)
}

fn undo(test_box: &TestBox, repo: &Path, id: &str) -> (i32, serde_json::Value) {
    result_json(test_box.bay(repo).args(["undo", id, "--wait"]))
}

/// The remote trunk's tip trailer `key`.
fn tip_trailer(test_box: &TestBox, remote: &Path, key: &str) -> String {
    test_box
        .git_bare(
            remote,
            &[
                "log",
                "-1",
                &format!("--format=%(trailers:key={key},valueonly)"),
                "main",
            ],
        )
        .trim()
        .to_owned()
}

fn has_file(test_box: &TestBox, remote: &Path, path: &str) -> bool {
    !test_box
        .git_bare(remote, &["ls-tree", "--name-only", "main", "--", path])
        .is_empty()
}

fn refs(test_box: &TestBox, remote: &Path, prefix: &str) -> Vec<String> {
    test_box
        .git_bare(
            remote,
            &["for-each-ref", "--format=%(objectname) %(refname)", prefix],
        )
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn undo_lands_a_compensating_change_and_keeps_history() {
    let (test_box, repo, remote, id) = landed_change();
    let history = test_box.git_bare(&remote, &["rev-list", "--first-parent", "main"]);
    let claims = refs(&test_box, &remote, "refs/bay/intent/");
    let landed_intents = refs(&test_box, &remote, "refs/bay/landed-intent/");
    assert!(has_file(&test_box, &remote, "src/new.rs"));

    let (code, undone) = undo(&test_box, &repo, &id);
    assert_eq!(code, 0, "{undone}");
    assert_eq!(undone["decision"], "merged", "{undone}");
    let undo_id = undone["change"].as_str().unwrap();
    assert_ne!(undo_id, id);

    // A new publication on top; the history before it is unchanged (never a rewrite).
    let after = test_box.git_bare(&remote, &["rev-list", "--first-parent", "main"]);
    let (tip, rest) = after.split_once('\n').unwrap();
    assert_eq!(rest, history);
    assert_eq!(test_box.trunk_change_ids(&remote)[0], undo_id);
    assert_eq!(tip_trailer(&test_box, &remote, "Reverts"), id);
    let subject = test_box.git_bare(&remote, &["log", "-1", "--format=%s", tip]);
    assert!(subject.starts_with(&format!("undo of {id}")), "{subject}");
    assert!(!has_file(&test_box, &remote, "src/new.rs"));
    assert!(has_file(&test_box, &remote, "src/a.rs"));

    // The undone intent's claims are untouched; the undo claims its own intent.
    let claims_after = refs(&test_box, &remote, "refs/bay/intent/");
    assert!(claims.iter().all(|claim| claims_after.contains(claim)));
    assert_eq!(claims_after.len(), claims.len() + 1, "{claims_after:?}");
    let landed_after = refs(&test_box, &remote, "refs/bay/landed-intent/");
    assert!(
        landed_intents
            .iter()
            .all(|claim| landed_after.contains(claim))
    );
}

#[test]
fn undo_of_an_unlanded_change_is_change_not_landed() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, _view) = test_box.start_and_edit(&repo);
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let (code, refused) = undo(&test_box, &repo, &id);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHANGE_NOT_MERGED", "{refused}");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), tip);
}

#[test]
fn an_undo_can_be_undone_and_a_second_undo_claims_generation_two() {
    let (test_box, repo, remote, id) = landed_change();

    // The first undo, not yet landed: a second undo of the same change waits for it.
    let (code, queued) = result_json(test_box.bay(&repo).args(["undo", &id]));
    assert_eq!(code, 0, "{queued}");
    let first = queued["change"].as_str().unwrap().to_owned();
    let (code, early) = undo(&test_box, &repo, &id);
    assert_eq!(code, 10, "{early}");
    assert_eq!(early["reason"], "CHANGE_SUPERSEDED", "{early}");
    let (code, landed) = result_json(test_box.bay(&repo).args(["merge", "--wait", &first]));
    assert_eq!(code, 0, "{landed}");
    assert!(!has_file(&test_box, &remote, "src/new.rs"));

    // Undo of the undo brings the file back.
    let (code, redone) = undo(&test_box, &repo, &first);
    assert_eq!(code, 0, "{redone}");
    assert_eq!(tip_trailer(&test_box, &remote, "Reverts"), first);
    assert!(has_file(&test_box, &remote, "src/new.rs"));

    // A second undo of the change claims generation 2 of its undo intent and lands once.
    let (code, second) = undo(&test_box, &repo, &id);
    assert_eq!(code, 0, "{second}");
    let second_id = second["change"].as_str().unwrap();
    assert_ne!(second_id, first);
    assert_eq!(tip_trailer(&test_box, &remote, "Reverts"), id);
    assert_eq!(tip_trailer(&test_box, &remote, "Supersedes"), first);
    assert!(!has_file(&test_box, &remote, "src/new.rs"));
    let ids = test_box.trunk_change_ids(&remote);
    assert_eq!(ids.iter().filter(|landed| *landed == second_id).count(), 1);
    let generations: Vec<String> = refs(&test_box, &remote, "refs/bay/landed-intent/")
        .into_iter()
        .filter(|name| name.ends_with("/2"))
        .collect();
    assert_eq!(generations.len(), 1, "{generations:?}");
}

#[test]
fn an_undo_that_does_not_merge_is_a_merge_conflict_with_the_view_open() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let land_edit = |intent: &str, text: &str| {
        let started = test_box.bay_ok(&repo, &["start", intent, "--paths", "src/"]);
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join("src/a.rs"), text).unwrap();
        let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
        assert_eq!(code, 0, "{landed}");
        started["change"].as_str().unwrap().to_owned()
    };
    let id = land_edit("CARD-1", "fn a() { 1 }\n");
    land_edit("CARD-2", "fn a() { 2 }\n");
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);

    let (code, refused) = undo(&test_box, &repo, &id);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "MERGE_CONFLICT", "{refused}");
    assert_eq!(refused["detail"]["conflicts"][0], "src/a.rs", "{refused}");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), tip);
    let undo_id = refused["change"].as_str().unwrap();
    assert_ne!(undo_id, id);

    // The view is open for repair; the repaired undo lands with its trailer.
    let view = PathBuf::from(refused["detail"]["view"].as_str().unwrap());
    let text = std::fs::read_to_string(view.join("src/a.rs")).unwrap();
    assert!(text.contains("<<<<<<<"), "{text}");
    std::fs::write(view.join("src/a.rs"), "fn a() {}\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(tip_trailer(&test_box, &remote, "Reverts"), id);
}

/// Puts a `git` on the box's PATH that fails, as an unreachable remote does, every fetch or
/// listing of a landed marker; every other call runs the real git.
fn fail_marker_reads(test_box: &TestBox) {
    let out = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    support::write_executable(
        &test_box.path().join("fakebin/git"),
        &format!(
            "#!/bin/sh\ncase \"$*\" in\n  *fetch*refs/bay/landed/*|*ls-remote*refs/bay/landed/*)\n    echo 'fatal: unable to access the remote' >&2; exit 128;;\nesac\nexec \"{real}\" \"$@\"\n"
        ),
    );
}

#[test]
fn an_unreadable_remote_is_never_change_not_landed() {
    // CHANGE_NOT_MERGED retries never: an agent told so gives up on a real landing.
    let (test_box, repo, remote, id) = landed_change();
    fail_marker_reads(&test_box);
    let (code, result) = undo(&test_box, &repo, &id);
    assert_eq!(code, 20, "{result}");
    assert_eq!(result["decision"], "failed", "{result}");
    assert_ne!(result["reason"], "CHANGE_NOT_MERGED", "{result}");

    // A remote gone altogether is reported the same way.
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let missing = test_box.path().join("missing.git");
    test_box.git_ok(
        &repo,
        &["remote", "set-url", "origin", missing.to_str().unwrap()],
    );
    let (code, result) = undo(&test_box, &repo, &id);
    assert_eq!(code, 20, "{result}");
    assert_ne!(result["reason"], "CHANGE_NOT_MERGED", "{result}");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), tip);
}

#[test]
fn an_undo_whose_effect_is_already_gone_opens_no_change() {
    let (test_box, repo, remote, id) = landed_change();
    // A manual revert of the change, landed through bay.
    let started = test_box.bay_ok(&repo, &["start", "CARD-revert", "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::remove_file(view.join("src/new.rs")).unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let changes = refs(&test_box, &remote, "refs/bay/changes/");
    let claims = refs(&test_box, &remote, "refs/bay/intent/");

    let (code, result) = undo(&test_box, &repo, &id);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["decision"], "ok", "{result}");
    assert_eq!(result["detail"]["undo"], "nothing to undo", "{result}");
    assert_eq!(result["detail"]["reverts"], id.as_str(), "{result}");
    assert_eq!(result["detail"]["main"], tip.as_str(), "{result}");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), tip);
    assert_eq!(refs(&test_box, &remote, "refs/bay/changes/"), changes);
    assert_eq!(refs(&test_box, &remote, "refs/bay/intent/"), claims);
}
