//! G453 (T6). `bay talk --show <id>` reads a talk and every reply to it, with no `git show`.
//!
//! A notice names a talk by its id, and the only way to read its ask and its replies was Git
//! (`git show <ref>:talk.json`, `git log -p <ref>`). Now `bay talk --show <id>` fetches as
//! `bay status` does for talks, plus that one talk wherever it lives, and returns a `bay.result/1`
//! (`state: "shown"`) whose detail is the talk's row as `bay status --since` reads it, how it
//! closed, its body, and every entry in order: its kind, who wrote it, when, and its own fields
//! (an answer's `value`, a backbrief's `status` and `evidence`). It writes no entry. An id no
//! talk has is refused.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/talk/show.rs` `show_talk`: the fetch of the one talk dropped (a reader that is no
//!   party to it finds no talk).
//! - `src/talk/show.rs` `entries`: an entry's own fields left out (no answer value).
//! - `src/talk/show.rs` `show_talk`: the missing talk returned as an empty detail, not refused.
//! - `src/talk/show.rs` `show_talk`: the id check dropped (`*` reads another talk).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::{Value, json};
use support::result_json;
use support::talk::{approvals_talk, talk_ref};

fn remote_tip(case: &support::talk::Approvals, name: &str) -> String {
    case.test_box
        .git_bare(&case.remote, &["rev-parse", name])
        .trim()
        .to_owned()
}

#[test]
fn show_lists_the_talk_and_each_entry_in_order() {
    let case = approvals_talk();
    let id = case.ask(
        "agent",
        "agent-b",
        "Pick the wire format",
        &["--kind", "discovery", "--decision", "wire-format"],
        &[],
    );
    let (code, status) = case.bay("agent-b", &["status"]);
    assert_eq!(code, 0, "{status}");
    let (code, answered) = case.bay("agent-b", &["talk", "--answer", &id, "--value", "json"]);
    assert_eq!(code, 0, "{answered}");
    let name = talk_ref("agent-b", &id);
    let tip = remote_tip(&case, &name);

    // The sender has not fetched since the answer: the show fetches it.
    let (code, shown) = case.bay("agent", &["talk", "--show", &id]);
    assert_eq!(code, 0, "{shown}");
    assert_eq!(shown["decision"], "ok", "{shown}");
    assert_eq!(shown["state"], "shown", "{shown}");
    let detail = &shown["detail"];
    assert_eq!(detail["talk"], id.as_str(), "{shown}");
    assert_eq!(detail["ref"], name.as_str(), "{shown}");
    assert_eq!(detail["from"], "agent", "{shown}");
    assert_eq!(detail["who"], "agent-b", "{shown}");
    assert_eq!(detail["what"], "Pick the wire format", "{shown}");
    assert_eq!(detail["standing"], "closed", "{shown}");
    assert_eq!(detail["closed"], "answered", "{shown}");
    assert_eq!(detail["body"]["decision"], "wire-format", "{shown}");
    let entries: Vec<(Value, Value)> = detail["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("{shown}"))
        .iter()
        .map(|entry| (entry["kind"].clone(), entry["by"].clone()))
        .collect();
    assert_eq!(
        entries,
        vec![
            (json!("ack"), json!("agent-b")),
            (json!("answer"), json!("agent-b"))
        ],
        "{shown}"
    );
    let answer = &detail["entries"][1];
    assert_eq!(answer["value"], "json", "{shown}");
    assert!(answer["at"].is_string(), "{shown}");
    assert_eq!(answer["n"], 2, "{shown}");

    // A person reads the same as text, a line each.
    let out = case
        .test_box
        .bay(&case.a)
        .env("BAY_ACTOR", "agent")
        .env("BAY_SESSION", "s-agent")
        .args(["talk", "--show", &id])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stderr).unwrap();
    assert!(
        text.starts_with(&format!(
            "talk {id} (discovery) from agent to agent-b, sent "
        )),
        "{text}"
    );
    assert!(
        text.contains(": closed (answered)\n  Pick the wire format\n  0001 ack by agent-b at "),
        "{text}"
    );
    assert!(text.contains("\n  0002 answer by agent-b at "), "{text}");
    assert!(text.trim_end().ends_with(": json"), "{text}");

    // It wrote nothing: the remote's chain is where the answer left it.
    assert_eq!(remote_tip(&case, &name), tip);
}

#[test]
fn show_names_a_backbrief_s_status_and_evidence() {
    let case = approvals_talk();
    let id = case.ask(
        "agent",
        "agent-b",
        "Ship the parser change",
        &["--kind", "delivery", "--change", "abcd2345"],
        &[],
    );
    case.entry(
        "agent-b",
        "agent-b",
        &id,
        "backbrief",
        &json!({"status": "blocked", "evidence": "abcd2345", "outcome": "The lexer lands first"}),
    );
    // The recipient reads it too.
    let (code, shown) = case.bay("agent-b", &["talk", "--show", &id]);
    assert_eq!(code, 0, "{shown}");
    let detail = &shown["detail"];
    assert_eq!(detail["closed"], Value::Null, "{shown}");
    let backbrief = detail["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "backbrief")
        .unwrap_or_else(|| panic!("{shown}"));
    assert_eq!(backbrief["by"], "agent-b", "{shown}");
    assert_eq!(backbrief["status"], "blocked", "{shown}");
    assert_eq!(backbrief["evidence"], "abcd2345", "{shown}");
    assert_eq!(backbrief["outcome"], "The lexer lands first", "{shown}");
}

#[test]
fn show_refuses_an_id_no_talk_has() {
    let case = approvals_talk();
    case.ask(
        "agent",
        "agent-b",
        "Pick a colour",
        &["--kind", "discovery", "--decision", "colour"],
        &[],
    );
    let (code, shown) = case.bay("agent", &["talk", "--show", "aaaaaaaaaaaaaaaa"]);
    assert_eq!(code, 10, "{shown}");
    assert_eq!(shown["reason"], "TALK_REFUSED", "{shown}");
    // A pattern is no id: it never reads some other talk.
    let (code, shown) = case.bay("agent", &["talk", "--show", "*"]);
    assert_eq!(code, 10, "{shown}");
    assert_eq!(shown["reason"], "TALK_REFUSED", "{shown}");
    assert_eq!(shown["detail"]["why"], "\"*\" is not a talk id", "{shown}");
}

#[test]
fn a_reader_that_is_no_party_reads_the_talk_from_the_remote() {
    let case = approvals_talk();
    let id = case.ask(
        "agent",
        "agent-b",
        "Pick the log format",
        &["--kind", "discovery", "--decision", "log-format"],
        &[],
    );
    // A clone that never fetched a talk ref: its own fetch of this one talk reads it.
    let c = case.test_box.clone_of(&case.remote, "c");
    let (code, shown) = result_json(
        case.test_box
            .bay(&c)
            .env("BAY_ACTOR", "agent-c")
            .env("BAY_SESSION", "s-agent-c")
            .args(["talk", "--show", &id]),
    );
    assert_eq!(code, 0, "{shown}");
    assert_eq!(shown["detail"]["what"], "Pick the log format", "{shown}");
    assert_eq!(shown["detail"]["entries"], json!([]), "{shown}");
}
