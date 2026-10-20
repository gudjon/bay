//! Auto-update: at most one check a day, from `status` and `doctor` only, in the background.
//!
//! `DESIGN` (the binary's lifecycle, auto-update). The check never changes the command's
//! result or exit code; `BAY_DISABLE_AUTOUPDATE=1`, the box's `bay.updateAuto = false` and a
//! box pin (global config or the process's `BAY_UPDATE_VERSION`) each keep it from running, and a repository's `[update]` keys never do; the hook
//! never runs it. The fixture release is the one of G50.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::release_fixture;

use std::process::Command;
use std::time::{Duration, Instant};

use release_fixture::RetryBusy as _;
use release_fixture::{Build, Fixture};

const NEWER: &str = "9.0.0";
const DAY_S: u64 = 24 * 3600;

/// `bay doctor` in `repo`: its exit code and stdout.
fn doctor(fixture: &Fixture, repo: &std::path::Path, disabled: bool) -> (i32, String) {
    let mut cmd = fixture.bay(repo);
    cmd.arg("doctor");
    if disabled {
        cmd.env("BAY_DISABLE_AUTOUPDATE", "1");
    }
    let out = cmd.output_busy();
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout).unwrap(),
    )
}

/// Waits until `done` holds, for the background check.
fn eventually(what: &str, done: impl Fn() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(60), "never: {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn checked_at(fixture: &Fixture) -> Option<u64> {
    fixture.state()?["checked_at"].as_u64()
}

#[test]
fn doctor_updates_in_the_background_without_changing_its_result() {
    let fixture = Fixture::new();
    fixture.release(NEWER, NEWER, None);
    fixture.list(&[(NEWER, false)]);
    let repo = fixture.repo("");

    let baseline = doctor(&fixture, &repo, true);
    assert!(fixture.state().is_none(), "a disabled run records nothing");
    let with_check = doctor(&fixture, &repo, false);
    assert_eq!(
        with_check, baseline,
        "the check changes no result or exit code"
    );
    assert!(checked_at(&fixture).is_some(), "the check is recorded");

    eventually("the background update swaps the binary", || {
        Command::new(fixture.bin())
            .arg("--version")
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim() == format!("bay {NEWER}"))
    });
    eventually("the new binary's install ran", || {
        fixture.home().join(format!("installed-{NEWER}")).exists()
    });
}

#[test]
fn status_checks_once_a_day() {
    let fixture = Fixture::new();
    // The newest release is the installed version, so a check changes nothing on disk.
    fixture.list(&[(bay::version::BAY, false)]);
    let repo = fixture.repo("");

    let an_hour_ago = fixture.checked_ago(3600);
    let _ = fixture.bay(&repo).arg("status").output_busy();
    assert_eq!(
        checked_at(&fixture),
        Some(an_hour_ago),
        "not due within a day"
    );

    let two_days_ago = fixture.checked_ago(2 * DAY_S);
    let _ = fixture.bay(&repo).arg("status").output_busy();
    let checked = checked_at(&fixture).unwrap();
    assert!(checked > two_days_ago + DAY_S, "due after a day: {checked}");
    eventually("the background check records the newest release", || {
        fixture
            .state()
            .is_some_and(|state| state["newest"] == bay::version::BAY)
    });
    fixture.assert_untouched();
}

