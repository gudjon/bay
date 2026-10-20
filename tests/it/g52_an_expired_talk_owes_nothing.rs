//! G52: an expired talk owes nothing (`DESIGN` § 11c; ruling T3).
//!
//! A talk may carry `expires`, after which the ask is void. The read is the recipient's accept,
//! on record as the ack, and it is judged by the ack's commit time, never by the clock of whoever
//! looks later. A talk read after `expires` still gets its ack, so arrival stays a fact, and then
//! closes as `expired`: the notice prints `EXPIRED` and no backbrief is owed, so status never
//! lists it as owed, `stale` or `lost`. A talk read before `expires` owes its backbrief however
//! late it is looked at, even with a skewed clock. The host refuses an `expired` close on a talk
//! read in time.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use serde_json::{Value, json};
use support::result_json;
use support::talk::{TalkCase, canonical, contents, forge, from_now, hosted_talk, talk_ref};

fn fixture(name: &str, session: &str, cwd: &Path) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code-2.1.302")
        .join(name);
    let mut payload: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    payload["session_id"] = Value::from(session);
    payload["cwd"] = Value::from(cwd.display().to_string());
    payload
}

/// `bay hook <event>` as `agent-b` on its clone: its stderr, where a notice goes.
fn hook(case: &TalkCase, event: &str, name: &str) -> String {
    // The live poll (G54) ran a moment ago, so the hook only tells what accept already acked.
    let now = bay::live::unix_now();
    std::fs::create_dir_all(case.b.join(".git/bay")).unwrap();
    std::fs::write(
        case.b.join(".git/bay/talk-poll.json"),
        format!("{{\"last\": {now}}}"),
    )
    .unwrap();
    let payload = fixture(name, "s-b", &case.b);
    let mut child = case
        .test_box
        .bay_process(&case.b)
        .env("BAY_ACTOR", "agent-b")
        .args(["hook", event])
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
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    String::from_utf8(output.stderr).unwrap()
}

