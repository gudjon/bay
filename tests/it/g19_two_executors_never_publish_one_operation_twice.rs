//! G19. Two executors never publish one operation twice.
//!
//! Two executors receive the same operation concurrently: exactly one publishes; the other
//! records `superseded` or reports the existing publication.
//!
//! Every case holds both executors inside the operation at once, at named boundaries, so the
//! loser's answer is exactly one of the two G19 names: `superseded` when it resumes before the
//! winner published, the existing publication when it resumes after, and `superseded` when it
//! loses the claim itself (`two_executors_on_one_operation_publish_it_once`, both held just
//! before their claim and released together). No case accepts a third answer.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use support::{TestBox, check_files, passing_result, result_json};

/// A change with one passing check submitted for an executor, and two executor clones of its
/// remote, `exec-a` and `exec-b`. Returns (change id, remote, clone a, clone b).
fn submitted(test_box: &TestBox) -> (String, PathBuf, PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0);
    let a = test_box.clone_of(&remote, "executor-a");
    let b = test_box.clone_of(&remote, "executor-b");
    for (clone, name) in [(&a, "exec-a"), (&b, "exec-b")] {
        test_box.git_ok(clone, &["config", "bay.executorName", name]);
        test_box.git_ok(
            clone,
            &["config", "bay.producerName", &format!("{name}-checks")],
        );
    }
    (id, remote, a, b)
}

/// An executor held at a publication boundary. A case that fails while it is held kills it,
/// so no executor outlives its test waiting for a release file in a removed directory.
struct Held {
    child: Option<Child>,
    release: PathBuf,
}

