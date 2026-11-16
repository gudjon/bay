//! G752 (a fleet finding, 2026-11-16: a `bay merge --wait=7200` ran 61,303 Git commands, a
//! `rev-parse` and a `cat-file` five times a second while its run checked, and nineteen such
//! waits ended in fifteen minutes on one box at load 70). A wait with a time limit reads its
//! change's chain head once a second, and the record only when the head moved.
//!
//! Guard to mutate: in `src/publish.rs` `observe_land`, read the chain every 200 ms again, and
//! the wait runs about eighty Git commands in eight seconds.

#![cfg(unix)]

use crate::support;

use std::time::Duration;

use support::{TestBox, check_files, passing_result};

#[test]
fn a_timed_wait_reads_its_chain_once_a_second() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let files = check_files("sleep 40", &passing_result(1), 1, 120);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (_id, view) = test_box.start_and_edit(&repo);

    // WHEN the author waits with a time limit while the run checks for forty seconds
    let calls = test_box.path().join("calls");
    test_box.wrap_git(&format!("echo \"$PPID\" >> '{}'", calls.display()));
    let mut wait = test_box
        .bay_process(&view)
        .args(["--json", "merge", "--wait=60"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let me = wait.id().to_string();
    let mine = || {
        std::fs::read_to_string(&calls)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == me)
            .count()
    };
    // Its submission settles first; then eight seconds of watching are counted.
    std::thread::sleep(Duration::from_secs(12));
    let before = mine();
    std::thread::sleep(Duration::from_secs(8));
    let watched = mine() - before;
    let _ = wait.kill();
    let _ = wait.wait();

    // THEN it read its chain about once a second: one `rev-parse` a second, no `cat-file`
    assert!(
        watched <= 12,
        "the wait ran {watched} Git commands in 8 s while its run checked"
    );
}
