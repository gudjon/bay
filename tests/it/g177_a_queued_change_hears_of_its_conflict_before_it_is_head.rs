//! G177. A queued change hears of its conflict before it is head.
//!
//! Three changes wait in the publication queue. Trunk moves into a conflict with the third
//! (another clone adds the file the third adds). While the head's check still runs, the third's
//! `bay status` already reports `MERGE_CONFLICT` with the conflicting path: the executor probed
//! every entry behind the head against the new trunk with `git merge-tree --write-tree --quiet`
//! and noted the conflict on the operation in its chain. The second, which merges cleanly,
//! reports none, and the third keeps its place in the queue (`DESIGN` § 6 step 2).

// The check runs a shell script through the relay, so this runs on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

/// A trunk whose check moves trunk (from the `mover` clone) on its first two runs, so the first
/// change joins the queue, and whose third run, the head's check, waits until `go` exists,
/// writing `held` once it waits.
struct Held {
    test_box: TestBox,
    repo: PathBuf,
    remote: PathBuf,
    mover: PathBuf,
    waiting: PathBuf,
    go: PathBuf,
}

fn setup() -> Held {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let runs = test_box.path().join("runs");
    let mover = test_box.path().join("mover");
    let held = test_box.path().join("held");
    let go = test_box.path().join("go");
    // The test side of the check (G62): it logs the run, moves trunk on runs 1 and 2, and on
    // run 3 holds the check until the test lets it go.
    let relayed = test_box.relay(&format!(
        "exec 3>&1 >/dev/null\n\
         echo \"$tree\" >> '{runs}'; n=$(wc -l < '{runs}' | tr -d ' '); \
         if [ \"$n\" -le 2 ]; then ( cd '{mover}' && git pull -q --rebase && echo \"$n\" > docs/move-$n.md && git add docs && git -c user.name=t -c user.email=t@t commit -q -m \"move $n\" && git push -q origin main ); fi; \
         if [ \"$n\" -eq 3 ]; then : > '{held}'; until [ -e '{go}' ]; do sleep 0.05; done; fi\n\
         echo \"$n\" >&3",
        runs = runs.display(),
        mover = mover.display(),
        held = held.display(),
        go = go.display(),
    ));
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("docs/keep.md", "k\n")],
        &check_files(&format!("n=$({relayed})\n"), &passing_result(1), 1, 120),
    );
    test_box.clone_of(&remote, "mover");
    Held {
        test_box,
        repo,
        remote,
        mover,
        waiting: held,
        go,
    }
}

/// Starts change `file` on `src/`, adds `src/<file>.rs` in its view, and submits it.
fn land(held: &Held, file: &str) -> (String, PathBuf) {
    let started = held
        .test_box
        .bay_ok(&held.repo, &["start", file, "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(format!("src/{file}.rs")), "fn x() {}\n").unwrap();
    let (code, submitted) = result_json(held.test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{submitted}");
    (id, view)
}

/// `land --wait` of a change while the queue is live: it holds a place at `position`.
fn hold_place(held: &Held, view: &Path, position: u64) {
    let (code, waiting) = result_json(held.test_box.bay(view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{waiting}");
    assert_eq!(waiting["decision"], "queued", "{waiting}");
    assert_eq!(waiting["detail"]["position"], position, "{waiting}");
}

fn operation(held: &Held, id: &str) -> serde_json::Value {
    let status = held.test_box.bay_ok(&held.repo, &["status", id]);
    status["detail"]["changes"][0]["operation"].clone()
}

#[test]
fn the_third_queued_change_sees_its_conflict_while_the_head_is_checked() {
    let held = setup();
    let (first, _) = land(&held, "first");
    let executor = held.test_box.clone_of(&held.remote, "executor");
    // Runs 1 and 2 move trunk: the first change joins the queue as its head.
    let queued = held
        .test_box
        .bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(
        queued["detail"]["ran"][0]["reason"], "PUBLICATION_QUEUED",
        "{queued}"
    );
    let (second, second_view) = land(&held, "second");
    hold_place(&held, &second_view, 2);
    let (third, third_view) = land(&held, "third");
    hold_place(&held, &third_view, 3);

    // Trunk moves into a conflict with the third: the same new file, other bytes.
    let mover = &held.mover;
    held.test_box.git_ok(mover, &["pull", "-q", "--rebase"]);
    std::fs::write(mover.join("src/third.rs"), "fn on_trunk() {}\n").unwrap();
    held.test_box.git_ok(mover, &["add", "src/third.rs"]);
    held.test_box.git_ok(
        mover,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "third on trunk",
        ],
    );
    held.test_box
        .git_ok(mover, &["push", "-q", "origin", "main"]);

    // The executor's pass: the head's check (run 3) waits for the test.
    let child = held
        .test_box
        .bay_process(&executor)
        .args(["--json", "executor", "run", "--once"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    while !held.waiting.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the head's check never started"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // The head is still being checked: the third already hears of its conflict, with its path.
    let third_op = operation(&held, &third);
    let conflict = &third_op["conflict"];
    assert_eq!(conflict["reason"], "MERGE_CONFLICT", "{third_op}");
    assert_eq!(
        conflict["conflicts"],
        serde_json::json!(["src/third.rs"]),
        "{third_op}"
    );
    assert!(
        conflict["allowed"]
            .as_array()
            .is_some_and(|allowed| allowed.iter().any(|action| action == "refresh")),
        "{third_op}"
    );
    // It keeps its place: still queued, not blocked, and its reservation is not released.
    assert_eq!(third_op["state"], "admitted", "{third_op}");
    assert_eq!(third_op["outcome"], "PUBLICATION_QUEUED", "{third_op}");
    let released = held.test_box.git_bare(
        &held.remote,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/bay/queue-done/",
        ],
    );
    assert!(!released.contains("queue-done/3"), "{released}");
    // The second merges cleanly and hears of nothing.
    let second_op = operation(&held, &second);
    assert!(second_op.get("conflict").is_none(), "{second_op}");
    assert_eq!(held.test_box.trunk_change_ids(&held.remote).len(), 0);

    // The head goes on and lands, then the second; the third, as head, meets the conflict.
    std::fs::write(&held.go, "go").unwrap();
    let out = child.wait_with_output().unwrap();
    let ran: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        held.test_box.trunk_change_ids(&held.remote),
        vec![second, first],
        "{ran}"
    );
    let noted = &ran["detail"]["early_conflicts"];
    assert_eq!(noted[0]["change"], third.as_str(), "{ran}");
    let third_ran = ran["detail"]["ran"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["change"] == third.as_str())
        .cloned()
        .unwrap();
    assert_eq!(third_ran["reason"], "MERGE_CONFLICT", "{ran}");
}
