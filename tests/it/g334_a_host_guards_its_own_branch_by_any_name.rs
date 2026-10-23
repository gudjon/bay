//! G334 (BACKLOG 55, owner 2026-10-22: "we will never know what people will call the main
//! branch"): a bay host guards the branch it serves, whatever its name.
//!
//! `bay host <path> --main <branch>` serves that branch (`dev`, `master`, any name): git
//! config `bay.main`, and the host's `HEAD`. The host hook judges pushes to that branch. When
//! `bay.main` is not set (a host made before 0.3.3 wrote `bay.trunk` instead), the hook reads the
//! branch `HEAD` names, never a fixed `main`, so the host's own branch stays guarded.
//!
//! Guard to mutate: `src/host.rs` `served_branch`: drop the `HEAD` fallback (the hook guards
//! `main`, and a force push to `master` is taken).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::TestBox;

#[test]
fn a_host_for_master_guards_master_even_without_bay_main() {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    let (object_format, _) = support::formats();
    test_box.bay_ok(
        test_box.path(),
        &[
            "host",
            host.to_str().unwrap(),
            "--main",
            "master",
            "--object-format",
            &object_format,
        ],
    );
    assert_eq!(test_box.git_bare(&host, &["config", "bay.main"]), "master");
    // As a host made before 0.3.3 holds it: no bay.main.
    test_box.git_bare(&host, &["config", "--unset", "bay.main"]);

    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    test_box.git_ok(&repo, &["checkout", "-q", "-b", "master"]);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/a.rs"), "fn a() {}\n").unwrap();
    test_box.git_ok(&repo, &["add", "src/a.rs"]);
    test_box.git_ok(&repo, &["commit", "-q", "-m", "a"]);
    let pushed = test_box
        .git(&repo)
        .args(["push", "-q", "origin", "HEAD:master"])
        .output()
        .unwrap();
    assert!(pushed.status.success(), "{pushed:?}");

    test_box.git_ok(&repo, &["commit", "-q", "--amend", "-m", "rewritten"]);
    let forced = test_box
        .git(&repo)
        .args(["push", "-f", "origin", "HEAD:master"])
        .output()
        .unwrap();
    assert!(
        !forced.status.success(),
        "the host took a force push to its own branch: {forced:?}"
    );
    assert!(
        String::from_utf8_lossy(&forced.stderr).contains("non-fast-forward update is refused"),
        "{forced:?}"
    );
}
