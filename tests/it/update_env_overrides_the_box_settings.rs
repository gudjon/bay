//! A process's `BAY_UPDATE_VERSION` overrides the box's global Git config `bay.updateVersion`
//! for that process; with no pin, `--version` picks the target. On a shared login every agent
//! shares one global config, so its supervisor chooses for it in its environment. A set
//! variable that is not a version is refused by name, never ignored. The fixture release is the
//! one of G50.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::release_fixture;

use release_fixture::Fixture;
use release_fixture::RetryBusy as _;

const NEWER: &str = "9.0.0";
const NEWEST: &str = "9.1.0";
const RC: &str = "9.2.0-rc.1";

/// Releases `NEWER` and `NEWEST`, and a list whose newest is a pre-release.
fn fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture.release(NEWER, NEWER, None);
    fixture.release(NEWEST, NEWEST, None);
    fixture.list(&[(RC, true), (NEWEST, false), (NEWER, false)]);
    fixture
}

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
fn the_env_pin_wins_over_the_box_pin() {
    let fixture = fixture();
    fixture.box_config("bay.updateVersion", NEWER);
    let env = [("BAY_UPDATE_VERSION", NEWEST)];

    let (code, result) = update(&fixture, &env, &["--check"]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["detail"]["target"], NEWEST, "{result}");
    assert_eq!(result["detail"]["pinned"], NEWEST, "{result}");

    // A target off the env pin, even the box's own pin, is refused naming the env pin.
    let (code, result) = update(&fixture, &env, &["--version", NEWER]);
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "UPDATE_PINNED", "{result}");
    assert_eq!(result["detail"]["pinned"], NEWEST, "{result}");
    let why = result["detail"]["why"].as_str().unwrap();
    assert!(
        why.contains("BAY_UPDATE_VERSION") && why.contains(NEWEST),
        "{why}"
    );
    fixture.assert_untouched();

    // Without the variable, the box's pin holds and is named as config.
    let (code, result) = update(&fixture, &[], &["--version", NEWEST]);
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["detail"]["pinned"], NEWER, "{result}");
    let why = result["detail"]["why"].as_str().unwrap();
    assert!(why.contains("bay.updateVersion"), "{why}");
    assert!(!why.contains("BAY_UPDATE_VERSION"), "{why}");
}

#[test]
fn the_version_flag_picks_the_target_over_the_newest() {
    let fixture = fixture();
    let (code, result) = update(&fixture, &[], &["--check"]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["detail"]["target"], RC, "{result}");

    // With no pin, --version picks the target over the newest release.
    let (code, result) = update(&fixture, &[], &["--check", "--version", NEWER]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["detail"]["target"], NEWER, "{result}");
    fixture.assert_untouched();
}

#[test]
fn an_invalid_env_value_is_refused_by_name() {
    let fixture = fixture();
    // The box's own settings are valid; the variable is checked anyway, never skipped.
    fixture.box_config("bay.updateVersion", NEWER);
    for (name, value) in [
        ("BAY_UPDATE_VERSION", "latest"),
        ("BAY_UPDATE_VERSION", "9.1"),
        ("BAY_UPDATE_VERSION", ""),
    ] {
        let (code, result) = update(&fixture, &[(name, value)], &["--check"]);
        assert_eq!(code, 20, "{name}={value:?}: {result}");
        assert_eq!(result["decision"], "failed", "{result}");
        let detail = result["detail"].to_string();
        assert!(detail.contains(name), "{name}={value:?}: {detail}");
    }
    fixture.assert_untouched();
}
