//! G786d (a fleet finding, 2026-11-18: changes waited 67 minutes for the one runner of a check
//! environment, and bay could name the thin environment but not how many waited for it or what
//! it cost). A live change's estimate names the check environments its paths may wake, and how
//! many live runners serve each; the flow names, for each environment the policy's checks run
//! in, how many places in line may wait for it, its runners, how long its checks ran, and how
//! long it takes to clear what waits.
//!
//! Guards to mutate: in `src/flow.rs` `flow`, drop `"environments"`; in `src/eta.rs`
//! `environments_of`, return every environment of the policy (the doc change would wait for
//! the code checks).

#![cfg(unix)]

use crate::support;

use support::queued::{Check, host};

const POLICY: &str = "revision = 1\n\n\
[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\n\
adapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\nwhen_paths = [\"src/**\"]\n\n\
[[check]]\nid = \"docs\"\nenvironment = \"writing\"\ncommand = \"scripts/bay-check\"\n\
adapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\nwhen_paths = [\"doc/**\"]\n";

#[test]
fn a_change_names_the_check_environments_it_waits_for() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 60\nheartbeat_s = 1\n",
        policy: Some(POLICY),
        seed: &[("src/a.rs", "fn a() {}\n"), ("doc/a.md", "a\n")],
        hold_first: false,
        fails: "false",
    });
    // GIVEN two code changes and a doc change in line
    let _one = held.queue("one", "src");
    let docs = held.queue("two", "doc");
    let _three = held.queue("three", "src");

    // WHEN the doc change is read THEN it waits for the writing checks alone
    let read = held.test_box.bay_ok(&held.repo, &["status", &docs]);
    let needs: Vec<&str> = read["detail"]["changes"][0]["eta"]["environments"]
        .as_array()
        .unwrap_or_else(|| panic!("no environments: {read}"))
        .iter()
        .map(|row| row["environment"].as_str().unwrap())
        .collect();
    assert_eq!(needs, ["writing"], "{read}");

    // AND the flow says how many places wait for each environment
    let status = held.test_box.bay_ok(&held.repo, &["status"]);
    let rows = status["detail"]["flow"]["environments"]
        .as_array()
        .unwrap_or_else(|| panic!("no environments: {status}"));
    let waiting = |name: &str| {
        rows.iter()
            .find(|row| row["environment"] == name)
            .unwrap_or_else(|| panic!("no {name}: {status}"))["waiting"]
            .as_u64()
    };
    assert_eq!(waiting("local"), Some(2), "{status}");
    assert_eq!(waiting("writing"), Some(1), "{status}");
    let local = rows
        .iter()
        .find(|row| row["environment"] == "local")
        .unwrap();
    assert_eq!(local["checks"][0]["check"], "tests", "{status}");
    assert!(
        local.get("runners").is_some() && local.get("clears_in_s").is_some(),
        "{status}"
    );
}
