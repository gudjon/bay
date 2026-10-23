//! Where bay reads `.bay/config.toml` (ruling: config location, from the first landing of a
//! large repository through bay). Every setting comes from the config committed at the fetched
//! trunk tracking ref, `refs/remotes/<remote>/<trunk>`: the same from the main work tree, a
//! linked worktree and a view, and never from a work tree's file or a change's own commits.
//! The trunk is found first, without reading any file a change can write: the repository's
//! own git config `bay.main` and `bay.remote` (written by `bay install`), else the remote's
//! default branch (`refs/remotes/<remote>/HEAD`). No bootstrap, or no config at the trunk tip,
//! is `CONFIG_MISSING`: never a silent default. The shell deny list (G39) judges a push against
//! that same trunk.

#![cfg(unix)]

use crate::support;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use support::{TestBox, check_files, passing_result, result_json, row};

/// A repository whose main work tree is an old checkout of `main` with no `.bay/` (and whose
/// remote default branch is still `main`), and a linked worktree on `trunk`, the repository's
/// trunk, that committed and pushed `.bay/config.toml`. `bay install --main trunk` named it.
/// Returns (main work tree, linked worktree, trunk tip).
fn stale_main_and_a_linked_trunk(test_box: &TestBox) -> (PathBuf, PathBuf, String) {
    let repo = test_box.repo_any("repo");
    std::fs::remove_dir_all(repo.join(".bay")).unwrap();
    std::fs::write(repo.join("old.txt"), "old\n").unwrap();
    test_box.git_ok(&repo, &["add", "old.txt"]);
    test_box.git_ok(&repo, &["commit", "-q", "-m", "an old checkout"]);
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);
    let linked = test_box.path().join("linked");
    let path = linked.to_str().unwrap();
    test_box.git_ok(&repo, &["worktree", "add", "-q", "-b", "trunk", path]);
    std::fs::create_dir_all(linked.join("src")).unwrap();
    std::fs::write(linked.join("src/a.rs"), "fn a() {}\n").unwrap();
    TestBox::bay_config(&linked, "main = \"trunk\"\n");
    test_box.git_ok(&linked, &["add", "src", ".bay"]);
    test_box.git_ok(&linked, &["commit", "-q", "-m", "trunk"]);
    test_box.git_ok(&linked, &["push", "-q", "origin", "trunk"]);
    let tip = test_box.git_ok(&linked, &["rev-parse", "HEAD"]);
    let (code, installed) = result_json(test_box.bay(&repo).args(["install", "--main", "trunk"]));
    assert_eq!(code, 0, "{installed}");
    assert_eq!(test_box.git_ok(&repo, &["config", "bay.main"]), "trunk");
    assert_eq!(test_box.git_ok(&repo, &["config", "bay.remote"]), "origin");
    (repo, linked, tip)
}

fn pre_tool(test_box: &TestBox, cwd: &Path, command: &str) -> (Option<i32>, String) {
    let payload = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "cwd": cwd,
    });
    let mut child = test_box
        .bay_process(cwd)
        .args(["hook", "tool.before"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn assert_config_missing(result: &serde_json::Value) {
    assert_eq!(result["decision"], "blocked", "{result}");
    assert_eq!(result["reason"], "CONFIG_MISSING", "{result}");
    assert_eq!(result["allowed"], serde_json::json!(["read"]), "{result}");
    assert_eq!(result["retry_when"], "after_setup", "{result}");
}

#[test]
fn a_stale_main_work_tree_and_a_trunk_with_a_config_work_from_a_linked_worktree() {
    let test_box = TestBox::new();
    let (repo, linked, tip) = stale_main_and_a_linked_trunk(&test_box);
    assert!(
        !repo.join(".bay").exists(),
        "the main work tree has no .bay/"
    );

    let started = test_box.bay_ok(&linked, &["start", "CARD-1", "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    assert_eq!(
        test_box.git_ok(&view, &["rev-parse", "HEAD"]),
        tip,
        "the view is based on the trunk bay.main names"
    );
    assert!(view.join("src/a.rs").exists());

    // The view and the main work tree read the same config as the start that made the view.
    for dir in [&view, &repo, &linked] {
        let (code, status) = result_json(test_box.bay(dir).args(["status"]));
        assert_eq!(code, 0, "{}: {status}", dir.display());
    }
}

#[test]
fn a_draft_config_changes_nothing_uncommitted_or_committed_in_the_change() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let check = check_files("", &passing_result(1), 1, 60);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &check);
    // An executor that leaves a mark if bay ever submits to it.
    let mark = test_box.path().join("decoy-executor-ran");
    let decoy = test_box.path().join("decoy-executor");
    support::write_executable(
        &decoy,
        &format!("#!/bin/sh\ntouch {}\nexit 1\n", mark.display()),
    );
    let draft = format!(
        "main = \"elsewhere\"\n\n[policy]\nevidence_reuse = \"exact-inputs\"\n\n[runner]\ncommand = \"{}\"\n",
        decoy.display()
    );
    // Uncommitted in the main work tree.
    TestBox::bay_config(&repo, &draft);
    let started = test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "src/", ".bay/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    // And in the change's own draft, which landing commits into the change.
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    TestBox::bay_config(&view, &draft);
    let (code, status) = result_json(test_box.bay(&view).args(["status"]));
    assert_eq!(code, 0, "the draft's trunk was used: {status}");

    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert!(
        !mark.exists(),
        "bay submitted to the executor the draft names"
    );
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}

#[test]
fn no_config_at_the_trunk_tip_is_config_missing() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    // The trunk has no config; the work tree's file (uncommitted) does not count.
    std::fs::remove_dir_all(repo.join(".bay")).unwrap();
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    TestBox::bay_config(&repo, "main = \"main\"\n");

    for args in [
        &["start", "CARD-1", "--paths", "src/"][..],
        &["status"][..],
        &["status", "--since"][..],
        &["merge", "c-none"][..],
    ] {
        let (code, result) = result_json(test_box.bay(&repo).args(args));
        assert_eq!(code, 10, "bay {args:?}: {result}");
        assert_config_missing(&result);
        assert_eq!(
            result["detail"]["ref"], "refs/remotes/origin/main",
            "{result}"
        );
        assert_eq!(
            result["detail"]["bootstrap"], "refs/remotes/origin/HEAD",
            "{result}"
        );
    }

    // Commands that need no config keep working, and doctor names what is missing.
    let (code, doctor) = result_json(test_box.bay(&repo).args(["doctor"]));
    assert!(code == 0 || code == 10, "{doctor}");
    assert_ne!(doctor["reason"], "CONFIG_MISSING", "{doctor}");
    assert_eq!(row(&doctor, "config")["state"], "missing", "{doctor}");
    let version = test_box.bay(&repo).arg("--version").output().unwrap();
    assert!(version.status.success());

    // With no bootstrap at all, bay names what it looked for.
    test_box.git_ok(
        &repo,
        &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"],
    );
    let (code, result) = result_json(test_box.bay(&repo).args(["status"]));
    assert_eq!(code, 10, "{result}");
    assert_config_missing(&result);
    assert_eq!(
        result["detail"]["missing"],
        serde_json::json!(["git config bay.main", "refs/remotes/origin/HEAD"]),
        "{result}"
    );
}

