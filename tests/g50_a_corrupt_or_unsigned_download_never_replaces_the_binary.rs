//! G50. A corrupt or unsigned download never replaces the binary.
//!
//! `DESIGN` (the binary's lifecycle, `bay update`); `INTERFACES` § 2. The release is a local
//! fixture under a `file://` base (`BAY_TEST_RELEASE_URL`), signed by a throwaway key
//! (`BAY_TEST_RELEASE_KEY`). The bay under test is a copy, so the swap replaces the copy and
//! never the build. The real release key is never used.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

mod release_fixture;

use std::process::Command;

use release_fixture::RetryBusy as _;
use release_fixture::{Build, Fixture, assert_no_leftovers, run, target};

const NEWER: &str = "9.0.0";

#[test]
fn a_good_release_replaces_the_binary_and_runs_its_install() {
    let fixture = Fixture::new();
    fixture.release(NEWER, NEWER, None);
    // The stable channel's newest is on the list's second page, behind a pre-release.
    fixture.list(&[("9.1.0-rc.1", true), (NEWER, false)]);

    // --check reports and does nothing.
    let (code, checked) = fixture.update(fixture.path(), &["--check"]);
    assert_eq!(code, 0, "{checked}");
    assert_eq!(checked["detail"]["target"], NEWER);
    assert_eq!(checked["detail"]["due"], true);
    assert_eq!(checked["detail"]["updated"], false);
    fixture.assert_untouched();

    let (code, result) = fixture.update(fixture.path(), &[]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["schema"], "bay.result/1");
    assert_eq!(result["decision"], "ok");
    let detail = &result["detail"];
    assert_eq!(detail["installed"], bay::version::BAY);
    assert_eq!(detail["target"], NEWER);
    assert_eq!(detail["channel"], "stable");
    assert_eq!(detail["pinned"], serde_json::Value::Null);
    assert_eq!(detail["updated"], true);

    let version = run(Command::new(fixture.bin()).arg("--version"));
    assert_eq!(version.trim(), format!("bay {NEWER}"));
    // The new binary's install ran machine-level only: no repository is named.
    let args = std::fs::read_to_string(fixture.home().join(format!("installed-{NEWER}")))
        .expect("the new binary's install ran");
    assert!(
        args.contains("install") && args.contains("--machine"),
        "{args}"
    );
    assert!(!args.contains("-C"), "{args}");
    assert_no_leftovers(&fixture);
}

#[test]
fn a_corrupt_archive_is_refused_and_the_binary_is_unchanged() {
    let fixture = Fixture::new();
    let dir = fixture.release(NEWER, NEWER, None);
    fixture.list(&[(NEWER, false)]);
    // A well-formed archive that unpacks and prints the target, but is not the signed one.
    let stage = fixture.path().join("stage").join(NEWER);
    std::fs::write(
        stage.join("bay"),
        format!("#!/bin/sh\necho \"bay {NEWER}\"\n# not the signed build\n"),
    )
    .unwrap();
    run(Command::new("tar")
        .arg("-C")
        .arg(&stage)
        .arg("-czf")
        .arg(dir.join(format!("bay-{}.tar.gz", target())))
        .arg("bay"));

    let (code, result) = fixture.update(fixture.path(), &[]);
    assert_eq!(code, 20, "{result}");
    assert_eq!(result["reason"], "UPDATE_UNVERIFIED");
    assert_eq!(result["retry_when"], "release_republished");
    fixture.assert_untouched();
}

#[test]
fn a_bad_signature_is_refused_and_the_binary_is_unchanged() {
    let fixture = Fixture::new();
    let other = Fixture::keygen(fixture.path(), "other");
    fixture.release(NEWER, NEWER, Some(&other));
    fixture.list(&[(NEWER, false)]);

    let (code, result) = fixture.update(fixture.path(), &[]);
    assert_eq!(code, 20, "{result}");
    assert_eq!(result["reason"], "UPDATE_UNVERIFIED");
    fixture.assert_untouched();
}

#[test]
fn a_signed_older_release_under_a_newer_tag_is_refused() {
    // A replay: the signed sums are consistent and the binary even prints the tag, but the
    // signed VERSION names another release.
    let fixture = Fixture::new();
    fixture.release(NEWER, "8.0.0", None);
    fixture.list(&[(NEWER, false)]);

    let (code, result) = fixture.update(fixture.path(), &[]);
    assert_eq!(code, 20, "{result}");
    assert_eq!(result["reason"], "UPDATE_UNVERIFIED");
    fixture.assert_untouched();
}

#[test]
fn a_downgrade_is_refused_pinned() {
    let fixture = Fixture::new();
    fixture.release("0.0.1", "0.0.1", None);
    fixture.list(&[("0.0.1", false)]);

    let (code, result) = fixture.update(fixture.path(), &["--version", "0.0.1"]);
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "UPDATE_PINNED");
    assert_eq!(result["allowed"], serde_json::json!(["read"]));
    assert_eq!(result["retry_when"], "pin_moved");
    fixture.assert_untouched();

    // The channel's newest being older is a downgrade too.
    let (code, result) = fixture.update(fixture.path(), &[]);
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "UPDATE_PINNED");
    fixture.assert_untouched();
}

