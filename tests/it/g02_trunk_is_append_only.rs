//! G2. Trunk is append-only.
//!
//! A rewound remote is detected before publication; bay never forces; with the host rule on, a
//! force push is refused.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_rewound_remote_is_detected_before_publication() {
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
    let (_, view) = test_box.start_and_edit(&repo);

    // Someone rewinds the remote trunk to an ancestor of what the change saw.
    test_box.git_bare(&remote, &["update-ref", "refs/heads/main", &first]);

    let (code, result) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 20, "{result}");
    assert!(
        result["detail"].as_str().unwrap().contains("rewound"),
        "{result}"
    );
    // bay did not force anything onto the rewound trunk.
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), first);
}

#[test]
fn with_the_host_rule_on_a_force_push_is_refused() {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    let trunk = test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    test_box.git_ok(&repo, &["commit", "-q", "--amend", "-m", "rewritten"]);
    let out = test_box
        .git(&repo)
        .args(["push", "-f", "origin", "HEAD:main"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("append-only"));
    assert_eq!(test_box.git_bare(&host, &["rev-parse", "main"]), trunk);
}
