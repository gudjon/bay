//! G49. `bay status --fleet` fails closed without a fetch, and doctor checks the installed
//! skill path.
//!
//! `ROADMAP` § 1; `DESIGN` § 11a, § 15.2, § 15.3, § 15.4.

mod support;

use std::path::{Path, PathBuf};

use support::{TestBox, result_json, row};

const CLAUDE_ADAPTER: &str = "[agent]\nproduct = \"claude\"\n";

/// The skill this bay version installs, as the source tree holds it.
fn source_skill() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/bay")
}

fn installed_skill(test_box: &TestBox) -> PathBuf {
    test_box.home.join(".claude/skills/bay")
}

#[test]
fn install_copies_the_skill_and_leaves_other_skills_alone() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let other = test_box.home.join(".claude/skills/other/SKILL.md");
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    std::fs::write(&other, "another skill\n").unwrap();

    let (code, installed) = result_json(test_box.bay(&repo).arg("install"));
    assert_eq!(code, 0, "{installed}");
    let copy = installed_skill(&test_box);
    assert_eq!(
        installed["detail"]["skill"],
        copy.display().to_string(),
        "{installed}"
    );
    assert_eq!(
        std::fs::read(copy.join("SKILL.md")).unwrap(),
        std::fs::read(source_skill().join("SKILL.md")).unwrap()
    );
    assert_eq!(std::fs::read_to_string(&other).unwrap(), "another skill\n");

    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    assert_eq!(row(&result, "skill")["state"], "green");
}

/// A copy that does not match this bay version is red on every box, with or without the
/// Claude adapter, and a second install replaces that same directory.
#[test]
fn a_mismatched_copy_is_red_until_install_runs_again() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let (code, installed) = result_json(test_box.bay(&repo).arg("install"));
    assert_eq!(code, 0, "{installed}");
    let copy = installed_skill(&test_box);
    std::fs::write(copy.join("SKILL.md"), "an older skill\n").unwrap();

    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "SKILL_MISMATCH");
    let skill = row(&result, "skill");
    assert_eq!(skill["state"], "red");
    assert_eq!(skill["path"], copy.display().to_string());
    assert_eq!(skill["recovery"], "run install again");
    // Doctor wrote nothing.
    assert_eq!(
        std::fs::read_to_string(copy.join("SKILL.md")).unwrap(),
        "an older skill\n"
    );

    // A file this version does not install is a mismatch too; install removes it.
    let (code, _) = result_json(test_box.bay(&repo).arg("install"));
    assert_eq!(code, 0);
    std::fs::write(copy.join("extra.md"), "not bay's\n").unwrap();
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "SKILL_MISMATCH");

    let (code, _) = result_json(test_box.bay(&repo).arg("install"));
    assert_eq!(code, 0);
    assert!(!copy.join("extra.md").exists());
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    assert_eq!(row(&result, "skill")["state"], "green");
}

/// Outside a repository the skill row is a machine row: a mismatch is still red.
#[test]
fn a_mismatched_copy_is_red_outside_a_repository() {
    let test_box = TestBox::new();
    let copy = installed_skill(&test_box);
    std::fs::create_dir_all(&copy).unwrap();
    std::fs::write(copy.join("SKILL.md"), "an older skill\n").unwrap();
    let (code, result) = result_json(test_box.bay(test_box.path()).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "SKILL_MISMATCH");
}

