//! G570 (a fleet ask, 2026-10-29): `bay queue` shows the live line, head first, as one
//! `bay.queue/1` document, so a tool beside bay (as lazygit is beside Git) reads the line in one
//! fast read and never parses `bay status`.
//!
//! A submitted change no runner has placed yet is `arriving`, never missing: from another
//! clone, `bay queue` names it before any runner runs, and the sentence says it waits for a
//! runner. Three changes in line: `bay queue` names each place in line order with its change,
//! intent, paths, wait and state, and the sentence counts all four. Once a runner merges them,
//! the line is empty, though their places stay in the queue's history. `bay help --json`
//! carries the document's schema, and the document, a place and an arriving row each carry
//! every field the schema requires.
//!
//! Guard to mutate: `src/queue.rs`, `Queue::live`: drop `!released.contains(&one.seq)` (the
//! merged places are still shown as waiting).

use crate::support;

use std::path::PathBuf;

use serde_json::Value;
use support::PRODUCER;
use support::queued::{Check, host};

#[test]
fn bay_queue_names_each_waiting_change_in_line_order_and_none_once_merged() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let observer = held.test_box.clone_of(&held.remote, "observer");

    // GIVEN a change submitted while no runner runs WHEN another clone reads the line THEN it
    // names the change as arriving, and says it waits for a runner.
    let started = held
        .test_box
        .bay_ok(&held.repo, &["start", "early", "--paths", "src/"]);
    let early = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/early.rs"), "fn early() {}\n").unwrap();
    let submitted = held.test_box.bay_ok(&view, &["merge"]);
    assert_eq!(submitted["decision"], "queued", "{submitted}");
    let line = held.test_box.bay_ok(&observer, &["queue"]);
    let detail = &line["detail"];
    assert_eq!(detail["schema"], "bay.queue/1", "{line}");
    assert_eq!(detail["depth"], 0, "{line}");
    let arriving = detail["arriving"].as_array().unwrap();
    assert_eq!(arriving.len(), 1, "{line}");
    assert_eq!(arriving[0]["change"], early.as_str(), "{line}");
    assert_eq!(arriving[0]["intent"], "early", "{line}");
    assert_eq!(arriving[0]["paths"], serde_json::json!(["src/"]), "{line}");
    assert!(arriving[0]["requested_at"].is_string(), "{line}");
    assert!(arriving[0]["waited_s"].is_u64(), "{line}");
    assert_eq!(detail["unread"], serde_json::json!([]), "{line}");
    let say = line["say"].as_str().unwrap();
    assert!(say.starts_with("One change waits for a runner to take it"), "{say}");

    // GIVEN three changes in line WHEN another clone reads the line THEN it names each one,
    // head first, and still names the one no runner placed.
    let changes: Vec<String> = ["one", "two", "three"]
        .iter()
        .map(|file| held.queue(file, "src"))
        .collect();
    let line = held.test_box.bay_ok(&observer, &["queue"]);
    let detail = &line["detail"];
    assert_eq!(detail["depth"], 3, "{line}");
    let places = detail["places"].as_array().unwrap();
    let shown: Vec<&str> = places
        .iter()
        .map(|place| place["change"].as_str().unwrap())
        .collect();
    assert_eq!(shown, changes, "{line}");
    for (at, place) in places.iter().enumerate() {
        assert_eq!(place["place"], at + 1, "{line}");
        assert_eq!(place["intent"], ["one", "two", "three"][at], "{line}");
        assert_eq!(place["paths"], serde_json::json!(["src/"]), "{line}");
        assert!(place["actor"].is_string(), "{line}");
        assert!(place["joined_at"].is_string(), "{line}");
        assert!(place["waited_s"].is_u64(), "{line}");
        assert_eq!(place["state"], "submitted", "{line}");
    }
    let seqs: Vec<u64> = places
        .iter()
        .map(|place| place["seq"].as_u64().unwrap())
        .collect();
    assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{line}");
    let arriving = detail["arriving"].as_array().unwrap();
    assert_eq!(arriving.len(), 1, "{line}");
    assert_eq!(arriving[0]["change"], early.as_str(), "{line}");
    assert_eq!(detail["unread"], serde_json::json!([]), "{line}");
    let say = line["say"].as_str().unwrap();
    assert!(say.starts_with("4 changes wait to be added to main"), "{say}");

    // The schema `bay help --json` exports requires every field the document, a place and an
    // arriving row carry.
    let help = held.test_box.bay_ok(&observer, &["help"]);
    let schema = &help["detail"]["queue_schema"];
    assert_eq!(schema["title"], "bay.queue/1", "{schema}");
    let required = |at: &Value| -> Vec<String> {
        let mut keys: Vec<String> = at["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|key| key.as_str().unwrap().to_owned())
            .collect();
        keys.sort();
        keys
    };
    let fields = |at: &Value| -> Vec<String> {
        let mut keys: Vec<String> = at.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    };
    assert_eq!(fields(detail), required(schema), "{line}");
    assert_eq!(
        fields(&places[0]),
        required(&schema["properties"]["places"]["items"]),
        "{line}"
    );
    assert_eq!(
        fields(&arriving[0]),
        required(&schema["properties"]["arriving"]["items"]),
        "{line}"
    );

    // WHEN a runner places the arriving change and merges all four THEN the line is empty,
    // though their places stay in the queue's history.
    let runner = held.executor("exec-a", PRODUCER);
    let mut line = Value::Null;
    for _ in 0..6 {
        let _ = held.pass(&runner);
        line = held.test_box.bay_ok(&observer, &["queue"]);
        if line["detail"]["depth"] == 0 && line["detail"]["arriving"] == serde_json::json!([]) {
            break;
        }
    }
    assert_eq!(line["detail"]["depth"], 0, "{line}");
    assert_eq!(line["detail"]["places"], serde_json::json!([]), "{line}");
    assert_eq!(line["detail"]["arriving"], serde_json::json!([]), "{line}");
    assert_eq!(line["say"], "Nothing waits to be added to main.", "{line}");
    let history = held.test_box.git_bare(
        &held.remote,
        &["for-each-ref", "--format=%(refname)", "refs/bay/queue/"],
    );
    assert!(history.lines().count() >= 4, "{history}");
}
