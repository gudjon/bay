//! G388: a submit whose chain head another process pushed meanwhile is not refused.
//!
//! The remote can take the very chain head a submit pushes from another process (a runner
//! pushing this box's records) after it advertised the older tip and before it locks the ref.
//! The atomic push then fails ("cannot lock ref … is at <head> but expected <older>"), yet the
//! chain holds what was pushed and nothing was refused: bay pushes the request alone, and the
//! change is queued.

// The pause runs a submit as a background process, on Unix only like the other pause tests.
#![cfg(unix)]

use std::time::{Duration, Instant};

use crate::support;

use support::{TestBox, check_files, passing_result};

#[test]
fn a_chain_head_already_pushed_meanwhile_still_queues_the_change() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let release = test_box.path().join("release");
    let child = test_box
        .bay_process(&view)
        .env("BAY_TEST_PAUSE_AT", "before-request-push")
        .env("BAY_TEST_PAUSE_FILE", &release)
        .args(["--json", "merge"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !test_box.path().join("release.paused").exists() {
        assert!(
            Instant::now() < deadline,
            "the submit never reached its push"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // The remote has the head's objects, and moves the chain to it itself once, between its
    // advertisement and its ref lock, as another process's push of the same head would.
    let chain = format!("refs/bay/changes/{id}");
    let head = test_box.git_ok(&view, &["rev-parse", &chain]);
    test_box.git_ok(
        &view,
        &[
            "push",
            "-q",
            "--no-verify",
            "origin",
            &format!("{head}:refs/held/head"),
        ],
    );
    let hook = remote.join("hooks/pre-receive");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    let once = test_box.path().join("moved-once");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\nwhile read old new name; do\n  if [ \"$name\" = \"{chain}\" ] && [ ! -e {once} ]; then\n    touch {once}\n    env -u GIT_QUARANTINE_PATH git update-ref \"$name\" \"$new\" || exit 1\n  fi\ndone\nexit 0\n",
            once = once.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(&release, "go").unwrap();
    let output = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(once.exists(), "the remote moved the chain itself: {text}");
    assert!(output.status.success(), "{text}");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", &chain]), head);
    assert!(!text.contains("refused the push"), "{text}");
    let requests = format!("refs/bay/requests/{id}");
    assert!(
        !test_box
            .git_bare(&remote, &["rev-parse", "-q", "--verify", &requests])
            .is_empty(),
        "the request reached the remote: {text}"
    );
}