#[test]
fn the_switches_and_a_pin_keep_the_check_from_running() {
    let fixture = Fixture::new();
    fixture.release(NEWER, NEWER, None);
    fixture.list(&[(NEWER, false)]);
    let plain = fixture.repo("");
    let due = fixture.checked_ago(2 * DAY_S);

    // BAY_DISABLE_AUTOUPDATE=1 always wins.
    let _ = doctor(&fixture, &plain, true);
    assert_eq!(checked_at(&fixture), Some(due));

    // The hook never checks.
    let _ = fixture.bay(&plain).args(["hook", "pre-tool"]).output_busy();
    assert_eq!(checked_at(&fixture), Some(due));

    // The box's switch and the box's pin keep it from running.
    for (key, value) in [("bay.updateAuto", "false"), ("bay.updateVersion", NEWER)] {
        let fixture = Fixture::new();
        fixture.release(NEWER, NEWER, None);
        fixture.list(&[(NEWER, false)]);
        let repo = fixture.repo("");
        let due = fixture.checked_ago(2 * DAY_S);
        fixture.box_config(key, value);
        let _ = doctor(&fixture, &repo, false);
        let _ = fixture.bay(&repo).arg("status").output_busy();
        assert_eq!(checked_at(&fixture), Some(due), "{key}");
        fixture.assert_untouched();
    }

    // A pin in the process's environment holds the box still too, and an invalid one starts
    // no check.
    for value in [NEWER, "not-a-version"] {
        let fixture = Fixture::new();
        fixture.release(NEWER, NEWER, None);
        fixture.list(&[(NEWER, false)]);
        let repo = fixture.repo("");
        let due = fixture.checked_ago(2 * DAY_S);
        let _ = fixture
            .bay(&repo)
            .env("BAY_UPDATE_VERSION", value)
            .arg("status")
            .output_busy();
        assert_eq!(
            checked_at(&fixture),
            Some(due),
            "BAY_UPDATE_VERSION={value}"
        );
        fixture.assert_untouched();
    }
    fixture.assert_untouched();
}

#[test]
fn a_repository_cannot_switch_the_check_off() {
    let fixture = Fixture::new();
    fixture.list(&[(bay::version::BAY, false)]);
    let repo = fixture.repo(&format!("[update]\nauto = false\nversion = \"{NEWER}\"\n"));
    let due = fixture.checked_ago(2 * DAY_S);
    let _ = doctor(&fixture, &repo, false);
    assert!(
        checked_at(&fixture).unwrap() > due + DAY_S,
        "the box still checks"
    );
    eventually("the background check finishes", || {
        fixture
            .state()
            .is_some_and(|state| state["newest"] == bay::version::BAY)
    });
}

#[test]
fn concurrent_commands_start_at_most_one_check() {
    let fixture = Fixture::new();
    fixture.log_curl();
    fixture.list(&[(bay::version::BAY, false)]);
    let repo = fixture.repo("");
    let due = fixture.checked_ago(2 * DAY_S);

    let children: Vec<_> = (0..12)
        .map(|_| {
            fixture
                .bay(&repo)
                .arg("status")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn_busy()
        })
        .collect();
    for mut child in children {
        child.wait().unwrap();
    }
    assert!(checked_at(&fixture).unwrap() > due + DAY_S);
    eventually("one background check ran", || {
        fixture.curl_calls("releases-1") >= 1
    });
    // Any second check would have started by now.
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(fixture.curl_calls("releases-1"), 1, "one claim, one check");
}

#[test]
fn a_failed_install_after_a_background_update_is_recorded_for_doctor() {
    let fixture = Fixture::new();
    let build = Build {
        install_fails: true,
        ..Build::default()
    };
    fixture.release_with(NEWER, NEWER, None, &build);
    fixture.list(&[(NEWER, false)]);
    let repo = fixture.repo("");

    let _ = doctor(&fixture, &repo, false);
    eventually("the failed install is recorded", || {
        fixture
            .state()
            .is_some_and(|state| state["install_error"].is_string())
    });
    // The binary is the new one; doctor (now the fixture script) is not asked. The row is read
    // through the build's doctor against the same state file.
    let out = fixture
        .bay_at(std::path::Path::new(env!("CARGO_BIN_EXE_bay")), &repo)
        .args(["--json", "doctor"])
        .env("BAY_DISABLE_AUTOUPDATE", "1")
        .output_busy();
    let result: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let row = result["detail"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "bay")
        .unwrap()
        .clone();
    assert!(row["install_error"].is_string(), "{row}");
    assert_eq!(row["recovery"], "run bay install", "{row}");
}

#[test]
fn a_machine_install_never_touches_the_repository() {
    let fixture = Fixture::new();
    let repo = fixture.repo("[agent]\nproduct = \"claude\"\n");
    let out = fixture
        .bay(&repo)
        .args(["--json", "install", "--machine"])
        .output_busy();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(!repo.join(".claude/settings.json").exists());
    let config = std::fs::read_to_string(repo.join(".git/config")).unwrap();
    assert!(!config.contains("bay."), "{config}");

    // The repository-level install does write them.
    let out = fixture.bay(&repo).args(["--json", "install"]).output_busy();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(repo.join(".claude/settings.json").exists());
}
