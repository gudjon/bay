//! G319. The skill is in every Claude config dir on the machine.
//!
//! `bay install --machine` (and so `bay update`) and `bay install` write the skill into every
//! Claude config directory the machine has: `$HOME/.claude`, each `$HOME/.claude-*` that holds
//! `settings.json`, `.claude.json`, `projects/` or `skills/`, each running `claude` session's
//! `CLAUDE_CONFIG_DIR` under this `HOME` (Linux), and each value of the global Git config
//! `bay.claudeConfigDirs`; never a `$HOME/.claude-*` directory that is not a config directory.
//! `bay doctor`'s `skill` row checks each one, lists them in `dirs`, and is red
//! `SKILL_MISMATCH` naming in `mismatched` each directory whose copy is missing or differs.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/skill.rs`, `config_dirs`: `others.extend(home_config_dirs(home));` (dropped, the
//!   `.claude-two` copy is not written).
//! - `src/skill.rs`, `config_dirs`: `others.extend(named_config_dirs(home, git));` (dropped,
//!   the named directory's copy is not written).
//! - `src/skill.rs`, `home_config_dirs`: `&& CONFIG_MARKERS.iter().any(|marker| ...)` (dropped,
//!   `.claude-worktrees` gets a copy).
//! - `src/skill.rs`, `install_all`: `install(&path)?;` for each directory (only the first, the
//!   other copies are missing).
//! - `src/doctor.rs`, `skill_row`: `mismatched.push(dir.display().to_string());` under
//!   `Installed::Missing` when `required` (dropped, doctor stays green with a copy gone).

// The fake `claude` on PATH is a shell script, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, result_json, row};

const CLAUDE_ADAPTER: &str = "[agent]\nproduct = \"claude\"\n";

fn source_skill() -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/bay/SKILL.md")).unwrap()
}

fn skill_in(dir: &Path) -> PathBuf {
    dir.join("skills/bay")
}

#[test]
fn install_writes_every_config_dir_and_doctor_checks_each() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));

    let home_dir = test_box.home.join(".claude");
    let second = test_box.home.join(".claude-two");
    let not_config = test_box.home.join(".claude-worktrees");
    let named = test_box.path().join("named-config");
    for dir in [&home_dir, &second, &not_config, &named] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(second.join("settings.json"), "{}\n").unwrap();
    test_box.git_ok(
        &repo,
        &[
            "config",
            "--global",
            "--add",
            "bay.claudeConfigDirs",
            &named.display().to_string(),
        ],
    );
    let config_dirs = [&home_dir, &second, &named];

    let (code, installed) = result_json(test_box.bay(&repo).args(["install", "--machine"]));
    assert_eq!(code, 0, "{installed}");
    let written = installed["detail"]["skills"].as_array().unwrap();
    for dir in config_dirs {
        let skill = skill_in(dir);
        assert_eq!(
            std::fs::read(skill.join("SKILL.md")).unwrap(),
            source_skill(),
            "{}: {installed}",
            dir.display()
        );
        assert!(
            written.contains(&serde_json::json!(skill.display().to_string())),
            "{}: {installed}",
            dir.display()
        );
    }
    assert!(
        !not_config.join("skills").exists(),
        "a directory that is not a config directory got a copy: {installed}"
    );
    assert_eq!(written.len(), config_dirs.len(), "{installed}");

    // The repository's hooks, so that only the skill row can be red.
    let (code, installed) = result_json(test_box.bay(&repo).arg("install"));
    assert_eq!(code, 0, "{installed}");
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    let skill = row(&result, "skill");
    assert_eq!(skill["state"], "green", "{result}");
    assert_eq!(skill["dirs"].as_array().unwrap().len(), config_dirs.len());

    // One copy gone: red, naming that directory and no other.
    std::fs::remove_dir_all(skill_in(&second)).unwrap();
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "SKILL_MISMATCH", "{result}");
    let skill = row(&result, "skill");
    assert_eq!(skill["state"], "red", "{result}");
    assert_eq!(
        skill["mismatched"],
        serde_json::json!([second.display().to_string()]),
        "{result}"
    );
    assert_eq!(skill["recovery"], "run install again");
    let listed = skill["dirs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["dir"] == second.display().to_string())
        .unwrap_or_else(|| panic!("{result}"));
    assert_eq!(listed["state"], "missing", "{result}");

    // Install again: every copy back, doctor green.
    let (code, installed) = result_json(test_box.bay(&repo).args(["install", "--machine"]));
    assert_eq!(code, 0, "{installed}");
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    assert_eq!(row(&result, "skill")["state"], "green", "{result}");
}

/// A copy that differs in any listed directory is red on every box, even where the skill is
/// not required (no Claude adapter).
#[test]
fn a_differing_copy_in_another_config_dir_is_red() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let second = test_box.home.join(".claude-two");
    std::fs::create_dir_all(second.join("projects")).unwrap();
    let (code, installed) = result_json(test_box.bay(&repo).arg("install"));
    assert_eq!(code, 0, "{installed}");
    std::fs::write(skill_in(&second).join("SKILL.md"), "an older skill\n").unwrap();

    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "SKILL_MISMATCH", "{result}");
    let skill = row(&result, "skill");
    assert_eq!(skill["state"], "red", "{result}");
    assert_eq!(skill["required"], false);
    assert_eq!(skill["note"], "skill does not match this bay");
    assert_eq!(
        skill["mismatched"],
        serde_json::json!([second.display().to_string()]),
        "{result}"
    );
}
