//! G148. A delivery talk about a change closes when a landing that names the change lands
//! (`DESIGN` § 11c; `plans/wave2.md` G148, corrected by the review dossier's K9).
//!
//! A landing names change X when its publication carries `Fixes: X` (G161), `Reverts: X`
//! (`bay undo`) or `Supersedes: X` (`start --new`). Once one is on trunk, the recipient's accept
//! closes each open `delivery --change X` talk to it as `answered`, and the close entry names the
//! answering change by its relation (`fixed_by`, `undone_by` or `superseded_by`) and its
//! publication commit (`landing`), so a reader tells an answered talk from an abandoned one by
//! structure. A landing that names another change closes nothing. The host refuses an
//! `answered` close whose landing does not name the talk's change, or is not that change's
//! publication.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use serde_json::{Value, json};
use support::result_json;
use support::talk::{TalkCase, from_now, hosted_talk_landing, talk_ref};

/// Starts a change on `src/` as `agent`, writes `file`, and lands it. Returns (id, landing).
fn land(case: &TalkCase, args: &[&str], file: &str, text: &str) -> (String, String) {
    let mut start = vec!["start"];
    start.extend(args);
    start.extend(["--paths", "src/"]);
    let (code, started) = result_json(case.bay_as("agent").args(&start));
    assert_eq!(code, 0, "{started}");
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(file), text).unwrap();
    let (code, landed) = result_json(
        case.test_box
            .bay(&view)
            .env("BAY_ACTOR", "agent")
            .env("BAY_SESSION", "s-agent")
            .args(["merge", "--wait"]),
    );
    assert_eq!(code, 0, "{landed}");
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