impl Drop for Held {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// `bay executor run --once` in `clone`, held at publication boundary `at` until the file
/// `release` exists. Returns once the executor has reached the boundary.
fn paused_at(test_box: &TestBox, clone: &Path, at: &str, release: &Path) -> Held {
    let child = test_box
        .bay_process(clone)
        .env("BAY_TEST_PAUSE_AT", at)
        .env("BAY_TEST_PAUSE_FILE", release)
        .args(["--json", "executor", "run", "--once"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let held = Held {
        child: Some(child),
        release: release.to_path_buf(),
    };
    let paused = PathBuf::from(format!("{}.paused", release.display()));
    // The gate box runs on two cores at nice 19: allow it time to reach the boundary.
    let deadline = Instant::now() + Duration::from_secs(120);
    while !paused.exists() {
        assert!(Instant::now() < deadline, "the executor never reached {at}");
        std::thread::sleep(Duration::from_millis(50));
    }
    held
}

/// Releases a held executor and returns the one operation result it ran.
fn resumed(mut held: Held) -> serde_json::Value {
    std::fs::write(&held.release, "go").unwrap();
    let out = held.child.take().unwrap().wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let ran: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    ran["detail"]["ran"][0].clone()
}

/// Both executors hold the operation: A checked it and is about to publish, B took it over
/// with a higher epoch and is about to publish. A resumes first: its epoch is stale and nothing
/// is on trunk, so it records `superseded` and publishes nothing. Then B publishes, once.
#[test]
fn the_loser_records_superseded_when_it_resumes_before_the_publication() {
    let test_box = TestBox::new();
    let (id, remote, a, b) = submitted(&test_box);
    let trunk = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let release_a = test_box.path().join("release-a");
    let release_b = test_box.path().join("release-b");
    let first = paused_at(&test_box, &a, "after-persist", &release_a);
    let second = paused_at(&test_box, &b, "before-publish", &release_b);

    let loser = resumed(first);
    assert_eq!(loser["decision"], "ok", "{loser}");
    assert_eq!(loser["detail"]["superseded"], true, "{loser}");
    assert_eq!(loser["detail"]["epoch"], 2, "{loser}");
    assert_eq!(loser["detail"]["owner"], "exec-b", "{loser}");
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", "main"]),
        trunk,
        "the superseded executor pushed nothing"
    );

    let winner = resumed(second);
    assert_eq!(winner["decision"], "landed", "{winner}");
    assert_ne!(winner["detail"]["existing"], true, "{winner}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}

/// Both executors hold the operation, and B publishes while A is held before its own publish.
/// A resumes: it reports B's publication as the existing one, the same commit, and pushes
/// nothing.
#[test]
fn the_loser_reports_the_existing_publication_when_it_resumes_after_it() {
    let test_box = TestBox::new();
    let (id, remote, a, b) = submitted(&test_box);
    let release_a = test_box.path().join("release-a");
    let first = paused_at(&test_box, &a, "after-persist", &release_a);

    let ran = test_box.bay_ok(&b, &["executor", "run", "--once"]);
    let winner = &ran["detail"]["ran"][0];
    assert_eq!(winner["decision"], "landed", "{ran}");
    assert_ne!(winner["detail"]["existing"], true, "{ran}");
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);

    let loser = resumed(first);
    assert_eq!(loser["decision"], "landed", "{loser}");
    assert_eq!(loser["detail"]["existing"], true, "{loser}");
    assert_eq!(loser["detail"]["commit"], tip.as_str(), "{loser}");
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", "main"]),
        tip,
        "the loser pushed nothing"
    );
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}

/// One race: both executors read the operation unclaimed and are held just before their
/// claim, then both are released at once. Exactly one publishes; the other lost the claim's
/// compare-and-swap and records `superseded`, or reports the existing publication. Neither
/// can answer `held`: each saw the operation unclaimed before it claimed.
fn run(test_box: &TestBox, round: usize) {
    let repo = test_box.repo_any(&format!("repo{round}"));
    let remote = test_box.path().join(format!("repo{round}-remote.git"));
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("sleep 1", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0);

    let one = test_box.clone_of(&remote, &format!("executor-a{round}"));
    let two = test_box.clone_of(&remote, &format!("executor-b{round}"));
    for (clone, name) in [(&one, "exec-a"), (&two, "exec-b")] {
        test_box.git_ok(clone, &["config", "bay.executorName", name]);
        test_box.git_ok(
            clone,
            &["config", "bay.producerName", &format!("{name}-checks")],
        );
    }
    let release_one = test_box.path().join(format!("release-a{round}"));
    let release_two = test_box.path().join(format!("release-b{round}"));
    let mut first = paused_at(test_box, &one, "before-claim", &release_one);
    let mut second = paused_at(test_box, &two, "before-claim", &release_two);
    std::fs::write(&release_one, "go").unwrap();
    std::fs::write(&release_two, "go").unwrap();
    let outputs: Vec<serde_json::Value> = [first.child.take(), second.child.take()]
        .into_iter()
        .map(|child| {
            let out = child.unwrap().wait_with_output().unwrap();
            assert!(out.status.success(), "{out:?}");
            serde_json::from_slice(&out.stdout).unwrap()
        })
        .collect();

    let results: Vec<&serde_json::Value> = outputs
        .iter()
        .filter_map(|out| out["detail"]["ran"].get(0))
        .collect();
    assert_eq!(
        results.len(),
        2,
        "both executors ran the operation: {outputs:?}"
    );
    let published = results
        .iter()
        .filter(|result| result["decision"] == "landed" && result["detail"]["existing"] != true)
        .count();
    assert_eq!(published, 1, "exactly one executor publishes: {outputs:?}");
    let log = test_box.git_bare(
        &remote,
        &["log", "--format=%s", &format!("refs/bay/changes/{id}")],
    );
    for result in &results {
        let landed = result["decision"] == "landed";
        let superseded = result["detail"]["superseded"] == true;
        assert!(
            landed || superseded,
            "{result}\nall: {outputs:?}\nchain:\n{log}"
        );
    }
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}

#[test]
fn two_executors_on_one_operation_publish_it_once() {
    let test_box = TestBox::new();
    for round in 0..3 {
        run(&test_box, round);
    }
}