/// With the Claude adapter a missing copy is red; without it a missing copy is no required
/// failure. The consumer repository's own skills are never taken for the installed copy.
#[test]
fn a_missing_copy_is_red_only_with_the_claude_adapter() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    let skill = row(&result, "skill");
    assert_eq!(skill["state"], "missing");
    assert_eq!(skill["required"], false);

    TestBox::bay_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));
    for consumer in ["skills/bay", ".claude/skills/bay"] {
        let dir = repo.join(consumer);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(source_skill().join("SKILL.md"), dir.join("SKILL.md")).unwrap();
    }
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    let skill = row(&result, "skill");
    assert_eq!(skill["state"], "red", "{result}");
    assert_eq!(skill["required"], true);
    assert_eq!(skill["recovery"], "run install again");
    // Doctor does not write the copy, global Git config or user-global Claude settings.
    assert!(!installed_skill(&test_box).exists());
    assert!(!test_box.home.join(".claude/settings.json").exists());
    assert_eq!(
        std::fs::read_to_string(&test_box.global_config).unwrap(),
        ""
    );
}

/// Doctor does not skip a mismatch where the Claude adapter is configured either (§ 15.2: "red
/// on every box").
#[test]
fn a_mismatched_copy_is_red_with_the_claude_adapter() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    TestBox::bay_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));
    let (code, installed) = result_json(test_box.bay(&repo).arg("install"));
    assert_eq!(code, 0, "{installed}");
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    std::fs::write(
        installed_skill(&test_box).join("SKILL.md"),
        "an older skill\n",
    )
    .unwrap();
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "SKILL_MISMATCH");
    let skill = row(&result, "skill");
    assert_eq!(skill["state"], "red");
    assert_eq!(skill["required"], true);
    assert_eq!(skill["note"], "skill does not match this bay");
}

/// Something other than a directory at the skill path (a file, a dangling link) is a mismatch,
/// red with `SKILL_MISMATCH`, not an operation fault and not "missing"; install replaces it.
#[test]
fn a_non_directory_at_the_skill_path_is_a_mismatch_install_repairs() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let copy = installed_skill(&test_box);
    std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
    for dangling in [false, true] {
        if dangling {
            std::os::unix::fs::symlink(test_box.path().join("nowhere"), &copy).unwrap();
        } else {
            std::fs::write(&copy, "not a directory\n").unwrap();
        }
        let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
        assert_eq!(code, 10, "dangling={dangling}: {result}");
        assert_eq!(result["reason"], "SKILL_MISMATCH", "{result}");
        assert_eq!(row(&result, "skill")["state"], "red");

        let (code, installed) = result_json(test_box.bay(&repo).arg("install"));
        assert_eq!(code, 0, "dangling={dangling}: {installed}");
        let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
        assert_eq!(code, 0, "{result}");
        assert_eq!(row(&result, "skill")["state"], "green");
        std::fs::remove_dir_all(&copy).unwrap();
    }
}

/// Never fetched: `--fleet` is blocked with `REMOTE_NOT_FETCHED`, exit 10, and its detail does
/// not present the local cache as the fleet. It opens no SSH and starts no session.
#[test]
fn status_fleet_fails_closed_when_the_remote_was_never_fetched() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let ssh_marker = test_box.path().join("ssh-ran");
    let claude_marker = test_box.path().join("claude-ran");
    support::write_executable(
        &test_box.fakebin.join("ssh"),
        &format!("#!/bin/sh\ntouch '{}'\nexit 255\n", ssh_marker.display()),
    );
    support::write_executable(
        &test_box.fakebin.join("claude"),
        &format!("#!/bin/sh\ntouch '{}'\n", claude_marker.display()),
    );
    // The configured remote would need SSH; the fleet map must not reach for it.
    test_box.git_ok(
        &repo,
        &[
            "remote",
            "set-url",
            "origin",
            "ssh://fleet.invalid/repo.git",
        ],
    );

    let (code, result) = result_json(test_box.bay(&repo).args(["status", "--fleet"]));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["schema"], "bay.result/1");
    assert_eq!(result["decision"], "blocked");
    assert_eq!(result["reason"], "REMOTE_NOT_FETCHED");
    for key in ["machines", "actors", "talks", "sessions"] {
        assert!(result["detail"].get(key).is_none(), "{result}");
    }
    assert!(!ssh_marker.exists(), "status --fleet opened SSH");
    assert!(!claude_marker.exists(), "status --fleet started a session");
}

