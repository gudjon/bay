//! G47b: a talk shows in the fleet map and in `bay status --since` (`DESIGN` § 11a, § 11c; `INTERFACES`
//! § 5, § 5a).
//!
//! `bay status --all` lists every open talk the local cache holds, as `bay status` lists them:
//! who owes the reply, the ages and the standing. Each session entry counts the open talks its
//! actor owes (`owes_talks`) and is owed (`owed_talks`), and a talk's sender is an actor the map
//! knows. `bay status --since` from a cursor lists `talk_arrived` once when a talk to its actor arrives,
//! and `talk_closed` once when that talk closes; a watch from the cursor it returned lists
//! neither again.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::result_json;
use support::talk::{TalkCase, from_now, hosted_talk_landing};

/// `bay status --since` as `agent` on its clone, outside any view, from `since`: its events and cursor.
fn watch(case: &TalkCase, since: Option<&str>) -> (Vec<Value>, String) {
    let mut args = vec!["status", "--since"];
    if let Some(cursor) = since {
        args.push(cursor);
    }
    let (code, watched) = result_json(case.bay_as("agent").args(&args));
    assert_eq!(code, 0, "{watched}");
    let events = watched["detail"]["events"]
        .as_array()
        .unwrap_or_else(|| panic!("{watched}"))
        .clone();
    let cursor = watched["detail"]["cursor"].as_str().unwrap().to_owned();
    (events, cursor)
}

/// The fleet map on `agent`'s clone after `bay status`, its fetch.
fn fleet(case: &TalkCase) -> Value {
    let (code, status) = case.status("agent");
    assert_eq!(code, 0, "{status}");
    let (code, map) = result_json(case.bay_as("agent").args(["status", "--all"]));
    assert_eq!(code, 0, "{map}");
    map["detail"].clone()
}

fn about<'a>(events: &'a [Value], kind: &str, talk: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["kind"] == kind && event["talk"] == talk)
        .collect()
}

/// The fleet entry of change `id`.
fn entry<'a>(fleet: &'a Value, id: &str) -> &'a Value {
    fleet["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["change"] == id)
        .unwrap_or_else(|| panic!("no entry for {id}: {fleet}"))
}

#[test]
fn the_fleet_lists_the_open_talk_and_watch_sees_it_arrive_and_close_once() {
    let case = hosted_talk_landing();
    // `agent` holds a change on its machine: its entry in the map is the owing session.
    let mut start = case.bay_as("agent");
    let (code, started) = result_json(start.args(["start", "CARD-wire", "--paths", "src/"]));
    assert_eq!(code, 0, "{started}");
    let change = started["change"].as_str().unwrap().to_owned();
    let (events, before) = watch(&case, None);
    let talk_events = events
        .iter()
        .filter_map(|event| event["kind"].as_str())
        .filter(|kind| kind.starts_with("talk_"))
        .count();
    assert_eq!(talk_events, 0, "{events:?}");

    let follow_up = from_now(3600);
    let (code, sent) = case.talk(
        "agent-c",
        &[
            "agent",
            "Pick the wire format",
            "--kind",
            "discovery",
            "--decision",
            "wire-format",
            "--follow-up",
            &follow_up,
        ],
    );
    assert_eq!(code, 0, "{sent}");
    let talk = sent["detail"]["talk"].as_str().unwrap().to_owned();

    // The watch accepts the talk and lists its arrival, once.
    let (events, arrived) = watch(&case, Some(&before));
    let arrival = about(&events, "talk_arrived", &talk);
    assert_eq!(arrival.len(), 1, "{events:?}");
    assert_eq!(arrival[0]["schema"], "bay.event/1", "{events:?}");
    assert_eq!(arrival[0]["from"], "agent-c", "{events:?}");
    assert_eq!(arrival[0]["owes"], "agent", "{events:?}");
    assert_eq!(arrival[0]["talk_kind"], "discovery", "{events:?}");
    let (again, arrived) = watch(&case, Some(&arrived));
    assert!(about(&again, "talk_arrived", &talk).is_empty(), "{again:?}");
    assert!(about(&again, "talk_closed", &talk).is_empty(), "{again:?}");

    // The fleet map lists the open talk, its sender as an actor, and the session that owes it.
    let map = fleet(&case);
    let rows: Vec<&Value> = map["talks"]
        .as_array()
        .unwrap_or_else(|| panic!("{map}"))
        .iter()
        .filter(|row| row["talk"] == talk.as_str())
        .collect();
    assert_eq!(rows.len(), 1, "{map}");
    assert_eq!(rows[0]["from"], "agent-c", "{map}");
    assert_eq!(rows[0]["who"], "agent", "{map}");
    assert_eq!(rows[0]["owes"], "agent", "{map}");
    assert_eq!(rows[0]["kind"], "discovery", "{map}");
    assert!(rows[0]["sent"].is_string(), "{map}");
    assert_eq!(rows[0]["standing"], "arrived", "{map}");
    assert!(
        map["actors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|actor| actor["actor"] == "agent-c"),
        "{map}"
    );
    assert_eq!(entry(&map, &change)["owes_talks"], 1, "{map}");
    assert_eq!(entry(&map, &change)["owed_talks"], 0, "{map}");

    // The answer closes the discovery talk: the watch lists the close, once.
    let (code, answered) = case.talk("agent", &["--answer", &talk, "--value", "json"]);
    assert_eq!(code, 0, "{answered}");
    let (events, closed) = watch(&case, Some(&arrived));
    let close = about(&events, "talk_closed", &talk);
    assert_eq!(close.len(), 1, "{events:?}");
    assert_eq!(close[0]["state"], "answered", "{events:?}");
    assert!(
        about(&events, "talk_arrived", &talk).is_empty(),
        "{events:?}"
    );
    let (again, _) = watch(&case, Some(&closed));
    assert!(about(&again, "talk_closed", &talk).is_empty(), "{again:?}");
    assert!(about(&again, "talk_arrived", &talk).is_empty(), "{again:?}");

    // Closed, it leaves the map, and the session owes nothing.
    let map = fleet(&case);
    assert!(
        map["talks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["talk"] != talk.as_str()),
        "{map}"
    );
    assert_eq!(entry(&map, &change)["owes_talks"], 0, "{map}");
}
