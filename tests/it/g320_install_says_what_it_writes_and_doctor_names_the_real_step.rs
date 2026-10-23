//! G320 (learned in a fleet rollout, 2026-10-22): bay says what an install writes, and doctor
//! names the step that really puts hooks in place.
//!
//! An operator refused a safe `bay install --machine`, believing it adds blocking hooks, and that
//! held a fleet's rollout. So `--machine`'s result says what it writes (the binary and the skill)
//! and what it never writes (`never`: hook entries, a repository's agent settings and Git
//! config), and its `say` sentence says so plainly. And when a repository tracks its agent
//! settings file, doctor's recovery for missing hook entries names landing the change that
//! commits them, not "run install again", which would only leave an uncommitted edit.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/install.rs` `run`: drop `detail["never"] = ...` (the result names nothing it never writes).
//! - `src/result.rs` `ok_say`: drop the `scope == "machine"` branch (the sentence says "Done").
//! - `src/doctor.rs` `hooks_recovery`: make `tracked` `false` (a tracked file's recovery says
//!   "run install again").

// `bay install` and doctor run with a fake `claude` on PATH (a shell script), so Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, result_json, row};

const CLAUDE_ADAPTER: &str = "main = \"main\"\n\n[agent]\nproduct = \"claude\"\n";

#[test]
fn install_machine_says_it_writes_no_hooks() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.fake_claude(Some("2.1.302"));
    let installed = test_box.bay_ok(&repo, &["install", "--machine"]);
    let never = installed["detail"]["never"]
        .as_array()
        .unwrap_or_else(|| panic!("no detail.never: {installed}"));
    assert!(
        never.iter().any(|item| item == "hook entries"),
        "{installed}"
    );
    let say = installed["say"].as_str().unwrap_or_default();
    assert!(
        say.contains("safety settings come with the project"),
        "{installed}"
    );
    assert!(
        !repo.join(".claude/settings.json").exists(),
        "--machine wrote a repository's settings"
    );
}

#[test]
fn a_tracked_settings_file_without_entries_names_landing_the_change() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));
    // The repository tracks a settings file that holds no bay entry yet.
    std::fs::create_dir_all(repo.join(".claude")).unwrap();
    std::fs::write(repo.join(".claude/settings.json"), "{}\n").unwrap();
    test_box.git_ok(&repo, &["add", ".claude/settings.json"]);
    test_box.git_ok(&repo, &["commit", "-q", "-m", "settings"]);

    let (_, doctor) = result_json(test_box.bay(&repo).arg("doctor"));
    let hooks = row(&doctor, "hooks");
    assert_eq!(hooks["state"], "red", "{doctor}");
    let recovery = hooks["recovery"].as_str().unwrap_or_default();
    assert!(recovery.contains("is tracked"), "{hooks}");
    assert!(recovery.contains("merge the change"), "{hooks}");
}

#[test]
fn an_untracked_settings_file_still_says_run_install() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));
    let (_, doctor) = result_json(test_box.bay(&repo).arg("doctor"));
    let hooks = row(&doctor, "hooks");
    assert_eq!(hooks["state"], "red", "{doctor}");
    assert_eq!(hooks["recovery"], "run install again", "{hooks}");
}

#[test]
fn a_clone_install_says_its_hook_entries_still_have_to_land() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));
    let installed = test_box.bay_ok(&repo, &["install"]);
    assert_eq!(installed["detail"]["hooks_to_land"], true, "{installed}");
    let say = installed["say"].as_str().unwrap_or_default();
    assert!(
        say.contains("add the new settings file to main"),
        "{installed}"
    );
    assert!(!say.contains("nothing else is needed"), "{installed}");
}
