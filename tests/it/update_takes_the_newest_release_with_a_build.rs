//! bay is 0.x, so every release is a pre-release and every release is a candidate: with no pin
//! and no `--version`, `bay update` takes the newest release that has a build for this
//! platform, skipping back past a newer one that has none. Update channels are gone: the
//! `--channel` flag is accepted and ignored, and `BAY_UPDATE_CHANNEL` and `bay.updateChannel`
//! are not read. The fixture release is the one of G50.

// Each case runs shell scripts (the fixture's binaries), so these run on Unix only.
#![cfg(unix)]

use crate::release_fixture;

use std::process::Command;

use release_fixture::RetryBusy as _;
use release_fixture::{Fixture, run, target};

const OLDER: &str = "9.1.0";
const RC: &str = "9.2.0-rc.1";

/// `bay update <args>` with `env` set: its exit code and result.
fn update(fixture: &Fixture, env: &[(&str, &str)], args: &[&str]) -> (i32, serde_json::Value) {
    let mut cmd = fixture.bay(fixture.path());
    cmd.envs(env.iter().copied()).arg("update").args(args);
    let out = cmd.output_busy();
    let text = String::from_utf8(out.stdout).unwrap();
    let value = serde_json::from_str(&text).unwrap_or_else(|_| panic!("one result: {text}"));
    (out.status.code().unwrap(), value)
}

#[test]
fn a_newer_pre_release_is_taken_with_no_setting() {
    let fixture = Fixture::new();
    fixture.release(RC, RC, None);
    fixture.release(OLDER, OLDER, None);
    fixture.list(&[(RC, true), (OLDER, false)]);

    let (code, result) = update(&fixture, &[], &["--check"]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["detail"]["target"], RC, "{result}");
    assert_eq!(result["detail"]["due"], true, "{result}");
    assert!(result["detail"].get("channel").is_none(), "{result}");
    assert_eq!(fixture.state().unwrap()["newest"], RC);
    fixture.assert_untouched();
}

#[test]
fn a_newest_release_with_no_build_here_is_skipped_for_the_next_older() {
    let fixture = Fixture::new();
    fixture.release(OLDER, OLDER, None);
    // 9.2.0 is the newest, but it was built only for another platform.
    fixture.list_with(&[("9.2.0", false, false), (OLDER, false, true)]);

    let (code, result) = update(&fixture, &[], &["--check"]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["detail"]["target"], OLDER, "{result}");
    fixture.assert_untouched();

    let (code, result) = update(&fixture, &[], &[]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["detail"]["updated"], true, "{result}");
    let version = run(Command::new(fixture.bin()).arg("--version"));
    assert_eq!(version.trim(), format!("bay {OLDER}"));
}

#[test]
fn no_release_with_a_build_here_is_unavailable_naming_the_platform() {
    let fixture = Fixture::new();
    fixture.list_with(&[("9.2.0", false, false), (OLDER, true, false)]);

    let (code, result) = update(&fixture, &[], &[]);
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "UPDATE_UNAVAILABLE", "{result}");
    let why = result["detail"]["why"].as_str().unwrap();
    assert_eq!(why, format!("no release has a build for {}", target()));
    fixture.assert_untouched();
}

#[test]
fn a_channel_setting_changes_nothing() {
    let fixture = Fixture::new();
    fixture.release(RC, RC, None);
    fixture.release(OLDER, OLDER, None);
    fixture.list(&[(RC, true), (OLDER, false)]);

    // Old scripts that still pass --channel keep working; the flag is not read.
    for channel in ["stable", "latest", "nightly"] {
        let (code, result) = update(&fixture, &[], &["--check", "--channel", channel]);
        assert_eq!(code, 0, "--channel {channel}: {result}");
        assert_eq!(
            result["detail"]["target"], RC,
            "--channel {channel}: {result}"
        );
    }
    let (code, result) = update(&fixture, &[("BAY_UPDATE_CHANNEL", "stable")], &["--check"]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["detail"]["target"], RC, "{result}");

    fixture.box_config("bay.updateChannel", "bogus");
    let (code, result) = update(&fixture, &[], &["--check"]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["detail"]["target"], RC, "{result}");
    fixture.assert_untouched();

    // `--help` no longer offers the flag.
    let out = fixture
        .bay(fixture.path())
        .args(["update", "--help"])
        .output_busy();
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--version"), "{help}");
    assert!(!help.contains("--channel"), "{help}");
}
