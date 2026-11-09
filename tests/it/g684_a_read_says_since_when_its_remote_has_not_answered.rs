//! G684 (a fleet finding, 2026-11-09: a box that could not reach its remote for two hours read,
//! on every `bay status`, that a newer fetch was still running and to read again shortly). A
//! stale read of a store whose last sweep could not ask the remote says so: its `stale_read`
//! warning names since when (`unreached.since`) and why (`unreached.why`), and its sentence says
//! the remote has not answered since then. A sweep that reaches the remote again clears it.
//!
//! Guards to mutate: in `src/status.rs` `swept_answer`, drop the `sweep::unreached` record (the
//! warning names no unreached remote); in `src/sweep.rs` `remember`, keep the record (a read after
//! the remote is back still says it has not answered).

use crate::support;

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::{TestBox, result_json};

use crate::g617_a_listings_sweep_serves_the_reads_in_its_window::store;

fn status(test_box: &TestBox, dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Value {
    let mut cmd = test_box.bay(dir);
    cmd.arg("status").args(args).env("BAY_ACTOR", "reader");
    for (key, value) in env {
        cmd.env(key, value);
    }
    result_json(&mut cmd).1
}

fn stale_read(result: &Value) -> Option<&Value> {
    result["detail"]["warnings"]
        .as_array()?
        .iter()
        .find(|warning| warning["code"] == "stale_read")
}

/// Waits up to 60 s until the sweep lock of the store at `dir` says its sweep ended.
fn sweep_ended(dir: &Path) {
    let lock = dir.join(".git").join("bay").join("sweep.lock");
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        let held: Value = std::fs::read_to_string(&lock)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or(Value::Null);
        if held["ended"].is_u64() {
            return;
        }
        assert!(Instant::now() < until, "the sweep never ended: {held}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_read_says_since_when_its_remote_has_not_answered() {
    let (test_box, _, remote, reader, _) = store();
    let fresh = status(&test_box, &reader, &[], &[]);
    assert!(stale_read(&fresh).is_none(), "{fresh}");

    // GIVEN a remote that stops answering, and a sweep that tried it
    let away = test_box.path().join("repo-remote.away");
    std::fs::rename(&remote, &away).unwrap();
    let _ = status(&test_box, &reader, &[], &[]);
    sweep_ended(&reader);

    // WHEN a read passes its deadline while its sweep runs on
    let stale = status(
        &test_box,
        &reader,
        &["--deadline", "1"],
        &[("BAY_TEST_SWEEP_DELAY_MS", "3000")],
    );

    // THEN it says the remote has not answered, since when and why
    let warning = stale_read(&stale).unwrap_or_else(|| panic!("no stale_read warning: {stale}"));
    let since = warning["unreached"]["since"]
        .as_str()
        .unwrap_or_else(|| panic!("no unreached remote named: {warning}"));
    assert!(
        warning["unreached"]["why"]
            .as_str()
            .is_some_and(|why| !why.is_empty()),
        "{warning}"
    );
    let say = stale["say"].as_str().unwrap();
    assert!(
        say.contains(&format!("the remote has not answered since {since}")),
        "{say}"
    );
    assert!(!say.contains("still running"), "{say}");

    // AND once the remote answers a sweep again, a stale read no longer says so
    sweep_ended(&reader);
    std::fs::rename(&away, &remote).unwrap();
    let back = status(&test_box, &reader, &[], &[]);
    assert!(stale_read(&back).is_none(), "{back}");
    let stale = status(
        &test_box,
        &reader,
        &["--deadline", "1"],
        &[
            ("BAY_TEST_SWEEP_DELAY_MS", "3000"),
            ("BAY_TEST_SWEPT_S", "0"),
        ],
    );
    let warning = stale_read(&stale).unwrap_or_else(|| panic!("no stale_read warning: {stale}"));
    assert!(warning.get("unreached").is_none(), "{warning}");
    assert!(
        stale["say"].as_str().unwrap().contains("still running"),
        "{stale}"
    );
}
