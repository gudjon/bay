//! G709 (a measurement, 2026-11-13: one `bay status <change>` ran 266 Git calls, about 150 of
//! them one `cat-file` each for a talk's entries and its root commit). A status read reads every
//! object its talk rows need in one batch: however many talks it lists, it runs no `cat-file
//! blob` or `cat-file commit` of its own for them.
//!
//! Read from the trace a test build writes when `BAY_TEST_GIT_TRACE` names a file (G171).
//!
//! Guard to mutate: in `src/talk/standing.rs` `open_where`, the `preload` call (each row reads
//! its own objects again).

#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::result_json;
use support::talk::{from_now, hosted_talk};

#[test]
fn a_status_reads_its_talks_in_one_batch() {
    // GIVEN three open talks to agent-b, each read by agent-b, so each holds an ack
    let case = hosted_talk();
    let follow_up = from_now(3600);
    for what in [
        "Review the parser",
        "Review the lexer",
        "Review the printer",
    ] {
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
            ],
        );
        assert_eq!(code, 0, "{sent}");
    }
    let (_, status) = case.status("agent-b");
    assert_eq!(
        status["detail"]["talks"].as_array().map(Vec::len),
        Some(3),
        "{status}"
    );

    // WHEN the sender reads its status
    let trace = case.test_box.path().join("trace.jsonl");
    let (_, status) = result_json(
        case.bay_as("agent")
            .env("BAY_TEST_GIT_TRACE", &trace)
            .arg("status"),
    );

    // THEN it lists the three talks and read none of their objects on its own
    assert_eq!(
        status["detail"]["talks"].as_array().map(Vec::len),
        Some(3),
        "{status}"
    );
    let git = |args: &[&str]| -> String {
        let out = case.test_box.git(&case.a).args(args).output().unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    let mut talk_objects = std::collections::BTreeSet::new();
    for name in git(&["for-each-ref", "--format=%(refname)", "refs/bay/talk/"]).lines() {
        talk_objects.extend(git(&["rev-list", name]).lines().map(str::to_owned));
        for entry in git(&["ls-tree", "-r", name]).lines() {
            if let Some(oid) = entry.split_whitespace().nth(2) {
                talk_objects.insert(oid.to_owned());
            }
        }
    }
    assert!(talk_objects.len() >= 6, "{talk_objects:?}");
    let single: Vec<String> = std::fs::read_to_string(&trace)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter_map(|line| {
            let args: Vec<&str> = line["args"]
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .collect();
            let at = args.iter().position(|arg| *arg == "cat-file")?;
            let (Some(&("blob" | "commit")), Some(name)) = (args.get(at + 1), args.get(at + 2))
            else {
                return None;
            };
            let oid = name.split('~').next()?;
            talk_objects.contains(oid).then(|| args.join(" "))
        })
        .collect();
    assert!(single.is_empty(), "{single:#?}");
}
