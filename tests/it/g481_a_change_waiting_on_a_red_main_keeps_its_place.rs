//! G481 (bay 0.6.0, lever S3; BACKLOG 119, a fleet measurement, 2026-10-28). A change that waits
//! on a red main keeps its place in line, and no runner claims it while the red stands.
//!
//! A change on a red main's paths gave its place back, and every few minutes a runner queued it
//! again at the back and claimed it again at the head, only to find main still red: zynsylhi
//! was claimed eight times in half an hour, at places 1687, 1689, 1691, 1699, 1704, 1710, 1717
//! and 1720, rlkohjff eleven times in 25 minutes, and red waits were 29% of all lead time. Now
//! it waits in its place, judged before any claim; the line passes it in arrival order, a
//! change that names the red's owner still runs ahead, and once main is green it runs before
//! every change submitted after it. A batch never carries a change the red holds.
//!
//! The guards to mutate, all in `src/publish.rs` unless named: the `unless_red` before a head's
//! claim in `queue_gate` (the change is claimed while main is red); `wait_in_line` in `red_gate`
//! back to `wait_unplaced` (it gives its place back); the `TrunkRed` wait arm in `execute_op` (it is
//! checked ahead of its turn); `passes_red` in `queue_gate` (the line stops behind it); in
//! `src/executor.rs`, the `waits_on_main` skip in `abandoned_head` (it is visited after every
//! operation) and in `deadlines` (its progress deadline releases it); `ends_before_red` in
//! `batch` (a batch lands it on the red main); and `passes_red` in the host's hook
//! (`src/hook.rs`: a signed host refuses the change that passes it). The `TrunkRed` arm of
//! `given_up` in `execute_op` keeps the place of a head the red gate stops after its claim,
//! which the gate before the claim leaves to a red that appears mid-visit.

// Each case runs shell scripts (the check adapter), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::PRODUCER;
use support::queued::{Check, host, ran_for};
use support::{TestBox, check_files, passing_result, result_json};

/// A post check on `src/**` that fails on any tree holding `src/bad.rs`.
const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check suite\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

/// A trunk with `src/` and `docs/`, a pre check every change passes and [`POST`]; runners run
/// post checks, under the `[runner]` lines `runner` adds. The box, the work repository (which is
/// also the runner's clone) and the remote.
fn trunk(runner: &str) -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let mut files = check_files(
        &format!("[ \"$1\" = suite ] && [ -e src/bad.rs ] && {{ {failing}; exit 0; }}"),
        &passing_result(1),
        1,
        60,
    );
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(POST);
            text.push_str("\n[policy]\nreuse_unsigned = true\n");
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        format!("[runner]\nlease_s = 30\nheartbeat_s = 1\npost = true\n{runner}"),
    ));
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("docs/readme.md", "readme\n")],
        &files,
    );
    (test_box, repo, remote)
}

/// Starts change `name` on `dir/`, writes `<dir>/<name>` in its view, and submits it with
/// `bay merge`, which only queues it. With `fixes`, it names that change and removes
/// `src/bad.rs`. Its change id and operation id.
fn submit(
    test_box: &TestBox,
    repo: &Path,
    dir: &str,
    name: &str,
    fixes: Option<&str>,
) -> (String, String) {
    let paths = format!("{dir}/");
    let mut args = vec!["start", name, "--paths", paths.as_str()];
    if let Some(owner) = fixes {
        args.extend(["--fixes", owner]);
    }
    let started = test_box.bay_ok(repo, &args);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(format!("{dir}/{name}")), format!("{name}\n")).unwrap();
    if fixes.is_some() {
        std::fs::remove_file(view.join("src/bad.rs")).unwrap();
    }
    let (code, submitted) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");
    (
        started["change"].as_str().unwrap().to_owned(),
        submitted["op_id"].as_str().unwrap().to_owned(),
    )
}

/// The change that owns main's red now, as `bay status` names it.
fn owner(test_box: &TestBox, repo: &Path) -> String {
    let status = test_box.bay_ok(repo, &["status"]);
    status["detail"]["red"][0]["change"]
        .as_str()
        .unwrap_or_else(|| panic!("main is not red: {status}"))
        .to_owned()
}

