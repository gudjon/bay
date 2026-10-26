//! G393 (a fleet finding, 2026-10-26: a box pinned to 0.5.6 stayed there after 0.5.9 was out,
//! and runners told once of a new binary stayed on the old one after their pin moved past it).
//! A pin is the oldest version a box runs, never a ceiling: `bay update` takes a pinned box to
//! the newest release above its pin, and a version below the pin is still refused.
//!
//! Guard to mutate: `src/update.rs` `resolve`: return the pin as the target whenever one is set.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::release_fixture;

use release_fixture::Fixture;
use release_fixture::RetryBusy as _;

const OLD: &str = "9.0.0";
const NEW: &str = "9.1.0";

fn update(fixture: &Fixture, args: &[&str]) -> (i32, serde_json::Value) {
    let out = fixture
        .bay(fixture.path())
        .arg("update")
        .args(args)
        .output_busy();
    let text = String::from_utf8(out.stdout).unwrap();
    let value = serde_json::from_str(&text).unwrap_or_else(|_| panic!("one result: {text}"));
    (out.status.code().unwrap(), value)
}

#[test]
fn a_box_pinned_to_an_old_release_goes_to_the_newest() {
    let fixture = Fixture::new();
    fixture.release(OLD, OLD, None);
    fixture.release(NEW, NEW, None);
    fixture.list(&[(NEW, false), (OLD, false)]);
    fixture.box_config("bay.updateVersion", OLD);

    let (code, result) = update(&fixture, &["--check"]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["detail"]["target"], NEW, "{result}");
    assert_eq!(result["detail"]["pinned"], OLD, "{result}");

    // Below the pin is still refused.
    fixture.box_config("bay.updateVersion", NEW);
    let (code, result) = update(&fixture, &["--check", "--version", OLD]);
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "UPDATE_PINNED", "{result}");
    fixture.assert_untouched();
}