fn closed(status: &Value) -> Vec<Value> {
    status["detail"]["talk_accept"]["closed"]
        .as_array()
        .unwrap()
        .clone()
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
fn a_talk_closes_when_a_landing_fixes_undoes_or_supersedes_its_change() {
    let case = hosted_talk_landing();
    let (x, _) = land(&case, &["CARD-x"], "src/x.rs", "fn x() {}\n");
    let (y, _) = land(&case, &["CARD-y"], "src/y.rs", "fn y() {}\n");
    let (z, _) = land(&case, &["CARD-z"], "src/z.rs", "fn z() {}\n");
    let about_x = ask_about(&case, &x, "Fix the crash in x");
    let about_y = ask_about(&case, &y, "Take y back out");
    let about_z = ask_about(&case, &z, "Redo z");

    // Accepted and acked; no landing names any of them yet.
    let (code, status) = case.status("agent");
    assert_eq!(code, 0, "{status}");
    assert_eq!(
        status["detail"]["talk_accept"]["acked"]
            .as_array()
            .unwrap()
            .len(),
        3,
        "{status}"
    );
    assert_eq!(closed(&status), Vec::<Value>::new(), "{status}");

    // A landing that names no change closes nothing, even when its intent says "fix".
    let fix_words = format!("fix {x}");
    land(&case, &[&fix_words], "src/x.rs", "fn x() { 0 }\n");
    let (code, status) = case.status("agent");
    assert_eq!(code, 0, "{status}");
    assert_eq!(closed(&status), Vec::<Value>::new(), "{status}");

    // A fix of x closes the talk about x, and only that one.
    let (fix, fix_landing) = land(
        &case,
        &["CARD-fix", "--fixes", &x],
        "src/x.rs",
        "fn x() { 1 }\n",
    );
    let (code, status) = case.status("agent");
    assert_eq!(code, 0, "{status}");
    assert_eq!(
        closed(&status),
        vec![
            json!({"talk": about_x, "state": "answered", "fixed_by": fix,
            "landing": fix_landing})
        ],
        "{status}"
    );
    let tip = case.host_tip(&talk_ref("agent", &about_x)).unwrap();
    let close = entry(&case, &tip, "entries/0002-close.json");
    assert_eq!(close["state"], "answered", "{close}");
    assert_eq!(close["fixed_by"], fix.as_str(), "{close}");
    assert_eq!(close["landing"], fix_landing.as_str(), "{close}");
    assert_eq!(close["actor"]["name"], "agent", "{close}");

    // An undo of y closes the talk about y, naming the undo.
    let (code, undone) = result_json(case.bay_as("agent").args(["undo", &y, "--wait"]));
    assert_eq!(code, 0, "{undone}");
    let undo = undone["change"].as_str().unwrap().to_owned();
    let undo_landing = trunk_tip(&case);
    let (code, status) = case.status("agent");
    assert_eq!(code, 0, "{status}");
    assert_eq!(
        closed(&status),
        vec![
            json!({"talk": about_y, "state": "answered", "undone_by": undo,
            "landing": undo_landing})
        ],
        "{status}"
    );

    // The next generation of z's intent supersedes z and closes the talk about z.
    let (next, next_landing) = land(&case, &["CARD-z", "--new"], "src/z.rs", "fn z() { 2 }\n");
    let (code, status) = case.status("agent");
    assert_eq!(code, 0, "{status}");
    assert_eq!(
        closed(&status),
        vec![
            json!({"talk": about_z, "state": "answered", "superseded_by": next,
            "landing": next_landing})
        ],
        "{status}"
    );

    // Closed once: a later accept closes nothing again.
    let (code, status) = case.status("agent");
    assert_eq!(code, 0, "{status}");
    assert_eq!(closed(&status), Vec::<Value>::new(), "{status}");
}

/// An `answered` close the recipient writes by hand, past its bay.
fn forged_close(case: &TalkCase, talk: &str, relation: &str, by: &str, landing: &str) -> String {
    let name = talk_ref("agent", talk);
    let tip = case.local_tip(&case.a, &name).unwrap();
    let mut close = json!({"schema": "bay.talk/1", "entry": "close", "talk": talk,
        "actor": {"name": "agent", "session": "s-agent"}, "state": "answered",
        "landing": landing, "at": bay::clock::rfc3339_of(bay::live::unix_now())});
    close[relation] = json!(by);
    let git = bay::git::Git::new(&case.a);
    let commit = bay::talk::entry_commit(&git, &tip, &close, &case.keys.agent, "agent").unwrap();
    format!("{commit}:{name}")
}

#[test]
fn the_host_refuses_an_answered_close_whose_landing_does_not_name_the_change() {
    let case = hosted_talk_landing();
    let (x, _) = land(&case, &["CARD-x"], "src/x.rs", "fn x() {}\n");
    let (y, y_landing) = land(&case, &["CARD-y"], "src/y.rs", "fn y() {}\n");
    let about_x = ask_about(&case, &x, "Fix the crash in x");
    let (code, status) = case.status("agent");
    assert_eq!(code, 0, "{status}");

    // y's landing names no change: it answers nothing.
    let stderr = case.refused(
        &case.a,
        &forged_close(&case, &about_x, "fixed_by", &y, &y_landing),
    );
    assert!(stderr.contains("does not name"), "{stderr}");

    // A fix of x, named under the wrong relation or with another landing, is refused too.
    let (fix, fix_landing) = land(
        &case,
        &["CARD-fix", "--fixes", &x],
        "src/x.rs",
        "fn x() { 1 }\n",
    );
    let stderr = case.refused(
        &case.a,
        &forged_close(&case, &about_x, "undone_by", &fix, &fix_landing),
    );
    assert!(stderr.contains("does not name"), "{stderr}");
    let stderr = case.refused(
        &case.a,
        &forged_close(&case, &about_x, "fixed_by", &fix, &y_landing),
    );
    assert!(stderr.contains("is not the landing of"), "{stderr}");

    // The true close is taken.
    let out = case
        .test_box
        .git(&case.a)
        .args([
            "push",
            "origin",
            &forged_close(&case, &about_x, "fixed_by", &fix, &fix_landing),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
