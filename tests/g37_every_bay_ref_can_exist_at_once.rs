//! G37. Every bay ref can exist at once.
//!
//! On a reftable repository the full ref set of a change (change, request, landed, intent and
//! landed-intent generations, a queue pair, live, log, talk, registry, evidence notes), for two
//! changes side by side, is created in one transaction; the validator rejects a proposed name
//! that is a prefix of another; the hook refuses an id outside the fixed alphabet, an operator
//! name with `/` or an upper-case letter (so producers `ci` and `ci/linux` cannot coexist), and
//! a name longer than 32 characters.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

mod support;

use std::io::Write;
use std::process::Stdio;

use support::{TestBox, check_files, passing_result};

fn ref_set(id: &str, hash: &str) -> Vec<String> {
    [
        format!("refs/bay/changes/{id}"),
        format!("refs/bay/requests/{id}"),
        format!("refs/bay/landed/{id}"),
        format!("refs/bay/intent/{hash}/1"),
        format!("refs/bay/intent/{hash}/2"),
        format!("refs/bay/landed-intent/{hash}/1"),
        format!("refs/bay/landed-intent/{hash}/2"),
    ]
    .into_iter()
    .collect()
}

#[test]
fn the_full_ref_set_of_two_changes_exists_at_once_on_reftable() {
    let test_box = TestBox::new();
    let repo = test_box.repo_without_remote("repo", "sha256", "reftable");
    test_box.git_ok(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
    let commit = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);
    let mut names = Vec::new();
    for (id, hash) in [
        ("abcd2345", "0123456789abcdef0123456789abcdef"),
        ("wxyz6723", "fedcba9876543210fedcba9876543210"),
    ] {
        names.extend(ref_set(id, hash));
    }
    names.extend(
        [
            "refs/bay/queue/1",
            "refs/bay/queue-done/1",
            "refs/bay/queue/2",
            "refs/bay/live/build-box-1",
            "refs/bay/log/exec-1",
            "refs/bay/talk/agent-b/abcdefghijklmnop",
            "refs/bay/registry",
            "refs/bay/landed-index/1",
            "refs/notes/bay/evidence/ci",
            "refs/notes/bay/evidence/ci-linux",
        ]
        .map(str::to_owned),
    );
    let as_str: Vec<&str> = names.iter().map(String::as_str).collect();
    for name in &as_str {
        bay::names::bay_ref(name).unwrap();
    }
    assert_eq!(bay::names::prefix_clash(&as_str), None);

    let input = names.iter().fold(String::new(), |mut input, name| {
        use std::fmt::Write as _;
        let _ = writeln!(input, "create {name} {commit}");
        input
    });
    let mut child = test_box
        .git(&repo)
        .args(["update-ref", "--stdin"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    assert!(
        child.wait().unwrap().success(),
        "one transaction creates every bay ref"
    );
    let count = test_box
        .git_ok(
            &repo,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/bay/",
                "refs/notes/bay/",
            ],
        )
        .lines()
        .count();
    assert_eq!(count, names.len());
}

#[test]
fn a_name_that_is_a_directory_of_another_is_rejected() {
    let proposed = [
        "refs/bay/changes/abcd2345",
        "refs/bay/changes/abcd2345/request",
    ];
    assert!(bay::names::prefix_clash(&proposed).is_some());
    assert!(bay::names::bay_ref(proposed[1]).is_err());
    // Git refuses it too, on reftable as on files.
    let test_box = TestBox::new();
    let repo = test_box.repo_without_remote("repo", "sha1", "reftable");
    test_box.git_ok(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
    test_box.git_ok(&repo, &["update-ref", proposed[0], "HEAD"]);
    let out = test_box
        .git(&repo)
        .args(["update-ref", proposed[1], "HEAD"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn the_hook_refuses_names_outside_the_alphabet_and_the_name_rules() {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.git_ok(&repo, &["remote", "add", "origin", host.to_str().unwrap()]);
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    let head = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);
    for name in [
        "refs/bay/changes/ABCD2345".to_owned(),
        "refs/notes/bay/evidence/ci/linux".to_owned(),
        "refs/notes/bay/evidence/CI".to_owned(),
        format!("refs/bay/log/{}", "e".repeat(33)),
        "refs/bay/unknown/x".to_owned(),
    ] {
        let out = test_box
            .git(&repo)
            .args(["push", "origin", &format!("{head}:{name}")])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{name} was accepted");
        assert!(
            stderr.contains("not a bay ref name")
                || stderr.contains("must have 1 to 32")
                || stderr.contains("only lower-case"),
            "{name}: {stderr}"
        );
    }
}
