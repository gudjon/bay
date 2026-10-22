//! G234. The landing guarantees hold with agent hooks off.
//!
//! bay's agent hooks are an early warning, never a guard a landing rests on. With no hook of
//! any agent product in any settings file the agent reads, and no hook ever run, the core of
//! each landing guarantee still holds: what lands is exactly what was checked (G1), a rewound
//! trunk is never forced (G2), unknown is never pass (G3), and on a bay-hosted trunk an
//! executor publishes, executor-signed, while an agent's own push of trunk is refused (G33).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{EXECUTOR, TestBox, check_files, passing_result, result_json};

/// The agent settings files that could hold a hook, relative to a repository, a view or HOME.
const SETTINGS: &[&str] = &[".claude/settings.json", ".claude/settings.local.json"];

/// No agent hook is installed for `dirs` or for the box's HOME, and none has run: no settings
/// file exists, and no repository holds a record only a hook writes (the session work records
/// and the hook's own session state).
fn assert_hooks_off(test_box: &TestBox, dirs: &[&Path]) {
    for dir in dirs.iter().copied().chain([test_box.home.as_path()]) {
        for settings in SETTINGS {
            let path = dir.join(settings);
            assert!(!path.exists(), "an agent settings file: {}", path.display());
        }
    }
    for dir in dirs {
        let common = PathBuf::from(test_box.git_ok(
            dir,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ));
        for record in ["bay/sessions", "bay/session-state"] {
            let path = common.join(record);
            assert!(!path.exists(), "a hook wrote {}", path.display());
        }
    }
}

/// Starts a change on `src/` and edits it, with hooks off before and after.
fn start(test_box: &TestBox, repo: &Path) -> (String, PathBuf) {
    assert_hooks_off(test_box, &[repo]);
    let (id, view) = test_box.start_and_edit(repo);
    assert_hooks_off(test_box, &[repo, &view]);
    (id, view)
}

#[test]
fn what_lands_is_exactly_what_was_checked_with_hooks_off() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let other = test_box.path().join("other");
    let marker = test_box.path().join("moved-once");
    // The first check moves trunk from another clone before it answers (as in G1).
    let before = test_box.relay(&format!(
        "if [ ! -f '{marker}' ]; then touch '{marker}'; ( cd '{other}' && echo moved > docs/moved.md && git add docs/moved.md && git -c user.name=t -c user.email=t@t commit -q -m 'trunk moves' && git push -q origin main ); fi",
        marker = marker.display(),
        other = other.display()
    ));
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("docs/b.md", "b\n")],
        &check_files(&before, &passing_result(2), 2, 60),
    );
    test_box.clone_of(&remote, "other");
    let (id, view) = start(&test_box, &repo);

    let (code, result) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["decision"], "merged");
    assert_hooks_off(&test_box, &[&repo, &view]);
    let published = result["detail"]["commit"].as_str().unwrap().to_owned();

    // What landed is the second candidate, built on the moved trunk and checked itself.
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", "main"]),
        published
    );
    assert_eq!(
        test_box.git_bare(&remote, &["log", "-1", "--format=%s", "main^"]),
        "trunk moves"
    );
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id.clone()]);
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let change = &status["detail"]["changes"][0];
    assert_eq!(change["state"], "landed");
    assert_eq!(change["operation"]["attempts"], 2);
    let tree = test_box.git_bare(&remote, &["rev-parse", "main^{tree}"]);
    let note = test_box.git_bare(
        &remote,
        &[
            "notes",
            "--ref=refs/notes/bay/evidence/local",
            "show",
            &tree,
        ],
    );
    assert!(note.contains(&published), "{note}");
    assert!(note.contains("\"verdict\": \"pass\""), "{note}");
}

#[test]
fn a_rewound_trunk_is_never_forced_with_hooks_off() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let first = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("src/a.rs"), "fn a() { 2 }\n").unwrap();
    test_box.git_ok(&repo, &["commit", "-q", "-am", "second"]);
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);
    let (_, view) = start(&test_box, &repo);
    test_box.git_bare(&remote, &["update-ref", "refs/heads/main", &first]);

    let (code, result) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 20, "{result}");
    assert!(
        result["detail"].as_str().unwrap().contains("rewound"),
        "{result}"
    );
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), first);
    assert_hooks_off(&test_box, &[&repo, &view]);
}

#[test]
fn unknown_is_never_pass_with_hooks_off() {
    // (what the check does first, the result it prints): each is unknown, never a pass.
    let cases = [
        ("true", passing_result(0)),
        ("exit 3", "true".to_owned()),
        ("echo all good; exit 0", "true".to_owned()),
        (
            "commit=0000000000000000000000000000000000000000",
            passing_result(1),
        ),
    ];
    for (before, printed) in &cases {
        let test_box = TestBox::new();
        let repo = test_box.repo_any("repo");
        let remote = test_box.path().join("repo-remote.git");
        let trunk = test_box.seed_with_check(
            &repo,
            &[("src/a.rs", "fn a() {}\n")],
            &check_files(before, printed, 1, 60),
        );
        let (_, view) = start(&test_box, &repo);
        let (code, result) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
        assert_eq!(code, 10, "{before}: {result}");
        assert_eq!(result["reason"], "CHECK_UNKNOWN", "{before}: {result}");
        assert_eq!(
            test_box.git_bare(&remote, &["rev-parse", "main"]),
            trunk,
            "{before}: trunk moved"
        );
        assert_hooks_off(&test_box, &[&repo, &view]);
    }
}

#[test]
fn an_executor_publishes_and_an_agent_cannot_push_trunk_with_hooks_off() {
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
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    let (id, view) = start(&test_box, &repo);

    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert_hooks_off(&test_box, &[&repo, &view]);
    assert_eq!(test_box.trunk_change_ids(&host), vec![id]);
    let tip = test_box.git_bare(&host, &["rev-parse", "main"]);
    assert!(test_box.executor_signed(&host, &keys, &tip));
    assert_eq!(
        test_box.git_bare(&host, &["log", "-1", "--format=%cn", "main"]),
        EXECUTOR
    );

    // The agent's own commit on trunk, pushed past bay, is refused at the host.
    let clone = test_box.clone_of(&host, "agent-clone");
    std::fs::write(clone.join("past-bay.md"), "no landing\n").unwrap();
    test_box.git_ok(&clone, &["add", "past-bay.md"]);
    test_box.git_ok(&clone, &["commit", "-q", "-m", "past bay"]);
    let out = test_box
        .git(&clone)
        .args(["push", "origin", "main"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "the host accepted an agent's push");
    assert!(
        stderr.contains("trunk takes only commits signed by an executor key"),
        "{stderr}"
    );
    assert_eq!(test_box.git_bare(&host, &["rev-parse", "main"]), tip);
}
