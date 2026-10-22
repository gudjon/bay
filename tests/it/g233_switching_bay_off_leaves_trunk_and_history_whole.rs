//! G233. Switching bay off leaves trunk and history whole.
//!
//! bay can be switched off at any time, and nothing it kept is lost. After the runbook below a
//! plain `git push` fast-forwards trunk, every landed commit is still on trunk, a queued change
//! stays unmerged with its record, and every `refs/bay/*` ref and every evidence note under
//! `refs/notes/bay/*` is still there and reads with plain Git. No step deletes a ref.
//!
//! Runbook: switching bay off.
//!
//! 1. Stop every `bay runner run`. A queued change stays unmerged, and its record stays under
//!    `refs/bay/changes/`.
//! 2. Bay-hosted main: on the host, `git config --remove-section hook.bay-trunk;
//!    git config --remove-section hook.bay-scope; git config --unset bay.trunk`.
//! 3. Forge main: remove the branch protection that lets only the executor push. (In this test
//!    the forge is a plain bare remote, so there is nothing to do.)
//! 4. On each agent machine, remove bay's agent hook entries first: while they stay, they refuse
//!    a `git push` of main.
//! 5. Push one commit with plain git and confirm the remote takes it.
//!
//! What stays: every merge as a plain commit, evidence under `refs/notes/bay/evidence/` readable
//! with `git notes`, and change records and executor logs under `refs/bay/`.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use support::{TestBox, check_files, passing_result};

/// Step 2 of the runbook, on the host `host`.
fn switch_the_host_off(test_box: &TestBox, host: &Path) {
    let steps: [&[&str]; 3] = [
        &["config", "--remove-section", "hook.bay-trunk"],
        &["config", "--remove-section", "hook.bay-scope"],
        &["config", "--unset", "bay.trunk"],
    ];
    for step in steps {
        test_box.git_bare(host, step);
    }
}

/// Why step 4 comes first: while bay's agent hook entries stay, the hook they run refuses an
/// agent's `git push` of main in `repo` (Claude Code's deny exit, 2). Step 4 itself: no agent
/// settings file on this box holds a `bay hook` entry.
fn remove_the_agent_hooks(test_box: &TestBox, repo: &Path) {
    let payload = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": "git push origin main"},
        "cwd": repo,
    });
    let mut child = test_box
        .bay_process(repo)
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
    assert_eq!(
        out.status.code(),
        Some(2),
        "bay's hook allowed a push of main"
    );
    for dir in [repo, test_box.home.as_path()] {
        for settings in [".claude/settings.json", ".claude/settings.local.json"] {
            let text = std::fs::read_to_string(dir.join(settings)).unwrap_or_default();
            assert!(!text.contains("bay hook"), "{settings} holds bay's hook");
        }
    }
}

