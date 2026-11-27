//! G933 (the scale plan § 5, its hook row: a fleet at volume measured each push's host hooks at
//! p99 357 ms, where a push that lands a batch moves seven refs and the `update` hook ran a bay
//! process for each of them).
//!
//! A host judges its trunk rule in its one `pre-receive` hook, once a push: trunk is never
//! deleted and moves only by fast-forward. A new host has no `update` hook, and a serving origin
//! removes the one an older host kept.
//!
//! Guards to mutate: drop the call of `judge_trunk` in `host::pre_receive_bounded` (a force push
//! of main is taken); drop the call of `drop_update_hook` in `origin::run_serve` (the older
//! host keeps its `update` hook).

// Each case runs shell scripts (the check adapter), so these run on Unix only.
#![cfg(unix)]

use std::path::Path;

use crate::support;

use support::{TestBox, check_files, formats, passing_result};

/// `hook.*` of the bare repository `bare`, one `<key> <value>` a line.
fn hooks(test_box: &TestBox, bare: &Path) -> String {
    let out = test_box
        .git(test_box.path())
        .arg(format!("--git-dir={}", bare.display()))
        .args(["config", "--get-regexp", "^hook\\."])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap()
}

/// Pushes `refspec` from `repo` to its origin: whether the push was taken, and git's words.
fn push(test_box: &TestBox, repo: &Path, refspec: &str) -> (bool, String) {
    let out = test_box
        .git(repo)
        .args(["push", "-q", "origin", refspec])
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Main of `repo` moved by a commit that does not hold the one before it: a rewrite.
fn rewritten_main(test_box: &TestBox, repo: &Path) -> String {
    let tree = test_box.git_ok(repo, &["rev-parse", "HEAD^{tree}"]);
    let parent = test_box.git_ok(repo, &["rev-parse", "HEAD~1"]);
    test_box.git_ok(
        repo,
        &["commit-tree", &tree, "-p", &parent, "-m", "rewrite"],
    )
}

#[test]
fn a_new_host_judges_its_trunk_in_its_one_pre_receive_hook() {
    // GIVEN a new host and a repository whose origin it is, with two commits on main
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
    std::fs::write(repo.join("src/b.rs"), "fn b() {}\n").unwrap();
    test_box.git_ok(&repo, &["add", "src/b.rs"]);
    test_box.git_ok(&repo, &["commit", "-q", "-m", "b"]);

    // THEN its hooks are one pre-receive hook
    let configured = hooks(&test_box, &host);
    assert!(
        configured.contains("hook.bay-scope.event pre-receive"),
        "{configured}"
    );
    assert!(!configured.contains("update"), "{configured}");

    // AND a fast-forward of main is taken
    let (taken, said) = push(&test_box, &repo, "main");
    assert!(taken, "{said}");

    // AND a rewrite of main is refused, and so is its deletion
    let rewrite = rewritten_main(&test_box, &repo);
    let (taken, said) = push(&test_box, &repo, &format!("+{rewrite}:refs/heads/main"));
    assert!(!taken, "a rewrite of main was taken");
    assert!(
        said.contains("non-fast-forward update is refused"),
        "{said}"
    );
    let (taken, said) = push(&test_box, &repo, ":refs/heads/main");
    assert!(!taken, "the deletion of main was taken");
    assert!(said.contains("deleting it is refused"), "{said}");
}

#[test]
fn a_serving_origin_removes_the_update_hook_an_older_host_kept() {
    // GIVEN an origin that keeps the update hook of a host made before G933
    let test_box = TestBox::new();
    let hosted = test_box.bare_any("hosted.git");
    let origin = test_box.path().join("origin.git");
    let (object_format, _) = formats();
    test_box.bay_ok(
        test_box.path(),
        &[
            "host",
            origin.to_str().unwrap(),
            "--object-format",
            &object_format,
            "--upstream",
            hosted.to_str().unwrap(),
        ],
    );
    let command = format!("'{}' host hook update", env!("CARGO_BIN_EXE_bay"));
    test_box.git_bare(&origin, &["config", "hook.bay-trunk.command", &command]);
    test_box.git_bare(&origin, &["config", "hook.bay-trunk.event", "update"]);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &hosted);
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );

    // WHEN it serves one pass
    test_box.bay_ok(
        test_box.path(),
        &["host", "serve", origin.to_str().unwrap(), "--once"],
    );

    // THEN the update hook is gone and the pre-receive hook stays
    let configured = hooks(&test_box, &origin);
    assert!(!configured.contains("bay-trunk"), "{configured}");
    assert!(
        configured.contains("hook.bay-scope.event pre-receive"),
        "{configured}"
    );
}
