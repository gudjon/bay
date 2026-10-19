//! G65. The skill lands where the agent product reads it.
//!
//! With `CLAUDE_CONFIG_DIR` set, install (and `install --machine`) writes the skill to
//! `$CLAUDE_CONFIG_DIR/skills/bay/` and doctor's skill row checks and names that path; unset or
//! empty, both use `$HOME/.claude/skills/bay/`. No other config directory is looked for.
//!
//! `ROADMAP` § 1; `DESIGN` § 15.2, § 15.3, § 15.4.

// The fake `claude` on PATH is a shell script, so these run on Unix only.
#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};

use support::{TestBox, result_json, row};

const CLAUDE_ADAPTER: &str = "[agent]\nproduct = \"claude\"\n";

fn source_skill() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/bay")
}

fn config_dir(test_box: &TestBox) -> PathBuf {
    let dir = test_box.path().join("agent-config");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn install_writes_the_skill_under_the_config_dir_and_doctor_checks_it_there() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));
    let dir = config_dir(&test_box);
    let expected = dir.join("skills/bay");

    for args in [&["install"][..], &["install", "--machine"][..]] {
        let (code, installed) = result_json(
            test_box
                .bay(&repo)
                .args(args)
                .env("CLAUDE_CONFIG_DIR", &dir),
        );
        assert_eq!(code, 0, "{args:?}: {installed}");
        assert_eq!(
            installed["detail"]["skill"],
            expected.display().to_string(),
            "{args:?}: {installed}"
        );
        assert_eq!(
            std::fs::read(expected.join("SKILL.md")).unwrap(),
            std::fs::read(source_skill().join("SKILL.md")).unwrap()
        );
        assert!(
            !test_box.home.join(".claude/skills").exists(),
            "{args:?}: install wrote under HOME"
        );

        let (code, result) = result_json(
            test_box
                .bay(&repo)
                .arg("doctor")
                .env("CLAUDE_CONFIG_DIR", &dir),
        );
        assert_eq!(code, 0, "{args:?}: {result}");
        let skill = row(&result, "skill");
        assert_eq!(skill["state"], "green", "{result}");
        assert_eq!(skill["path"], expected.display().to_string());
        std::fs::remove_dir_all(&expected).unwrap();
    }
}

/// The skill only under `$HOME/.claude` is not the skill a session with `CLAUDE_CONFIG_DIR` set
/// reads: doctor is red there and names the path it expected.
#[test]
fn a_skill_only_under_home_is_missing_when_the_config_dir_is_set() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));
    let (code, installed) = result_json(test_box.bay(&repo).arg("install"));
    assert_eq!(code, 0, "{installed}");
    assert!(test_box.home.join(".claude/skills/bay/SKILL.md").exists());
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");

    let dir = config_dir(&test_box);
    let expected = dir.join("skills/bay");
    let (code, result) = result_json(
        test_box
            .bay(&repo)
            .arg("doctor")
            .env("CLAUDE_CONFIG_DIR", &dir),
    );
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "SKILL_MISMATCH");
    let skill = row(&result, "skill");
    assert_eq!(skill["state"], "red");
    assert_eq!(skill["path"], expected.display().to_string());
    assert_eq!(skill["note"], "skill not installed");
    assert_eq!(skill["recovery"], "run install again");
    // Doctor wrote nothing at the expected path.
    assert!(!expected.exists());
}

/// An empty `CLAUDE_CONFIG_DIR` is unset: both install and doctor use `$HOME/.claude`.
#[test]
fn an_empty_config_dir_is_the_home_default() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let home_skill = test_box.home.join(".claude/skills/bay");
    let (code, installed) = result_json(
        test_box
            .bay(&repo)
            .arg("install")
            .env("CLAUDE_CONFIG_DIR", ""),
    );
    assert_eq!(code, 0, "{installed}");
    assert_eq!(
        installed["detail"]["skill"],
        home_skill.display().to_string()
    );
    let (code, result) = result_json(
        test_box
            .bay(&repo)
            .arg("doctor")
            .env("CLAUDE_CONFIG_DIR", ""),
    );
    assert_eq!(code, 0, "{result}");
    let skill = row(&result, "skill");
    assert_eq!(skill["state"], "green");
    assert_eq!(skill["path"], home_skill.display().to_string());
}