/// A delivery talk from `agent` to `agent-b` that expires in `secs`. Its id and `expires`.
fn send(case: &TalkCase, what: &str, secs: u64) -> (String, u64) {
    let (follow_up, expires) = (from_now(3600), from_now(secs));
    let (code, sent) = case.talk(
        "agent",
        &[
            "agent-b",
            what,
            "--kind",
            "delivery",
            "--change",
            "abcd2345",
            "--follow-up",
            &follow_up,
            "--expires",
            &expires,
        ],
    );
    assert_eq!(code, 0, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();
    (id, bay::clock::unix_of(&expires).unwrap())
}

fn rows(status: &Value) -> Vec<Value> {
    status["detail"]["talks"]
        .as_array()
        .unwrap_or_else(|| panic!("no talks in {status}"))
        .clone()
}

fn row(status: &Value, id: &str) -> Value {
    rows(status)
        .into_iter()
        .find(|row| row["talk"] == id)
        .unwrap_or_else(|| panic!("talk {id} not listed: {status}"))
}

fn listed(status: &Value, id: &str) -> bool {
    rows(status).iter().any(|row| row["talk"] == id)
}

fn closed(status: &Value) -> Vec<Value> {
    status["detail"]["talk_accept"]["closed"]
        .as_array()
        .unwrap_or_else(|| panic!("no talk_accept in {status}"))
        .clone()
}

#[test]
fn a_talk_read_after_it_expires_prints_expired_and_owes_no_backbrief() {
    let case = hosted_talk();
    hook(&case, "session-start", "01-SessionStart.json");

    // In time: read (acked) before `expires`.
    let (in_time, _) = send(&case, "Take the capture now", 5);
    let (code, status) = case.status("agent-b");
    assert_eq!(code, 0, "{status}");
    assert_eq!(
        status["detail"]["talk_accept"]["acked"],
        json!([in_time]),
        "{status}"
    );
    assert_eq!(closed(&status), Vec::<Value>::new(), "{status}");

    // Late: not read until after its `expires`.
    let (late, expires) = send(&case, "Take the second capture now", 5);
    while bay::live::unix_now() <= expires + 1 {
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let (code, status) = case.status("agent-b");
    assert_eq!(code, 0, "{status}");
    assert_eq!(
        status["detail"]["talk_accept"]["acked"],
        json!([late]),
        "the ack keeps arrival a fact: {status}"
    );
    let expires_text = bay::clock::rfc3339_of(expires);
    assert_eq!(
        closed(&status),
        vec![json!({"talk": late, "state": "expired", "expires": expires_text})],
        "only the talk read after its `expires` closes: {status}"
    );
    assert!(
        !listed(&status, &late),
        "an expired talk is owed by no one: {status}"
    );
    let owed = row(&status, &in_time);
    assert_eq!(owed["standing"], "arrived", "{owed}");
    assert_eq!(owed["owes"], "agent-b", "{owed}");

    // The notice prints EXPIRED for the late one, and the in-time one as it arrived.
    let told = hook(&case, "post-tool", "08-PostToolBatch.json");
    assert!(
        told.contains(&format!(
            "bay: EXPIRED: talk {late} (delivery) arrived for agent-b after its `expires`, so it owes no backbrief: read it with `git show refs/bay/talk/agent-b/{late}:talk.json`\n"
        )),
        "{told}"
    );
    assert!(
        told.contains(&format!(
            "bay: talk {in_time} (delivery) arrived for agent-b: read it with `git show refs/bay/talk/agent-b/{in_time}:talk.json`\n"
        )),
        "{told}"
    );

    // A reader whose clock runs two hours ahead: the in-time talk was read before `expires` by
    // its ack's commit time, so it still owes its backbrief, and nothing closes it.
    let (code, status) = result_json(
        case.bay_as("agent-b")
            .env("BAY_TEST_TALK_CLOCK_SKEW_S", "7200")
            .arg("status"),
    );
    assert_eq!(code, 0, "{status}");
    assert_eq!(closed(&status), Vec::<Value>::new(), "{status}");
    let owed = row(&status, &in_time);
    assert_ne!(owed["standing"], "expired", "{owed}");
    assert_eq!(owed["owes"], "agent-b", "{owed}");

    // The sender sees the same after its fetch: the late talk closed, the in-time one owed.
    let (code, status) = case.status("agent");
    assert_eq!(code, 0, "{status}");
    assert!(!listed(&status, &late), "{status}");
    assert_eq!(row(&status, &in_time)["owes"], "agent-b", "{status}");

    on_the_host(&case, &late, &in_time);
}

/// On the host, the late talk's chain holds the ack, then the expired close; and the host refuses
/// an expired close on the talk read in time.
fn on_the_host(case: &TalkCase, late: &str, in_time: &str) {
    let tip = case.host_tip(&talk_ref("agent-b", late)).unwrap();
    let close: Value = serde_json::from_str(&case.test_box.git_bare(
        &case.host,
        &["show", &format!("{tip}:entries/0002-close.json")],
    ))
    .unwrap();
    assert_eq!(close["state"], "expired", "{close}");

    let name = talk_ref("agent-b", in_time);
    let held = case.local_tip(&case.b, &name).unwrap();
    let at = bay::live::unix_now();
    let entry = json!({"schema": "bay.talk/1", "entry": "close", "talk": in_time,
        "actor": {"name": "agent-b", "session": "s-agent-b"}, "state": "expired",
        "at": bay::clock::rfc3339_of(at)});
    let mut files = contents(&case.b, &held);
    files.push(("entries/0002-close.json".to_owned(), canonical(&entry)));
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let commit = forge(&case.b, &case.b_key, "agent-b", Some(&held), &files, at);
    let stderr = case.refused(&case.b, &format!("{commit}:{name}"));
    assert!(
        stderr.contains("was not read after its `expires`"),
        "{stderr}"
    );
}
