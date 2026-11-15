//! G728, BACKLOG 240 (v12-bay-core, 2026-11-15). A reliability talk closes as `done` once its
//! recipient's latest backbrief says `done` or `validated` and its `evidence` names a commit on
//! trunk.
//!
//! A reliability talk names a check, not a change, so no landing answers it (G148), and one sent
//! without `expires` never expires (G52). Its sender can supersede it; its recipient had no way
//! to close it. `Talk::closed` said delivery and reliability talks "close on evidence, which
//! comes with backbriefs (G47)", and nothing wrote that close: talk ei3tapt54lxmwo4d took a
//! `done` backbrief on 2026-10-30 and still read `stale`, owed by its recipient, on 2026-11-15.
//! Now the recipient's accept closes it: the close entry names `state: done` and the trunk
//! commit the evidence resolves to (`evidence`). A `done` with no evidence, evidence that is not
//! on trunk, or a status that is not an end closes nothing, as before. The host's judge takes
//! that close only from the talk's `who`, on a reliability talk, when the talk's latest
//! backbrief is `done` or `validated` with evidence that resolves to the close's commit, and
//! that commit is on the host's trunk.
//!
//! The guards to mutate: in `src/talk/accept.rs` `close_evidenced`, the reliability-only
//! filter; in `src/talk.rs`, the end-status filter of `Talk::end_evidence` and the ancestry test
//! of `evidence_on_trunk`; in `src/talk/judge.rs` `evidenced`, the kind, the no-`by` match, the
//! latest-backbrief match and the on-trunk test.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::{Value, json};
use support::talk::{TalkCase, from_now, hosted_talk_landing, talk_ref};

