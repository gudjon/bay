//! G729, BACKLOG 241 (a talk report, 2026-11-15). A delivery talk also closes as `done` once its
//! recipient's latest backbrief says `done` or `validated` and its `evidence` names a merge on
//! trunk: a commit a registered runner published.
//!
//! G148 closes a delivery talk about change X only on a merge whose `Fixes:`, `Reverts:` or
//! `Supersedes:` names X. Only the recipient knows the delivery happened, and the change that
//! delivered it may not carry that trailer, because nothing told it a talk was waiting: it named
//! X in its body, and the talk stayed owed by its recipient. Now the recipient closes it the
//! way a reliability talk closes (BACKLOG 240), with one difference: the evidence must be a
//! merge, never any commit on trunk. A trunk commit that is no merge closes nothing, also one
//! that carries a `Change-Id:` trailer. The host's judge takes the close only when the evidence
//! names a change whose landed marker is that commit.
//!
//! The guards to mutate: in `src/talk/accept.rs` `close_evidenced`, the delivery kind and the
//! merge test; in `src/talk/judge.rs` `evidenced`, the delivery kind and the merge test.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use serde_json::{Value, json};
use support::result_json;
use support::talk::{TalkCase, from_now, hosted_talk_landing, talk_ref};

/// Starts a change on `src/` as `agent`, writes `file`, and merges it. Returns (id, its merge).
fn merge(case: &TalkCase, card: &str, file: &str, text: &str) -> (String, String) {
    let (code, started) = result_json(
        case.bay_as("agent")
            .args(["start", card, "--paths", "src/"]),
    );
    assert_eq!(code, 0, "{started}");
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(file), text).unwrap();
    let (code, merged) = result_json(
        case.test_box
            .bay(&view)
            .env("BAY_ACTOR", "agent")
            .env("BAY_SESSION", "s-agent")
            .args(["merge", "--wait"]),
    );
    assert_eq!(code, 0, "{merged}");
    (id, trunk_tip(case))
}

fn trunk_tip(case: &TalkCase) -> String {
    case.host_tip("refs/heads/main").unwrap()
}

/// A delivery talk from `agent-c` to `agent` about `change`. Returns its id.
fn ask_about(case: &TalkCase, change: &str, what: &str) -> String {
    let follow_up = from_now(3600);
    let (code, sent) = case.talk(
        "agent-c",
        &[
            "agent",
            what,
            "--kind",
            "delivery",
            "--change",
            change,
            "--follow-up",
            &follow_up,
        ],
    );
    assert_eq!(code, 0, "{sent}");
    sent["detail"]["talk"].as_str().unwrap().to_owned()
}

fn backbrief(case: &TalkCase, id: &str, more: &[&str]) {
    let mut args = vec!["--backbrief", id];
    args.extend(more);
    let (code, sent) = case.talk("agent", &args);
    assert_eq!(code, 0, "{sent}");
    assert_eq!(sent["state"], "backbriefed", "{sent}");
}

/// What the accept of `agent`'s next `bay status` closed of talk `id`.
fn closed_now(case: &TalkCase, id: &str) -> Vec<Value> {
    let (code, status) = case.status("agent");
    assert_eq!(code, 0, "{status}");
    status["detail"]["talk_accept"]["closed"]
        .as_array()
        .unwrap_or_else(|| panic!("no talk_accept.closed: {status}"))
        .iter()
        .filter(|closed| closed["talk"] == id)
        .cloned()
        .collect()
}

fn standing(case: &TalkCase, id: &str) -> Value {
    let (code, shown) = case.talk("agent", &["--show", id]);
    assert_eq!(code, 0, "{shown}");
    shown["detail"]["standing"].clone()
}

/// A commit on the host's trunk that no runner published: written straight into the bare
/// repository, past the host's judge, with a `Change-Id:` trailer that names `change`.
fn unpublished_on_trunk(case: &TalkCase, change: &str) -> String {
    let tip = trunk_tip(case);
    let message = format!("names a change\n\nChange-Id: {change}\n");
    let commit = case
        .test_box
        .git_bare(
            &case.host,
            &[
                "commit-tree",
                &format!("{tip}^{{tree}}"),
                "-p",
                &tip,
                "-m",
                &message,
            ],
        )
        .trim()
        .to_owned();
    case.test_box.git_bare(
        &case.host,
        &["update-ref", "refs/heads/main", &commit, &tip],
    );
    commit
}

