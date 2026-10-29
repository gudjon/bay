//! G454 (U5, the talk uplift plan; a reader that is not bay, such as Landingbay). A reader finds
//! the talk contract in `bay help --json`, and the open talks in `bay status --all --json`
//! inside its budget.
//!
//! `bay help --json` named the `bay.result/1` schema only, so a reader of `refs/bay/talk/*` had
//! no contract to read talks by. Now its detail has `talk`: the schema name `bay.talk/1`, the ref
//! layout, the body's fields, each kind with its target fields, each entry with its fields, the
//! status words and the standings, each list read from the table the code judges by.
//!
//! `bay status --all` listed every open talk the cache holds, oldest first, with no bound. Now it
//! lists them newest first while their JSON fits [`FLEET_TALKS_MAX_BYTES`] (24 KiB), and
//! `talks_more` counts those left out; the sessions' `owes_talks` and `owed_talks` still count
//! them all.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/contract.rs` `contract`: the `talk` entry left out.
//! - `src/talk/show.rs` `contract`: an entry kind's fields left out.
//! - `src/fleet.rs` `talks`: the byte budget dropped (every row listed, `talks_more` 0).
//! - `src/fleet.rs` `talks`: the rows kept oldest first (the newest is cut).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::{Value, json};
use support::talk::approvals_talk;
use support::{TestBox, result_json};

/// The budget `bay status --all` keeps its talk rows in.
const FLEET_TALKS_MAX_BYTES: usize = 24 * 1024;

#[test]
fn bay_help_json_names_the_talk_contract() {
    let test_box = TestBox::new();
    let (code, result) = result_json(test_box.bay(test_box.path()).arg("help"));
    assert_eq!(code, 0, "{result}");
    let talk = &result["detail"]["talk"];
    assert_eq!(talk["schema"], "bay.talk/1", "{talk}");
    assert_eq!(talk["ref"], "refs/bay/talk/<who>/<id>", "{talk}");
    let body = talk["body"].as_array().unwrap_or_else(|| panic!("{talk}"));
    for field in [
        "id",
        "from",
        "who",
        "kind",
        "what",
        "follow_up",
        "sent",
        "decides",
    ] {
        assert!(body.contains(&json!(field)), "{field}: {talk}");
    }
    let kinds = talk["kinds"].as_array().unwrap_or_else(|| panic!("{talk}"));
    assert!(
        kinds.contains(&json!({"kind": "discovery", "targets": ["decision"]})),
        "{talk}"
    );
    let entry = |kind: &str| -> Vec<Value> {
        talk["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["entry"] == kind)
            .unwrap_or_else(|| panic!("no {kind}: {talk}"))["fields"]
            .as_array()
            .unwrap()
            .clone()
    };
    assert!(entry("answer").contains(&json!("value")), "{talk}");
    assert!(entry("answer").contains(&json!("actor")), "{talk}");
    assert!(entry("backbrief").contains(&json!("status")), "{talk}");
    assert!(entry("backbrief").contains(&json!("evidence")), "{talk}");
    assert!(entry("close").contains(&json!("state")), "{talk}");
    assert!(entry("ack").contains(&json!("at")), "{talk}");
    assert_eq!(
        talk["status_words"],
        json!(["ready", "blocked", "validated", "done"]),
        "{talk}"
    );
    assert!(
        talk["standings"]
            .as_array()
            .unwrap()
            .contains(&json!("closed")),
        "{talk}"
    );
}

#[test]
fn the_fleet_lists_the_newest_open_talks_inside_its_budget() {
    let case = approvals_talk();
    // Four asks of about 7 KiB each: more than the budget holds.
    let mut ids = Vec::new();
    for n in 0..4 {
        let what = format!(
            "Ask {n}: {}",
            "read the parser change and say what breaks. ".repeat(160)
        );
        let decision = format!("d{n}");
        ids.push(case.ask(
            "agent",
            "agent-b",
            &what,
            &["--kind", "discovery", "--decision", &decision],
            &[],
        ));
        // Each `sent` is a second apart, so newest first is one order.
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }
    let (code, status) = case.bay("agent", &["status"]);
    assert_eq!(code, 0, "{status}");
    let (code, map) = case.bay("agent", &["status", "--all"]);
    assert_eq!(code, 0, "{map}");
    let detail = &map["detail"];
    let rows = detail["talks"]
        .as_array()
        .unwrap_or_else(|| panic!("{map}"));
    let listed: Vec<&str> = rows
        .iter()
        .map(|row| row["talk"].as_str().unwrap())
        .collect();
    let newest: Vec<&str> = ids
        .iter()
        .rev()
        .take(listed.len())
        .map(String::as_str)
        .collect();
    assert_eq!(listed, newest, "newest first");
    // About 7 KiB a row: some fit, and not all.
    assert!(
        (2..ids.len()).contains(&listed.len()),
        "{} rows listed",
        listed.len()
    );
    let bytes = serde_json::to_string(&detail["talks"]).unwrap().len();
    assert!(bytes <= FLEET_TALKS_MAX_BYTES, "{bytes} bytes");
    assert_eq!(
        detail["talks_more"],
        json!(ids.len() - listed.len()),
        "{map}"
    );
}