/// With no remote configured there is no fetch of it either.
#[test]
fn status_fleet_fails_closed_without_a_configured_remote() {
    let test_box = TestBox::new();
    let repo = test_box.repo_without_remote_any("repo");
    let (code, result) = result_json(test_box.bay(&repo).args(["status", "--fleet"]));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "REMOTE_NOT_FETCHED");
}

/// A trunk tracking ref is not a fetch of the fleet: a plain push writes it, and a fetch of the
/// trunk brings no `refs/bay/live`. `--fleet` still fails closed.
#[test]
fn status_fleet_fails_closed_after_a_push_or_a_trunk_fetch() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("README", "trunk\n")]);
    let tracking = test_box.git_ok(
        &repo,
        &["rev-parse", "--verify", "refs/remotes/origin/main"],
    );
    assert_ne!(tracking, "");
    let (code, result) = result_json(test_box.bay(&repo).args(["status", "--fleet"]));
    assert_eq!(code, 10, "after a push: {result}");
    assert_eq!(result["reason"], "REMOTE_NOT_FETCHED");
    test_box.git_ok(&repo, &["fetch", "-q", "origin"]);
    let (code, result) = result_json(test_box.bay(&repo).args(["status", "--fleet"]));
    assert_eq!(code, 10, "after a trunk fetch: {result}");
    assert_eq!(result["reason"], "REMOTE_NOT_FETCHED");
}

/// The record a fetch of the remote's `refs/bay/live/*` writes (`bay status` writes it; this
/// test stands in for that fetch). A record of another URL is a fetch of another remote, so
/// the cache is still not the fleet. A record of this remote passes the guard: the map of a
/// remote with no live refs lists no machines, and says the day-old fetch is behind.
#[test]
fn status_fleet_passes_the_guard_only_for_a_fetch_of_this_remote() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let url = test_box.git_ok(&repo, &["remote", "get-url", "origin"]);
    let record = repo.join(".git/bay/fleet/fetched.json");
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    let at = "2026-10-18T00:00:00Z";
    let other = serde_json::json!({"remote": "origin", "url": format!("{url}-other"), "at": at});
    for text in [other.to_string(), "{not json".to_owned()] {
        std::fs::write(&record, &text).unwrap();
        let (code, result) = result_json(test_box.bay(&repo).args(["status", "--fleet"]));
        assert_eq!(code, 10, "{text}: {result}");
        assert_eq!(result["reason"], "REMOTE_NOT_FETCHED");
    }
    let text = serde_json::json!({"remote": "origin", "url": url, "at": at}).to_string();
    std::fs::write(&record, text).unwrap();
    let (code, result) = result_json(test_box.bay(&repo).args(["status", "--fleet"]));
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["decision"], "ok");
    assert_eq!(
        result["detail"]["machines"],
        serde_json::json!([]),
        "{result}"
    );
    assert_eq!(result["detail"]["fetched_at"], at, "{result}");
    assert_eq!(result["detail"]["behind"], true, "{result}");
}

/// `bay status` is the fetch of the fleet refs: after it, `--fleet` no longer fails closed.
#[test]
fn bay_status_records_the_fetch_of_the_fleet_refs() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("README", "trunk\n")]);
    let (code, result) = result_json(test_box.bay(&repo).args(["status", "--fleet"]));
    assert_eq!(code, 10, "{result}");
    test_box.bay_ok(&repo, &["status"]);
    let (code, result) = result_json(test_box.bay(&repo).args(["status", "--fleet"]));
    assert_eq!(code, 0, "{result}");
    // Whether the remote moved since is unknown to a read that never asks it: never `false`.
    assert!(result["detail"]["behind"].is_null(), "{result}");
    assert_eq!(
        result["detail"]["sessions"],
        serde_json::json!([]),
        "{result}"
    );
}
