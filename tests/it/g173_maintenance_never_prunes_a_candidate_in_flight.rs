//! G173. Maintenance never prunes a candidate in flight.
//!
//! Between `merge-tree` and the step that links the candidate into the change chain, no ref
//! but the operation's in-flight ref (`refs/bay/inflight/<op id>`, local to the executor's
//! store) holds the candidate. A `git gc --prune=now` in the executor's store at that moment
//! removes nothing the landing needs: the same candidate lands. The ref is gone once the
//! operation reaches its outcome (landed, failed or abandoned), no push carries one to the
//! remote, and `bay status` lists one whose operation ended or is gone as a leftover.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use support::{TestBox, check_files, passing_result, result_json};

/// Kills a background process if the test ends before it does.
struct Killed(Option<Child>);

impl Drop for Killed {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A trunk with one check whose result `result` prints, a change submitted on it, and an
/// executor clone of the remote. Returns (box, remote, view, change id, executor).
fn submitted(result: &str) -> (TestBox, PathBuf, PathBuf, String, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", result, 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, submitted) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");
    let executor = test_box.clone_of(&remote, "executor");
    (test_box, remote, view, id, executor)
}

/// Runs the executor once, held just after it committed the candidate, before its check.
/// Returns the process and the file that releases it.
fn held_after_commit(test_box: &TestBox, executor: &Path) -> (Killed, PathBuf) {
    let release = test_box.path().join("release");
    let child = test_box
        .bay_process(executor)
        .env("BAY_TEST_PAUSE_AT", "after-commit")
        .env("BAY_TEST_PAUSE_FILE", &release)
        .args(["--json", "runner", "run", "--once"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let paused = test_box.path().join("release.paused");
    let deadline = Instant::now() + Duration::from_secs(120);
    while !paused.exists() {
        assert!(
            Instant::now() < deadline,
            "the executor never committed a candidate"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    (Killed(Some(child)), release)
}

/// Releases the held executor and returns its one run's result.
fn release(mut held: Killed, release: &Path) -> serde_json::Value {
    std::fs::write(release, "go").unwrap();
    let out = held.0.take().unwrap().wait_with_output().unwrap();
    let ran: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    ran["detail"]["ran"][0].clone()
}

/// The anchors in `store`: (ref name, object) pairs.
fn anchors(test_box: &TestBox, store: &Path) -> Vec<(String, String)> {
    test_box
        .git_ok(
            store,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/bay/inflight/",
            ],
        )
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(name, oid)| (name.to_owned(), oid.to_owned()))
        .collect()
}

/// The in-flight refs the bare remote holds, one per line.
fn remote_inflight(test_box: &TestBox, remote: &Path) -> String {
    test_box.git_bare(
        remote,
        &["for-each-ref", "--format=%(refname)", "refs/bay/inflight/"],
    )
}

#[test]
fn a_prune_between_merge_and_push_lands_the_same_candidate() {
    let (test_box, remote, _view, id, executor) = submitted(&passing_result(1));
    let (held, file) = held_after_commit(&test_box, &executor);
    let anchored = anchors(&test_box, &executor);

    // Maintenance with no prune window runs in the executor's store while the candidate is
    // still held by nothing but its anchor.
    test_box.git_ok(&executor, &["config", "gc.pruneExpire", "now"]);
    test_box.git_ok(&executor, &["gc", "-q", "--prune=now"]);

    let ran = release(held, &file);
    assert_eq!(ran["decision"], "merged", "{ran}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id.clone()]);

    // What landed is the candidate the anchor held, tree and all.
    let [(name, candidate)] = anchored.as_slice() else {
        panic!("one anchor while the operation ran: {anchored:?}");
    };
    assert_eq!(name, &format!("refs/bay/inflight/{id}.1"));
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);
    assert_eq!(&tip, candidate);
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:src/new.rs"]),
        "fn new() {}"
    );
    assert!(
        anchors(&test_box, &executor).is_empty(),
        "landed: no anchor"
    );
    // The executor pushed its evidence, its chain and trunk while the ref existed; none of
    // those pushes carried it.
    assert_eq!(
        remote_inflight(&test_box, &remote),
        "",
        "the remote holds none"
    );
}

/// A store that holds in-flight refs pushes none of them, and `bay status` names each whose
/// operation ended (here, published) or is gone as a leftover.
#[test]
fn an_inflight_ref_is_never_pushed_and_a_stale_one_is_a_leftover() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let head = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);
    let gone = format!("refs/bay/inflight/{id}.9");
    test_box.git_ok(&repo, &["update-ref", &gone, &head]);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id.clone()]);
    assert_eq!(
        remote_inflight(&test_box, &remote),
        "",
        "the remote holds none"
    );

    // A ref left by an executor killed mid-run, of an operation that has since landed.
    let ended = format!("refs/bay/inflight/{id}.1");
    test_box.git_ok(&repo, &["update-ref", &ended, &head]);
    let status = test_box.bay_ok(&repo, &["status"]);
    let stale: Vec<&serde_json::Value> = status["detail"]["leftovers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|leftover| leftover["kind"] == "inflight")
        .collect();
    let named: Vec<&str> = stale
        .iter()
        .map(|leftover| leftover["ref"].as_str().unwrap())
        .collect();
    assert_eq!(named, vec![ended.as_str(), gone.as_str()], "{status}");
    assert_eq!(stale[0]["op_state"], "published", "{status}");
    assert_eq!(stale[1]["op_state"], serde_json::Value::Null, "{status}");
    assert_eq!(stale[0]["object"], head.as_str());
}

#[test]
fn a_failed_check_leaves_no_anchor() {
    let failing = passing_result(1).replace("pass", "product_failure");
    let (test_box, remote, _view, _id, executor) = submitted(&failing);
    let (held, file) = held_after_commit(&test_box, &executor);
    assert_eq!(anchors(&test_box, &executor).len(), 1);
    let ran = release(held, &file);
    assert_eq!(ran["decision"], "blocked", "{ran}");
    assert_eq!(test_box.trunk_change_ids(&remote), Vec::<String>::new());
    assert!(
        anchors(&test_box, &executor).is_empty(),
        "failed: no anchor"
    );
}

#[test]
fn a_change_abandoned_in_flight_leaves_no_anchor() {
    let (test_box, remote, view, _id, executor) = submitted(&passing_result(1));
    let (held, file) = held_after_commit(&test_box, &executor);
    assert_eq!(anchors(&test_box, &executor).len(), 1);
    let abandoned = test_box.bay_ok(&view, &["abandon"]);
    assert_eq!(abandoned["detail"]["state"], "abandoned", "{abandoned}");
    let ran = release(held, &file);
    assert_ne!(ran["decision"], "merged", "{ran}");
    assert_eq!(test_box.trunk_change_ids(&remote), Vec::<String>::new());
    assert!(
        anchors(&test_box, &executor).is_empty(),
        "abandoned: no anchor"
    );
}
