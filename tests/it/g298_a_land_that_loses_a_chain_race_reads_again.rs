//! G298: a land that loses a race on its change chain reads the chain again, never a fault.
//!
//! A land reads the change's chain, then writes its request on it by compare-and-swap. When
//! another writer moves the chain in between (an executor recording an attempt, or a second
//! land of the same view), the land's write loses. It is then `CHAIN_MOVED`: `land` reads the
//! chain again and decides from what it finds, so the agent sees the change queued, never
//! `failed` with "the change chain moved" for a race it did nothing wrong in.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/publish.rs` `Ctx::save_or_moved`: `Err(CHAIN_MOVED.to_owned())` to
//!   `Err("the change chain moved".to_owned())` (the land fails as a fault).
//! - `src/publish.rs` `submit`: `ctx.save_or_moved(` back to `ctx.save(` for the request.
//! - `src/publish.rs` `retry_blocked`: the same for the rearm is held by the same rule; the
//!   case here drives the request, the race the Mac's resubmit test met.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::time::{Duration, Instant};

use serde_json::Value;
use support::approval::trunk;

#[test]
fn a_land_whose_request_loses_the_chain_race_is_queued_not_failed() {
    let trunk = trunk();
    let (id, view) = trunk.start("CARD-race", &["src/"], &[("src/race.rs", "fn race() {}\n")]);

    // The first land holds after it read the chain, before it writes its request.
    let release = trunk.test_box.path().join("release-land");
    let first = trunk
        .test_box
        .bay_process(&view)
        .env("BAY_TEST_PAUSE_AT", "before-submit")
        .env("BAY_TEST_PAUSE_FILE", &release)
        .args(["--json", "land"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let paused = trunk.test_box.path().join("release-land.paused");
    let deadline = Instant::now() + Duration::from_secs(120);
    while !paused.exists() {
        assert!(
            Instant::now() < deadline,
            "the land never reached its request"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Meanwhile a second land of the same view writes the request on the chain.
    let (code, second) = support::result_json(trunk.test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{second}");
    assert_eq!(second["decision"], "queued", "{second}");
    let op = second["op_id"].as_str().unwrap().to_owned();
    std::fs::write(&release, "").unwrap();

    let out = first.wait_with_output().unwrap();
    let first: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stderr)));
    assert_ne!(
        first["decision"], "failed",
        "a lost chain race failed the land: {first}"
    );
    assert_eq!(first["decision"], "queued", "{first}");
    // It read the chain again and found the same revision's operation: one request, not two.
    assert_eq!(first["op_id"], op.as_str(), "{first}");
    assert_eq!(first["change"], id.as_str(), "{first}");
}
