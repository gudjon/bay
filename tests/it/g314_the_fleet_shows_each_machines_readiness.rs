//! G314. Each machine's readiness travels with its presence, and `bay status --fleet` shows
//! the whole fleet's.
//!
//! When `bay doctor` runs in a work repository whose remote answers, it publishes this
//! machine's verdict on the machine's live summary, `refs/bay/live/<machine>`, the ref its
//! sessions already travel on: `doctor: {at, bay_version, decision, reason, red, hooks,
//! sandbox, role}`, with no path and no secret. `bay status --fleet` lists each machine's
//! `doctor` block as the machine last published it, with its age (`null` for a machine that
//! never ran doctor), and `ready` counts the machines that are green, red or never ran doctor,
//! with the lowest bay version seen. No new command and no new flag.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/doctor.rs` `doctor_as`: drop `publish(dir, &result);` (no machine carries a verdict).
//! - `src/doctor.rs` `verdict`: `named("hooks").is_some_and(|row| row["state"] == "green")` to
//!   `true` (the red machine's hooks read `on`).
//! - `src/live.rs` `write_with`: `if lines.is_none() && doctor.is_none() {` to
//!   `if lines.is_none() {` (a machine with no sessions publishes nothing).
//! - `src/live.rs` `write_with`: drop `summary[DOCTOR_FIELD] = doctor;` (no verdict travels).
//! - `src/fleet.rs` `remote`: `"doctor": doctor})` to `"doctor": null})` (the other machine's
//!   verdict is not shown).
//! - `src/fleet.rs` `ready`: `Some("ok") => green += 1,` to `Some("ok") => red += 1,` (no
//!   machine counts green).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;

use serde_json::{Value, json};
use support::{TestBox, result_json};

const CLAUDE_ADAPTER: &str = "main = \"main\"\n[agent]\nproduct = \"claude\"\n";

/// The fleet map's row of `machine`.
fn row_of<'a>(fleet: &'a Value, name: &str) -> &'a Value {
    fleet["detail"]["machines"]
        .as_array()
        .unwrap_or_else(|| panic!("no machines: {fleet}"))
        .iter()
        .find(|row| row["machine"] == name)
        .unwrap_or_else(|| panic!("no machine {name}: {fleet}"))
}

/// Pushes from `dir` a live summary of `machine` that carries no doctor verdict, as a bay
/// before G314 wrote one.
fn publish_without_doctor(test_box: &TestBox, dir: &Path, machine: &str) {
    let summary = json!({"schema": "bay.live/1", "machine": machine,
        "at": "2026-10-19T00:00:00Z", "sessions": []});
    let file = test_box.path().join("live.json");
    std::fs::write(&file, summary.to_string()).unwrap();
    let blob = test_box.git_ok(dir, &["hash-object", "-w", file.to_str().unwrap()]);
    let index = test_box.path().join("live-index");
    let with_index = |args: &[&str]| {
        let out = test_box
            .git(dir)
            .env("GIT_INDEX_FILE", &index)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    let entry = format!("100644,{blob},live.json");
    with_index(&["update-index", "--add", "--cacheinfo", &entry]);
    let tree = with_index(&["write-tree"]);
    let commit = test_box.git_ok(dir, &["commit-tree", &tree, "-m", "live"]);
    test_box.git_ok(
        dir,
        &[
            "push",
            "-q",
            "origin",
            &format!("{commit}:refs/bay/live/{machine}"),
        ],
    );
}

#[test]
fn the_fleet_map_shows_each_machines_last_doctor_verdict_and_a_ready_count() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));
    let green = test_box.clone_of(&remote, "box-green");
    let red = test_box.clone_of(&remote, "box-red");
    test_box.git_ok(&green, &["config", "bay.machine", "box-green"]);
    test_box.git_ok(&red, &["config", "bay.machine", "box-red"]);

    // One machine installs bay's hook entries and is green; the other never did and is red.
    let (code, installed) = result_json(test_box.bay(&green).arg("install"));
    assert_eq!(code, 0, "{installed}");
    let (code, doctor) = result_json(test_box.bay(&green).arg("doctor"));
    assert_eq!(code, 0, "{doctor}");
    let (code, doctor) = result_json(test_box.bay(&red).arg("doctor"));
    assert_eq!(code, 10, "{doctor}");
    assert_eq!(doctor["reason"], "HOOKS_MISSING", "{doctor}");

    // The verdict travels on the machine's live ref, and names no path of the machine.
    let published = test_box.git_bare(
        &remote,
        &["cat-file", "blob", "refs/bay/live/box-green:live.json"],
    );
    let root = test_box.path().to_str().unwrap();
    assert!(!published.contains(root), "{published}");
    let summary: Value = serde_json::from_str(&published).unwrap();
    assert_eq!(summary["doctor"]["decision"], "ok", "{summary}");

    // A machine whose summary carries no verdict never ran doctor.
    publish_without_doctor(&test_box, &green, "box-never");

    // The fleet map reads the last fetch: `bay status` fetches, `--fleet` reads it.
    test_box.bay_ok(&red, &["status"]);
    let fleet = test_box.bay_ok(&red, &["status", "--fleet"]);

    let seen = &row_of(&fleet, "box-green")["doctor"];
    assert_eq!(seen["decision"], "ok", "{fleet}");
    assert_eq!(seen["reason"], Value::Null, "{fleet}");
    assert_eq!(seen["red"], json!([]), "{fleet}");
    assert_eq!(seen["hooks"], "on", "{fleet}");
    assert_eq!(seen["sandbox"], "green", "{fleet}");
    assert_eq!(seen["role"], "agent-view", "{fleet}");
    assert_eq!(seen["bay_version"], bay::version::BAY, "{fleet}");
    assert!(seen["at"].is_string(), "{fleet}");
    assert!(seen["age_s"].is_u64(), "{fleet}");

    // This machine's own verdict, as it last published it.
    let own = &row_of(&fleet, "box-red")["doctor"];
    assert_eq!(own["decision"], "blocked", "{fleet}");
    assert_eq!(own["reason"], "HOOKS_MISSING", "{fleet}");
    assert!(
        own["red"]
            .as_array()
            .is_some_and(|red| red.contains(&json!("hooks"))),
        "{fleet}"
    );
    assert_eq!(own["hooks"], "missing", "{fleet}");
    assert!(own["age_s"].is_u64(), "{fleet}");

    assert_eq!(
        row_of(&fleet, "box-never")["doctor"],
        Value::Null,
        "{fleet}"
    );

    let ready = &fleet["detail"]["ready"];
    assert_eq!(ready["machines"], 3, "{fleet}");
    assert_eq!(ready["green"], 1, "{fleet}");
    assert_eq!(ready["red"], 1, "{fleet}");
    assert_eq!(ready["never_ran_doctor"], 1, "{fleet}");
    assert_eq!(ready["lowest_bay_version"], bay::version::BAY, "{fleet}");
}