#[test]
fn a_target_off_the_pin_is_refused_pinned() {
    let fixture = Fixture::new();
    fixture.release(NEWER, NEWER, None);
    fixture.release("9.1.0", "9.1.0", None);
    fixture.list(&[("9.1.0", false), (NEWER, false)]);
    let repo = fixture.repo("");
    fixture.box_config("bay.updateVersion", NEWER);

    let (code, result) = fixture.update(&repo, &["--version", "9.1.0"]);
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "UPDATE_PINNED");
    assert_eq!(result["detail"]["pinned"], NEWER);
    fixture.assert_untouched();

    // The pin wins over the channel's newest.
    let (code, result) = fixture.update(&repo, &["--check"]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["detail"]["target"], NEWER);
    fixture.assert_untouched();
}

#[test]
fn the_installed_version_is_ok_and_nothing_is_done() {
    let fixture = Fixture::new();
    let (code, result) = fixture.update(fixture.path(), &["--version", bay::version::BAY]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["decision"], "ok");
    assert_eq!(result["detail"]["updated"], false);
    assert_eq!(result["detail"]["target"], bay::version::BAY);
    fixture.assert_untouched();
}

#[test]
fn a_repository_never_moves_the_box() {
    // Pins, channels and the auto switch are box-level: a cloned repository's [update] keys
    // change nothing.
    let fixture = Fixture::new();
    fixture.release(NEWER, NEWER, None);
    fixture.list(&[("9.1.0-rc.1", true), (NEWER, false)]);
    let repo = fixture.repo("[update]\nversion = \"0.0.1\"\nchannel = \"latest\"\nauto = false\n");

    let (code, result) = fixture.update(&repo, &["--check"]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["detail"]["target"], NEWER);
    assert_eq!(result["detail"]["channel"], "stable");
    assert_eq!(result["detail"]["pinned"], serde_json::Value::Null);

    // The box's own channel does move it.
    fixture.box_config("bay.updateChannel", "latest");
    let (code, result) = fixture.update(&repo, &["--check"]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["detail"]["target"], "9.1.0-rc.1");
    fixture.assert_untouched();
}

#[test]
fn an_altered_version_file_is_refused_even_when_it_names_the_target() {
    // VERSION still says the target, but it is not the file the signed sums cover.
    let fixture = Fixture::new();
    let dir = fixture.release(NEWER, NEWER, None);
    fixture.list(&[(NEWER, false)]);
    std::fs::write(dir.join("VERSION"), format!("{NEWER} \n")).unwrap();

    let (code, result) = fixture.update(fixture.path(), &[]);
    assert_eq!(code, 20, "{result}");
    assert_eq!(result["reason"], "UPDATE_UNVERIFIED");
    fixture.assert_untouched();
}

#[test]
fn a_signed_binary_that_runs_as_another_version_is_refused() {
    let fixture = Fixture::new();
    let build = Build {
        prints: Some("bay 8.9.9".to_owned()),
        ..Build::default()
    };
    fixture.release_with(NEWER, NEWER, None, &build);
    fixture.list(&[(NEWER, false)]);

    let (code, result) = fixture.update(fixture.path(), &[]);
    assert_eq!(code, 20, "{result}");
    assert_eq!(result["reason"], "UPDATE_UNVERIFIED");
    fixture.assert_untouched();
}

#[test]
fn a_bay_that_is_not_a_regular_file_is_refused() {
    let fixture = Fixture::new();
    let build = Build {
        symlink: true,
        ..Build::default()
    };
    fixture.release_with(NEWER, NEWER, None, &build);
    fixture.list(&[(NEWER, false)]);

    let (code, result) = fixture.update(fixture.path(), &[]);
    assert_eq!(code, 20, "{result}");
    assert_eq!(result["reason"], "UPDATE_UNVERIFIED");
    assert!(
        !std::fs::symlink_metadata(fixture.bin())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    fixture.assert_untouched();
}

#[test]
fn no_release_on_the_channel_or_for_the_platform_is_unavailable() {
    let fixture = Fixture::new();
    fixture.list(&[("9.1.0-rc.1", true)]);
    let (code, result) = fixture.update(fixture.path(), &[]);
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "UPDATE_UNAVAILABLE");
    assert_eq!(result["retry_when"], "release_reachable");
    fixture.assert_untouched();

    let build = Build {
        archive: Some("bay-other-platform.tar.gz".to_owned()),
        ..Build::default()
    };
    fixture.release_with(NEWER, NEWER, None, &build);
    fixture.list(&[(NEWER, false)]);
    let (code, result) = fixture.update(fixture.path(), &[]);
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "UPDATE_UNAVAILABLE");
    fixture.assert_untouched();
}

#[test]
fn a_bay_run_from_elsewhere_updates_the_installed_path_only() {
    let fixture = Fixture::new();
    fixture.release(NEWER, NEWER, None);
    fixture.list(&[(NEWER, false)]);
    let elsewhere = fixture.path().join("elsewhere/bay");
    std::fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_bay"), &elsewhere).unwrap();

    let out = fixture
        .bay_at(&elsewhere, fixture.path())
        .arg("update")
        .output_busy();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        std::fs::read(&elsewhere).unwrap() == std::fs::read(env!("CARGO_BIN_EXE_bay")).unwrap(),
        "the running copy elsewhere is untouched"
    );
    let version = run(Command::new(fixture.bin()).arg("--version"));
    assert_eq!(version.trim(), format!("bay {NEWER}"));
    assert_no_leftovers(&fixture);
}