#[test]
fn a_command_that_fetches_reads_the_config_at_the_tip_it_fetched() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    test_box.bay_ok(&repo, &["status"]);
    // Elsewhere, the config leaves trunk; this repository's tracking ref still has it.
    let other = test_box.clone_of(&remote, "other");
    test_box.git_ok(&other, &["rm", "-q", ".bay/config.toml"]);
    test_box.git_ok(&other, &["commit", "-q", "-m", "no config"]);
    test_box.git_ok(&other, &["push", "-q", "origin", "main"]);
    let (code, result) = result_json(test_box.bay(&repo).args(["status"]));
    assert_eq!(code, 10, "{result}");
    assert_config_missing(&result);
}

#[test]
fn before_the_first_fetch_a_command_fetches_the_bootstrap_trunk() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let tip = test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    // A repository that names the remote's default branch but never fetched it.
    let fresh = test_box.repo_without_remote_any("fresh");
    test_box.add_origin(&fresh, &remote);
    let ls = test_box
        .git(&fresh)
        .args(["rev-parse", "-q", "--verify", "refs/remotes/origin/main"])
        .output()
        .unwrap();
    assert!(!ls.status.success(), "the trunk was never fetched");
    // `share` reads the config without fetching trunk itself: the first read fetched it.
    let (_, result) = result_json(test_box.bay(&fresh).args(["share", "c-none"]));
    assert_ne!(result["reason"], "CONFIG_MISSING", "{result}");
    assert_eq!(
        test_box.git_ok(&fresh, &["rev-parse", "refs/remotes/origin/main"]),
        tip
    );
}

#[test]
fn a_trunk_config_naming_another_trunk_is_refused() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(
        &repo,
        &[
            ("src/a.rs", "fn a() {}\n"),
            (".bay/config.toml", "main = \"other\"\n"),
        ],
    );
    let (code, result) = result_json(test_box.bay(&repo).args(["status"]));
    assert_ne!(code, 0, "{result}");
    assert!(
        result.to_string().contains("names main \\\"other\\\""),
        "{result}"
    );
}

#[test]
fn the_trunk_push_denial_holds_with_a_decoy_trunk_in_the_draft() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    // A decoy trunk on the main work tree's disk, and committed on a linked worktree's branch.
    TestBox::bay_config(&repo, "main = \"decoy\"\n");
    let linked = test_box.path().join("linked");
    let path = linked.to_str().unwrap();
    test_box.git_ok(&repo, &["worktree", "add", "-q", "-b", "topic", path]);
    TestBox::bay_config(&linked, "main = \"decoy\"\n");
    test_box.git_ok(&linked, &["commit", "-q", "-am", "decoy"]);
    for cwd in [&repo, &linked] {
        for command in [
            "git push origin HEAD:main",
            "git push origin topic:refs/heads/main",
        ] {
            let (code, reason) = pre_tool(&test_box, cwd, command);
            assert_eq!(code, Some(2), "{command} in {}: {reason}", cwd.display());
            assert!(reason.contains("trunk (main)"), "{reason}");
        }
        let (code, reason) = pre_tool(&test_box, cwd, "git push origin topic");
        assert_eq!(code, Some(0), "{reason}");
    }

    // A repository that names no trunk denies every push that could be of trunk.
    test_box.git_ok(
        &repo,
        &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"],
    );
    let (code, reason) = pre_tool(&test_box, &linked, "git push origin topic");
    assert_eq!(code, Some(2), "{reason}");
    assert!(reason.contains("CONFIG_MISSING"), "{reason}");
    let (code, reason) = pre_tool(
        &test_box,
        &linked,
        "git push origin refs/bay/changes/abcd2345",
    );
    assert_eq!(code, Some(0), "{reason}");
}