/// Starts a change on `src/`, writes `file` in its view and submits it with no executor running,
/// so it stays queued (step 1). Returns the change id.
fn queue(test_box: &TestBox, repo: &Path) -> String {
    let started = test_box.bay_ok(repo, &["start", "G233-queued", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/queued.rs"), "// queued\n").unwrap();
    let queued = test_box.bay_ok(&view, &["merge"]);
    assert_eq!(queued["decision"], "queued", "{queued}");
    id
}

/// The queued change `id` is not on trunk, and its record is still under `refs/bay/changes/`.
fn assert_still_queued(test_box: &TestBox, remote: &Path, before: &[String], id: &str) {
    assert!(
        !test_box.trunk_change_ids(remote).contains(&id.to_owned()),
        "the queued change {id} merged"
    );
    let record = format!("refs/bay/changes/{id} ");
    assert!(
        before.iter().any(|line| line.starts_with(&record)),
        "no record of {id}: {before:?}"
    );
    assert!(
        bay_refs(test_box, remote)
            .iter()
            .any(|line| line.starts_with(&record)),
        "the record of {id} is gone"
    );
}

/// Starts a change on `src/`, writes `file` in its view and lands it. Returns the landed commit.
fn land(test_box: &TestBox, repo: &Path, intent: &str, file: &str) -> String {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(file), format!("// {intent}\n")).unwrap();
    let landed = test_box.bay_ok(&view, &["merge", "--wait"]);
    assert_eq!(landed["decision"], "merged", "{landed}");
    landed["detail"]["commit"].as_str().unwrap().to_owned()
}

/// Every `refs/bay/*` and `refs/notes/bay/*` ref of `bare`, as `<name> <object>` lines.
fn bay_refs(test_box: &TestBox, bare: &Path) -> Vec<String> {
    test_box
        .git_bare(
            bare,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/bay/",
                "refs/notes/bay/",
            ],
        )
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A commit on a plain clone of `remote`, on top of its trunk. Returns (clone, commit).
fn plain_commit(test_box: &TestBox, remote: &Path) -> (PathBuf, String) {
    let clone = test_box.clone_of(remote, "plain");
    std::fs::write(clone.join("after-bay.md"), "pushed with plain git\n").unwrap();
    test_box.git_ok(&clone, &["add", "after-bay.md"]);
    test_box.git_ok(&clone, &["commit", "-q", "-m", "after bay"]);
    let commit = test_box.git_ok(&clone, &["rev-parse", "HEAD"]);
    (clone, commit)
}

/// A plain `git push` of `commit` from `clone` fast-forwards the trunk of `remote`.
fn plain_push_fast_forwards(test_box: &TestBox, remote: &Path, clone: &Path, commit: &str) {
    let tip = test_box.git_bare(remote, &["rev-parse", "main"]);
    test_box.git_ok(clone, &["push", "-q", "origin", "main"]);
    assert_eq!(test_box.git_bare(remote, &["rev-parse", "main"]), commit);
    assert_eq!(
        test_box.git_bare(remote, &["rev-parse", "main^"]),
        tip,
        "the plain push was not a fast-forward of trunk"
    );
}

/// Every landed commit is on trunk, every bay ref of `before` is still there unchanged, and
/// every evidence note reads with plain Git, on the remote and in a clone that fetched them as
/// step 6 does. Each landed commit's tree carries a note that names the commit.
fn assert_history_whole(test_box: &TestBox, remote: &Path, before: &[String], landed: &[String]) {
    for commit in landed {
        test_box.git_bare(remote, &["merge-base", "--is-ancestor", commit, "main"]);
    }
    let after = bay_refs(test_box, remote);
    let lost: Vec<&String> = before.iter().filter(|line| !after.contains(line)).collect();
    assert!(
        lost.is_empty(),
        "bay refs lost or moved by switching bay off: {lost:?}"
    );
    assert!(
        before.iter().any(|line| line.starts_with("refs/bay/")),
        "no refs/bay/* to keep: {before:?}"
    );
    let notes: Vec<&str> = before
        .iter()
        .filter_map(|line| line.split(' ').next())
        .filter(|name| name.starts_with("refs/notes/bay/"))
        .collect();
    assert!(!notes.is_empty(), "no evidence notes to keep: {before:?}");

    let _ = std::fs::remove_dir_all(test_box.path().join("reader"));
    let reader = test_box.clone_of(remote, "reader");
    test_box.git_ok(
        &reader,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/bay/*:refs/bay/*",
            "+refs/notes/bay/*:refs/notes/bay/*",
        ],
    );
    for commit in landed {
        let tree = format!("{commit}^{{tree}}");
        let mut named = false;
        for notes_ref in &notes {
            let notes_arg = format!("--ref={notes_ref}");
            for annotated in test_box
                .git_bare(remote, &["notes", &notes_arg, "list"])
                .lines()
            {
                let object = annotated.split(' ').nth(1).unwrap();
                let text = test_box.git_bare(remote, &["notes", &notes_arg, "show", object]);
                let records: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert!(records.is_array(), "{notes_ref} on {object}: {text}");
            }
            let shown = test_box
                .git(&reader)
                .args(["notes", &notes_arg, "show", &tree])
                .output()
                .unwrap();
            named |=
                shown.status.success() && String::from_utf8_lossy(&shown.stdout).contains(commit);
        }
        assert!(named, "no evidence note names landed commit {commit}");
    }
    let fetched: Vec<String> = test_box
        .git_ok(
            &reader,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/bay/",
                "refs/notes/bay/",
            ],
        )
        .lines()
        .map(str::to_owned)
        .collect();
    let unread: Vec<&String> = before
        .iter()
        .filter(|line| !fetched.contains(line))
        .collect();
    assert!(
        unread.is_empty(),
        "bay refs a clone cannot fetch with plain git: {unread:?}"
    );
}

#[test]
fn on_a_bay_hosted_trunk_the_runbook_switches_bay_off() {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    // With a key registry the scoped hook is on: from here only executors publish trunk.
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    let landed = [
        land(&test_box, &repo, "G233-1", "src/one.rs"),
        land(&test_box, &repo, "G233-2", "src/two.rs"),
    ];
    let queued = queue(&test_box, &repo);
    let before = bay_refs(&test_box, &host);

    // While bay is on, a plain push of trunk is refused at the host.
    let (clone, commit) = plain_commit(&test_box, &host);
    let out = test_box
        .git(&clone)
        .args(["push", "origin", "main"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "bay's host accepted a plain push");

    // Step 1: no executor runs. Step 2.
    switch_the_host_off(&test_box, &host);
    let hooks = test_box
        .git(test_box.path())
        .arg(format!("--git-dir={}", host.display()))
        .args(["config", "--local", "--get-regexp", "^hook\\.|^bay\\."])
        .output()
        .unwrap();
    assert!(
        hooks.stdout.is_empty(),
        "bay host config is still set: {}",
        String::from_utf8_lossy(&hooks.stdout)
    );
    // Step 4, then step 5.
    remove_the_agent_hooks(&test_box, &repo);
    plain_push_fast_forwards(&test_box, &host, &clone, &commit);
    assert_history_whole(&test_box, &host, &before, &landed);
    assert_still_queued(&test_box, &host, &before, &queued);
}

#[test]
fn on_a_forge_switching_bay_off_is_to_stop_running_it() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let landed = [
        land(&test_box, &repo, "G233-1", "src/one.rs"),
        land(&test_box, &repo, "G233-2", "src/two.rs"),
    ];
    let queued = queue(&test_box, &repo);
    let before = bay_refs(&test_box, &remote);

    // Step 1: no executor runs. Step 3: a plain bare remote has no branch protection to remove.
    // Step 4, then step 5.
    remove_the_agent_hooks(&test_box, &repo);
    let (clone, commit) = plain_commit(&test_box, &remote);
    plain_push_fast_forwards(&test_box, &remote, &clone, &commit);
    assert_history_whole(&test_box, &remote, &before, &landed);
    assert_still_queued(&test_box, &remote, &before, &queued);
}