/// A talk from `agent-c` to `agent` of `kind` about `target` (`--check` or `--change`).
fn ask(case: &TalkCase, what: &str, kind: &str, target: &[&str]) -> String {
    let follow_up = from_now(3600);
    let mut args = vec!["agent", what, "--kind", kind];
    args.extend(target);
    args.extend(["--follow-up", &follow_up]);
    let (code, sent) = case.talk("agent-c", &args);
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

fn trunk_tip(case: &TalkCase) -> String {
    case.host_tip("refs/heads/main").unwrap()
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
fn a_reliability_talk_closes_on_a_done_backbrief_whose_evidence_is_on_trunk() {
    let case = hosted_talk_landing();
    let id = ask(&case, "Run the soak", "reliability", &["--check", "soak"]);
    let name = talk_ref("agent", &id);
    assert_eq!(closed_now(&case, &id), Vec::<Value>::new(), "acked only");
    let trunk = trunk_tip(&case);
    let short = &trunk[..11];

    // `ready` with trunk evidence is no end: it closes nothing.
    backbrief(&case, &id, &["ready", "--evidence", short]);
    assert_eq!(closed_now(&case, &id), Vec::<Value>::new());

    // `done` with no evidence closes nothing: only evidence does.
    backbrief(&case, &id, &["done"]);
    assert_eq!(closed_now(&case, &id), Vec::<Value>::new());

    // `done` with evidence that is not on trunk closes nothing.
    let off_trunk = case
        .test_box
        .git_ok(
            &case.a,
            &[
                "commit-tree",
                &format!("{trunk}^{{tree}}"),
                "-p",
                &trunk,
                "-m",
                "not on trunk",
            ],
        )
        .trim()
        .to_owned();
    backbrief(&case, &id, &["done", "--evidence", &off_trunk]);
    assert_eq!(closed_now(&case, &id), Vec::<Value>::new());

    // `done` with a trunk commit as evidence closes the talk, naming that commit in full.
    backbrief(&case, &id, &["done", "--evidence", short]);
    let closed = closed_now(&case, &id);
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(closed[0]["state"], "done", "{closed:?}");
    let tip = case.host_tip(&name).unwrap();
    let close = entry(&case, &tip, "entries/0006-close.json");
    assert_eq!(close["state"], "done", "{close}");
    assert_eq!(close["evidence"], trunk.as_str(), "{close}");
    assert_eq!(close["actor"]["name"], "agent", "{close}");

    // The talk reads closed and owes nothing; a later accept closes nothing again.
    let (code, shown) = case.talk("agent", &["--show", &id]);
    assert_eq!(code, 0, "{shown}");
    assert_eq!(shown["detail"]["standing"], "closed", "{shown}");
    assert!(shown["detail"]["owes"].is_null(), "{shown}");
    assert_eq!(closed_now(&case, &id), Vec::<Value>::new());
}

#[test]
fn a_delivery_talk_takes_no_close_on_evidence_that_is_no_merge() {
    let case = hosted_talk_landing();
    let id = ask(&case, "Tag it", "delivery", &["--change", "abcd2345"]);
    assert_eq!(closed_now(&case, &id), Vec::<Value>::new(), "acked only");
    let trunk = trunk_tip(&case);
    backbrief(&case, &id, &["done", "--evidence", &trunk]);
    // A delivery talk closes on a landing: one that names its change (G148), or one its
    // evidence names (BACKLOG 241). Trunk evidence that is no landing closes nothing.
    assert_eq!(closed_now(&case, &id), Vec::<Value>::new());
}

/// A `done` close the recipient writes by hand, past its bay.
fn forged_close(case: &TalkCase, talk: &str, fields: &Value) -> String {
    let name = talk_ref("agent", talk);
    let tip = case.local_tip(&case.a, &name).unwrap();
    let mut close = json!({"schema": "bay.talk/1", "entry": "close", "talk": talk,
        "actor": {"name": "agent", "session": "s-agent"}, "state": "done",
        "at": bay::clock::rfc3339_of(bay::live::unix_now())});
    for (key, value) in fields.as_object().unwrap() {
        close[key] = value.clone();
    }
    let git = bay::git::Git::new(&case.a);
    let commit = bay::talk::entry_commit(&git, &tip, &close, &case.keys.agent, "agent").unwrap();
    format!("{commit}:{name}")
}

#[test]
fn the_host_takes_a_done_close_only_on_the_latest_backbriefs_trunk_evidence() {
    let case = hosted_talk_landing();
    let id = ask(&case, "Run the soak", "reliability", &["--check", "soak"]);
    let about = ask(&case, "Tag it", "delivery", &["--change", "abcd2345"]);
    let (code, status) = case.status("agent");
    assert_eq!(code, 0, "{status}");
    let trunk = trunk_tip(&case);
    let before = case
        .test_box
        .git_bare(&case.host, &["rev-parse", "refs/heads/main~1"])
        .trim()
        .to_owned();

    // The latest backbrief is no end: no close on evidence.
    backbrief(&case, &id, &["ready", "--evidence", &trunk]);
    let stderr = case.refused(
        &case.a,
        &forged_close(&case, &id, &json!({"evidence": trunk})),
    );
    assert!(stderr.contains("done or validated"), "{stderr}");

    // A close names the latest backbrief's evidence, not another trunk commit.
    backbrief(&case, &id, &["done", "--evidence", &trunk[..11]]);
    let stderr = case.refused(
        &case.a,
        &forged_close(&case, &id, &json!({"evidence": before})),
    );
    assert!(stderr.contains("is not the evidence"), "{stderr}");

    // A done close names its commit, no talk `by` and no landing.
    for (extra, why) in [
        (
            json!({"evidence": trunk, "by": "abcdefgh23456789"}),
            "and no talk `by`",
        ),
        (
            json!({"evidence": trunk, "landing": trunk}),
            "only an answered close names a landing",
        ),
        (json!({}), "names its `evidence` commit"),
    ] {
        let stderr = case.refused(&case.a, &forged_close(&case, &id, &extra));
        assert!(stderr.contains(why), "{extra}: {stderr}");
    }

    // A delivery talk closes on evidence only when it names a landing (BACKLOG 241).
    backbrief(&case, &about, &["done", "--evidence", &trunk]);
    let stderr = case.refused(
        &case.a,
        &forged_close(&case, &about, &json!({"evidence": trunk})),
    );
    assert!(stderr.contains("is not a merge"), "{stderr}");

    // Evidence the host holds off its trunk (an entry of the talk itself) closes nothing.
    let side = case.host_tip(&talk_ref("agent", &id)).unwrap();
    backbrief(&case, &id, &["done", "--evidence", &side]);
    let stderr = case.refused(
        &case.a,
        &forged_close(&case, &id, &json!({"evidence": side})),
    );
    assert!(stderr.contains("is not the evidence"), "{stderr}");

    // The true close is taken.
    backbrief(&case, &id, &["done", "--evidence", &trunk[..11]]);
    let out = case
        .test_box
        .git(&case.a)
        .args([
            "push",
            "origin",
            &forged_close(&case, &id, &json!({"evidence": trunk})),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