/// Lands `src/bad.rs`, and a runner pass finds main red on `src/`.
fn red_main(test_box: &TestBox, repo: &Path) {
    let started = test_box.bay_ok(repo, &["start", "bad", "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/bad.rs"), "bad\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let ran = pass(test_box, repo);
    assert!(ran.to_string().contains("product_failure"), "{ran}");
}

/// One runner pass in `repo`.
fn pass(test_box: &TestBox, repo: &Path) -> Value {
    let (code, ran) = result_json(test_box.bay(repo).args(["runner", "run", "--once"]));
    assert_eq!(code, 0, "{ran}");
    ran
}

/// The messages of the remote's chain of change `id`, newest first.
fn chain(test_box: &TestBox, remote: &Path, id: &str) -> String {
    test_box.git_bare(
        remote,
        &["log", "--format=%s", &bay::change::change_ref(id)],
    )
}

/// How many claims the remote's chain of change `id` records.
fn claims(test_box: &TestBox, remote: &Path, id: &str) -> usize {
    chain(test_box, remote, id)
        .lines()
        .filter(|line| line.starts_with("claim "))
        .count()
}

/// The remote's queue, read in `repo`.
fn queue(repo: &Path) -> bay::queue::Queue {
    bay::queue::Queue::read(&bay::repo::Repo::open(repo).unwrap()).unwrap()
}

/// Puts operation `op_id` of change `id` in the line, as a runner placed it before main was red.
fn join(repo: &Path, id: &str, op_id: &str) {
    let handle = bay::repo::Repo::open(repo).unwrap();
    bay::queue::join(&handle, None, op_id, id).unwrap();
}

/// The live place of `op_id`.
fn place(repo: &Path, op_id: &str) -> Option<u64> {
    queue(repo).of(op_id).map(|entry| entry.seq)
}

/// How many rows `pass` holds for change `id`.
fn visits(pass: &Value, id: &str) -> usize {
    pass["detail"]["ran"].as_array().map_or(0, |rows| {
        rows.iter().filter(|row| row["change"] == id).count()
    })
}

#[test]
fn a_change_waiting_on_a_red_main_keeps_its_place_and_lands_first_once_green() {
    // Runners check one entry ahead of its turn, which a change the red holds never is.
    let (test_box, repo, remote) = trunk("ahead = 1\n");
    red_main(&test_box, &repo);
    // R1 holds a place in line when main turns red on its paths.
    let (r1, r1_op) = submit(&test_box, &repo, "src", "r1.rs", None);
    join(&repo, &r1, &r1_op);
    let first = pass(&test_box, &repo);
    let kept = place(&repo, &r1_op);
    assert!(
        kept.is_some(),
        "a change waiting on a red main holds no place: {first}"
    );
    assert_eq!(
        claims(&test_box, &remote, &r1),
        0,
        "a change was claimed on a red main: {first}"
    );
    // A docs change and a second red-path change, both submitted after R1.
    let (d1, d1_op) = submit(&test_box, &repo, "docs", "d1.md", None);
    let (r2, r2_op) = submit(&test_box, &repo, "src", "r2.rs", None);
    let mut passes = Vec::new();
    for _ in 0..4 {
        passes.push(pass(&test_box, &repo));
        if test_box.trunk_change_ids(&remote).contains(&d1) {
            break;
        }
    }
    assert!(
        test_box.trunk_change_ids(&remote).contains(&d1),
        "the line stopped behind the change the red holds: {passes:#?}"
    );
    assert_eq!(place(&repo, &r1_op), kept, "R1 lost its place");
    assert!(
        place(&repo, &r2_op).is_some_and(|r2_at| kept.is_some_and(|r1_at| r2_at > r1_at)),
        "R2 holds no place behind R1"
    );
    assert!(place(&repo, &d1_op).is_none(), "D1 still holds a place");
    let last = passes.last().unwrap();
    assert_eq!(
        visits(last, &r1),
        1,
        "the change the red holds was visited more than once a pass: {last}"
    );
    assert_eq!(
        (
            claims(&test_box, &remote, &r1),
            claims(&test_box, &remote, &r2)
        ),
        (0, 0),
        "a change the red holds was claimed: {passes:#?}"
    );
    // Before a runner judged it held it may have been checked ahead of its turn as any queued
    // change; once it waits on the red, never.
    let r2_chain = chain(&test_box, &remote, &r2);
    let lines: Vec<&str> = r2_chain.lines().collect();
    let held_at = lines
        .iter()
        .rposition(|line| line.contains("main is red on its paths"));
    let checked_since = |at: usize| {
        lines[..at]
            .iter()
            .any(|line| line.starts_with("prechecked"))
    };
    assert!(
        held_at.is_some_and(|at| !checked_since(at)),
        "a change the red holds was checked ahead of its turn: {r2_chain}"
    );
    // A fix names the red's owner: it runs ahead of the places the red holds, and once main is
    // green R1 lands before R2, which came after it.
    let red_owner = owner(&test_box, &repo);
    let (fix, _) = submit(&test_box, &repo, "src", "fix.rs", Some(&red_owner));
    let mut passes = Vec::new();
    for _ in 0..6 {
        passes.push(pass(&test_box, &repo));
        if test_box.trunk_change_ids(&remote).contains(&r2) {
            break;
        }
    }
    let landed = test_box.trunk_change_ids(&remote);
    let at = |id: &str| landed.iter().position(|landed| landed == id);
    assert!(
        at(&r1).is_some() && at(&r2).is_some() && at(&fix).is_some(),
        "{landed:?}: {passes:#?}"
    );
    // Newest first: R2 after R1 after the fix.
    assert!(
        at(&r2) < at(&r1) && at(&r1) < at(&fix),
        "the order was not kept: {landed:?}"
    );
    let released = queue(&repo).release_reasons.get(&kept.unwrap()).cloned();
    assert_eq!(
        released.as_deref(),
        Some("published"),
        "R1 did not land from the place it kept"
    );
    assert_eq!(
        claims(&test_box, &remote, &r1),
        1,
        "R1 was claimed while main was red"
    );
}

#[test]
fn a_batch_never_carries_a_change_the_red_holds() {
    let (test_box, repo, remote) = trunk("batch = { max = 4 }\n");
    red_main(&test_box, &repo);
    // A docs change at the head, a red-path change behind it: one batch would take both.
    let (d1, d1_op) = submit(&test_box, &repo, "docs", "d1.md", None);
    let (r3, r3_op) = submit(&test_box, &repo, "src", "r3.rs", None);
    join(&repo, &d1, &d1_op);
    join(&repo, &r3, &r3_op);
    let ran = pass(&test_box, &repo);
    let landed = test_box.trunk_change_ids(&remote);
    assert!(landed.contains(&d1), "{ran}");
    assert!(
        !landed.contains(&r3),
        "a batch landed a change on the red main's paths: {ran}"
    );
    assert!(
        place(&repo, &r3_op).is_some(),
        "the change the red holds lost its place"
    );
}

#[test]
fn a_head_the_red_holds_outlives_its_progress_deadline() {
    // A progress deadline of four seconds: a head that makes no progress for that long is
    // released, but one main's red holds waits for main, not for its own work.
    let (test_box, repo, _remote) = trunk("execution_budget_s = 3\npublish_window_s = 1\n");
    red_main(&test_box, &repo);
    let (r1, r1_op) = submit(&test_box, &repo, "src", "r1.rs", None);
    join(&repo, &r1, &r1_op);
    let _ = pass(&test_box, &repo);
    let kept = place(&repo, &r1_op);
    assert!(
        kept.is_some(),
        "a change waiting on a red main holds no place"
    );
    let mut looping = test_box
        .bay_process(&repo)
        .args(["--json", "runner", "run", "--interval-s", "1"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(12));
    let _ = looping.kill();
    let _ = looping.wait();
    let line = queue(&repo);
    assert_eq!(
        line.of(&r1_op).map(|entry| entry.seq),
        kept,
        "the head the red holds lost its place: released as {:?}",
        kept.and_then(|seq| line.release_reasons.get(&seq))
    );
}

#[test]
fn a_signed_host_takes_a_change_that_passes_a_place_the_red_holds() {
    // A bay host under a registry that signs everything: its hook lets only the queue's head
    // land, or a change that passes places main's red holds.
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\npost = true\n",
        policy: Some(&format!(
            "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\n{}",
            POST.replace("scripts/bay-check suite", "scripts/bay-check")
        )),
        seed: &[("src/a.rs", "fn a() {}\n"), ("docs/readme.md", "readme\n")],
        hold_first: false,
        fails: r#"{ case "$req" in *'"plan":{"id":"suite"'*) [ -e src/bad.rs ] ;; *) false ;; esac; }"#,
    });
    let executor = held.executor("exec", PRODUCER);
    let _bad = held.queue_with("bad", &["src"], &[("src/bad.rs", "bad\n")]);
    let first = held.pass(&executor);
    assert!(first.to_string().contains("product_failure"), "{first}");
    let r1 = held.queue("r1", "src");
    let d1 = held.queue_with("d1", &["docs"], &[("docs/d1.md", "d1\n")]);
    let r1_op = format!("{r1}.1");
    let kept = place(&held.repo, &r1_op);
    let ran = held.pass(&executor);
    let landed = held.test_box.trunk_change_ids(&held.remote);
    assert!(
        landed.contains(&d1),
        "the host refused the change past the place the red holds: {}",
        ran_for(&ran, &d1)
    );
    assert!(!landed.contains(&r1), "{ran}");
    assert!(kept.is_some() && place(&held.repo, &r1_op) == kept, "{ran}");
    assert_eq!(claims(&held.test_box, &held.remote, &r1), 0, "{ran}");
}
