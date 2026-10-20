//! G31. A dead executor is replaced.
//!
//! Kill the only owning executor mid-check: after its lease expires a second executor claims
//! the operation by compare-and-swap and completes it; the dead one, if resumed, cannot
//! publish; each observer measures the lease from its own first sighting on its monotonic
//! clock; a supervisor that reboots waits a full lease again before it claims.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use support::{TestBox, check_files, passing_result, result_json};

fn wait_for(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn supervisor(test_box: &TestBox, clone: &Path, once: bool) -> Child {
    let mut cmd = test_box.bay_process(clone);
    cmd.args(["executor", "run"]);
    if once {
        cmd.arg("--once");
    } else {
        cmd.args(["--interval-s", "1"]);
    }
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

const LEASE_S: u64 = 6;

#[test]
fn a_killed_owner_is_replaced_after_its_lease_by_a_supervisor_that_waited_a_full_lease() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let started = test_box.path().join("check-started");
    let marker = test_box.path().join("checked-once");
    // The first run of the check is long; later runs are quick.
    let before = format!(
        "if {first}then sleep 8; fi",
        first = test_box.relay(&format!(
            "[ -f '{marker}' ] && exit 1; touch '{marker}' '{started}'",
            marker = marker.display(),
            started = started.display()
        ))
    );
    // A lease well above a supervisor's own start-up time, so a claim that did not wait a full
    // lease cannot hide inside that start-up.
    let mut files = check_files(&before, &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        format!("[executor]\nlease_s = {LEASE_S}\nheartbeat_s = 1\n"),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0);
    let chain = format!("refs/bay/changes/{id}");

    let a = test_box.clone_of(&remote, "executor-a");
    let b = test_box.clone_of(&remote, "executor-b");
    for (clone, name) in [(&a, "exec-a"), (&b, "exec-b")] {
        test_box.git_ok(clone, &["config", "bay.executorName", name]);
        test_box.git_ok(
            clone,
            &["config", "bay.producerName", &format!("{name}-checks")],
        );
    }

    // A claims and starts the check; it renews its lease with heartbeats; then it dies.
    let mut owner = supervisor(&test_box, &a, true);
    wait_for("A's check", Duration::from_secs(30), || started.exists());
    wait_for("a heartbeat", Duration::from_secs(10), || {
        test_box
            .git_bare(&remote, &["log", "--format=%s", &chain])
            .contains("heartbeat")
    });
    owner.kill().unwrap();
    let _ = owner.wait();

    // B starts, then "reboots" before a lease has passed: it must not have claimed.
    let mut first = supervisor(&test_box, &b, false);
    std::thread::sleep(Duration::from_millis(1000));
    first.kill().unwrap();
    let _ = first.wait();
    assert!(
        !test_box
            .git_bare(&remote, &["log", "--format=%s", &chain])
            .contains("epoch 2")
    );

    // After the reboot B waits a full lease from its own new sighting, then claims and lands.
    let relaunched = now();
    let mut second = supervisor(&test_box, &b, false);
    wait_for("the landing", Duration::from_secs(60), || {
        test_box.trunk_change_ids(&remote) == vec![id.clone()]
    });
    second.kill().unwrap();
    let _ = second.wait();
    let log = test_box.git_bare(&remote, &["log", "--format=%ct %s", &chain]);
    let claimed: u64 = log
        .lines()
        .find(|line| line.contains("epoch 2 by exec-b"))
        .and_then(|line| line.split(' ').next())
        .and_then(|time| time.parse().ok())
        .unwrap_or_else(|| panic!("no takeover claim in:\n{log}"));
    // Both times are whole seconds; a wait of d >= LEASE_S seconds moves the whole-second clock
    // by at least LEASE_S, so this bound has no tolerance to hide an early claim in.
    assert!(
        claimed >= relaunched + LEASE_S,
        "the claim came {} s after the relaunch, under one lease",
        claimed.saturating_sub(relaunched)
    );

    // The dead owner, resumed, cannot publish: it finds the publication.
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let resumed = test_box.bay_ok(&a, &["executor", "run", "--once"]);
    let ran = resumed["detail"]["ran"].as_array().unwrap();
    assert!(
        ran.iter()
            .all(|result| result["decision"] != "landed" || result["detail"]["existing"] == true),
        "{resumed}"
    );
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), tip);
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}
