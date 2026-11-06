//! G664 (2026-11-06: a scratch test ran `bay install` from a development build with the real
//! `HOME`, and every session on that machine ran that build until it was restored).
//!
//! An install by a development build refuses when another bay is installed, before it writes
//! anything. Installing where none is, or where the same build is, goes on. A released build
//! carries `BAY_RELEASE_VERSION` and is not refused; a test binary cannot be one.
//!
//! Guard to mutate: `src/install.rs` `development_over_installed` (the refusal goes, and the first
//! two cases fail); its comparison of the two binaries (the third fails).

// Each case replaces a program file with a script, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use support::{TestBox, result_json};

const RELEASED: &str = "#!/bin/sh\necho released\n";

fn target(test_box: &TestBox) -> PathBuf {
    test_box.home.join(".local/bin/bay")
}

fn skill(test_box: &TestBox) -> PathBuf {
    test_box.home.join(".claude/skills/bay/SKILL.md")
}

/// A box with `RELEASED` installed.
fn installed() -> (TestBox, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &support::unchecked_files(),
    );
    let path = target(&test_box);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, RELEASED).unwrap();
    (test_box, repo)
}

#[test]
fn an_install_leaves_the_installed_bay_and_its_skill_as_they_are() {
    let (test_box, repo) = installed();

    let (code, refused) = result_json(test_box.bay(&repo).arg("install"));

    assert_eq!(code, 20, "{refused}");
    assert_eq!(refused["decision"], "failed", "{refused}");
    let text = refused.to_string();
    assert!(text.contains("development build"), "{text}");
    assert!(text.contains(target(&test_box).to_str().unwrap()), "{text}");
    assert_eq!(
        std::fs::read_to_string(target(&test_box)).unwrap(),
        RELEASED
    );
    assert!(!skill(&test_box).exists(), "the skill was written");
}

#[test]
fn a_machine_install_refuses_too() {
    let (test_box, repo) = installed();

    let (code, refused) = result_json(test_box.bay(&repo).args(["install", "--machine"]));

    assert_eq!(code, 20, "{refused}");
    assert_eq!(
        std::fs::read_to_string(target(&test_box)).unwrap(),
        RELEASED
    );
}

#[test]
fn the_same_build_or_no_bay_goes_on() {
    let (test_box, repo) = installed();
    std::fs::remove_file(target(&test_box)).unwrap();

    let (code, first) = result_json(test_box.bay(&repo).args(["install", "--machine"]));
    assert_eq!(code, 0, "nothing installed: {first}");
    assert!(target(&test_box).is_file());

    let (code, again) = result_json(test_box.bay(&repo).args(["install", "--machine"]));
    assert_eq!(code, 0, "the same build: {again}");
}