fn entry(case: &TalkCase, rev: &str, file: &str) -> Value {
    serde_json::from_str(
        &case
            .test_box
            .git_bare(&case.host, &["show", &format!("{rev}:{file}")]),
    )
    .unwrap()
}

#[test]
fn a_delivery_talk_closes_on_a_done_backbrief_whose_evidence_is_a_merge_on_trunk() {
    let case = hosted_talk_landing();
    let before = trunk_tip(&case);
    let (x, _) = merge(&case, "CARD-x", "src/x.rs", "fn x() {}\n");
    let id = ask_about(&case, &x, "Fix the crash in x");
    let name = talk_ref("agent", &id);
    assert_eq!(closed_now(&case, &id), Vec::<Value>::new(), "acked only");

    // The change that delivers names no change in a trailer, so G148 closes nothing.
    let (y, delivered) = merge(&case, "CARD-y", "src/x.rs", "fn x() { 1 }\n");
    assert_eq!(closed_now(&case, &id), Vec::<Value>::new());

    // `done` with a trunk commit that is no merge closes nothing: the talk stays owed.
    backbrief(&case, &id, &["done", "--evidence", &before]);
    assert_eq!(closed_now(&case, &id), Vec::<Value>::new());
    assert_ne!(standing(&case, &id), "closed");

    // Nor does a trunk commit that names a change but that no runner published.
    let unpublished = unpublished_on_trunk(&case, &y);
    backbrief(&case, &id, &["done", "--evidence", &unpublished]);
    assert_eq!(closed_now(&case, &id), Vec::<Value>::new());
    assert_ne!(standing(&case, &id), "closed");

    // `done` with the merge as evidence closes the talk, naming the merge in full.
    backbrief(&case, &id, &["done", "--evidence", &delivered[..11]]);
    let closed = closed_now(&case, &id);
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(closed[0]["state"], "done", "{closed:?}");
    assert_eq!(closed[0]["evidence"], delivered.as_str(), "{closed:?}");
    let tip = case.host_tip(&name).unwrap();
    let close = entry(&case, &tip, "entries/0005-close.json");
    assert_eq!(close["state"], "done", "{close}");
    assert_eq!(close["evidence"], delivered.as_str(), "{close}");
    assert_eq!(close["actor"]["name"], "agent", "{close}");
    assert_eq!(standing(&case, &id), "closed");
}

/// A `done` close the recipient writes by hand, past its bay.
fn forged_close(case: &TalkCase, talk: &str, evidence: &str) -> String {
    let name = talk_ref("agent", talk);
    let tip = case.local_tip(&case.a, &name).unwrap();
    let close = json!({"schema": "bay.talk/1", "entry": "close", "talk": talk,
        "actor": {"name": "agent", "session": "s-agent"}, "state": "done",
        "evidence": evidence, "at": bay::clock::rfc3339_of(bay::live::unix_now())});
    let git = bay::git::Git::new(&case.a);
    let commit = bay::talk::entry_commit(&git, &tip, &close, &case.keys.agent, "agent").unwrap();
    format!("{commit}:{name}")
}

#[test]
fn the_host_takes_a_delivery_done_close_only_on_a_merge() {
    let case = hosted_talk_landing();
    let before = trunk_tip(&case);
    let (x, _) = merge(&case, "CARD-x", "src/x.rs", "fn x() {}\n");
    let id = ask_about(&case, &x, "Fix the crash in x");
    let (code, status) = case.status("agent");
    assert_eq!(code, 0, "{status}");
    let (y, delivered) = merge(&case, "CARD-y", "src/x.rs", "fn x() { 1 }\n");

    // A trunk commit that is no merge.
    backbrief(&case, &id, &["done", "--evidence", &before]);
    let stderr = case.refused(&case.a, &forged_close(&case, &id, &before));
    assert!(stderr.contains("is not a merge"), "{stderr}");

    // A trunk commit that names a change but is not that change's merge.
    let unpublished = unpublished_on_trunk(&case, &y);
    backbrief(&case, &id, &["done", "--evidence", &unpublished]);
    let stderr = case.refused(&case.a, &forged_close(&case, &id, &unpublished));
    assert!(stderr.contains("is not a merge"), "{stderr}");

    // The merge is taken.
    backbrief(&case, &id, &["done", "--evidence", &delivered[..11]]);
    let out = case
        .test_box
        .git(&case.a)
        .args(["push", "origin", &forged_close(&case, &id, &delivered)])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
