//! G253. A terminal record carries its timing.
//!
//! A pilot computes queue wait apart from check time from Git alone (`ROADMAP` § 3). A change
//! whose first two checks see trunk move joins the queue, then lands as its head. Its terminal
//! record holds when it was submitted, queued, became head and was published, its attempts with
//! each one's check times, and the derived `queue_wait_s`, `check_s` and `total_s`, every time
//! the commit time of a commit bay signed. The times are ordered, and the queue wait and the
//! check time fit in the total. `bay status <change>` shows the same timing on its operation row.
//! Reading the timing costs a constant number of `git` spawns, however many heartbeats the
//! chain holds: one walk of the chain's subjects, never one read per state.

// The check moves trunk with a shell script, so this runs on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

fn at(record: &Value, field: &str) -> u64 {
    record[field]
        .as_u64()
        .unwrap_or_else(|| panic!("no {field} in the terminal record: {record}"))
}

#[test]
fn a_queued_landing_records_its_queue_wait_and_check_time() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let runs = test_box.path().join("runs");
    let mover = test_box.path().join("mover");
    // The first two runs move trunk from another clone while the check runs, so the
    // operation's pushes are refused twice and it joins the queue (`DESIGN` § 6).
    let relayed = test_box.relay(&format!(
        "exec 3>&1 >/dev/null\n\
         echo \"$tree\" >> '{runs}'; n=$(wc -l < '{runs}' | tr -d ' '); \
         if [ \"$n\" -le 2 ]; then ( cd '{mover}' && git pull -q --rebase && echo \"$n\" > docs/move-$n.md && git add docs && git -c user.name=t -c user.email=t@t commit -q -m \"move $n\" && git push -q origin main ); fi\n\
         echo \"$n\" >&3",
        runs = runs.display(),
        mover = mover.display(),
    ));
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("docs/keep.md", "k\n")],
        &check_files(&format!("n=$({relayed})\n"), &passing_result(1), 1, 60),
    );
    test_box.clone_of(&remote, "mover");

    let (id, view) = test_box.start_and_edit(&repo);
    let (code, submitted) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{submitted}");
    let executor = test_box.clone_of(&remote, "executor");
    let queued = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(
        queued["detail"]["ran"][0]["reason"], "PUBLICATION_QUEUED",
        "{queued}"
    );
    let landed = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(landed["detail"]["ran"][0]["decision"], "landed", "{landed}");

    let status = test_box.bay_ok(&repo, &["status", &id]);
    let change = &status["detail"]["changes"][0];
    let terminal = change["terminal"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["outcome"] == "published")
        .unwrap_or_else(|| panic!("no published terminal record: {status}"))
        .clone();
    assert_eq!(terminal["schema"], "bay.terminal/1");

    // Every field is there, and the times are in order.
    let submitted_at = at(&terminal, "submitted_at");
    let queued_at = at(&terminal, "queued_at");
    let head_at = at(&terminal, "head_at");
    let published_at = at(&terminal, "published_at");
    assert!(submitted_at <= queued_at, "{terminal}");
    assert!(queued_at <= head_at, "{terminal}");
    assert!(head_at <= published_at, "{terminal}");
    assert!(terminal.get("ended_at").is_none(), "{terminal}");

    // Three attempts: two refused by trunk moving, the third published as the head.
    assert_eq!(terminal["attempts"], 3, "{terminal}");
    let checks = terminal["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 3, "{terminal}");
    let mut seconds = 0;
    for attempt in checks {
        let started = at(attempt, "started_at");
        let finished = at(attempt, "finished_at");
        assert!(submitted_at <= started, "{attempt}");
        assert!(started <= finished && finished <= published_at, "{attempt}");
        assert_eq!(at(attempt, "seconds"), finished - started, "{attempt}");
        assert_eq!(attempt["verdict"], "pass", "{attempt}");
        seconds += finished - started;
    }
    // The published candidate's row names its evidence, with the worker time bay measured.
    let last = &checks[2];
    let run = &last["runs"][0];
    assert_eq!(run["check"], "tests", "{last}");
    assert_eq!(run["verdict"], "pass", "{last}");
    assert!(run["seconds"].is_number(), "{last}");
    assert!(
        head_at <= at(last, "started_at"),
        "the head's own check: {terminal}"
    );

    // Queue wait and check time are apart, and both fit in the total.
    let queue_wait_s = at(&terminal, "queue_wait_s");
    let checked_s = at(&terminal, "check_s");
    let total_s = at(&terminal, "total_s");
    assert_eq!(queue_wait_s, head_at - queued_at, "{terminal}");
    assert_eq!(checked_s, seconds, "{terminal}");
    assert_eq!(total_s, published_at - submitted_at, "{terminal}");
    assert!(queue_wait_s + checked_s <= total_s, "{terminal}");

    // The operation row of `bay status` shows the same timing.
    let timing = &change["operation"]["timing"];
    for field in [
        "submitted_at",
        "queued_at",
        "head_at",
        "published_at",
        "attempts",
        "checks",
        "queue_wait_s",
        "check_s",
        "total_s",
    ] {
        assert_eq!(timing[field], terminal[field], "{field}: {status}");
    }
}

/// The `git log` spawns of bay's own process in one `bay executor run --once` in `dir`, read
/// from the trace a test build writes (G171), and the run's result.
fn log_spawns(test_box: &TestBox, dir: &Path, name: &str) -> (usize, Value) {
    let trace = test_box.path().join(format!("{name}-trace.jsonl"));
    let (code, result) = result_json(
        test_box
            .bay(dir)
            .args(["executor", "run", "--once"])
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{result}");
    let text = std::fs::read_to_string(&trace).expect("the trace was written");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {line}")))
        .collect();
    let pid = lines.first().map(|line| line["pid"].clone());
    let logs = lines
        .iter()
        .filter(|line| Some(&line["pid"]) == pid.as_ref() && line["sub"] == "log")
        .count();
    (logs, result)
}

/// The heartbeat states on change `id`'s chain at the remote.
fn heartbeats(test_box: &TestBox, remote: &Path, id: &str) -> usize {
    test_box
        .git_bare(
            remote,
            &[
                "log",
                "--first-parent",
                "--format=%s",
                &format!("refs/bay/changes/{id}"),
            ],
        )
        .lines()
        .filter(|subject| subject.starts_with("heartbeat "))
        .count()
}

#[test]
fn the_timing_costs_the_same_spawns_however_many_heartbeats() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let runs = test_box.path().join("runs");
    // The second run checks for five seconds: with the one-second heartbeat the box seeds,
    // its chain holds several heartbeats more than the first's.
    let relayed = test_box.relay(&format!(
        "exec 3>&1 >/dev/null\n\
         echo x >> '{runs}'; n=$(wc -l < '{runs}' | tr -d ' '); \
         if [ \"$n\" -ge 2 ]; then sleep 5; fi\n\
         echo \"$n\" >&3",
        runs = runs.display(),
    ));
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files(&format!("n=$({relayed})\n"), &passing_result(1), 1, 60),
    );
    let executor = test_box.clone_of(&remote, "executor");

    let mut spawns = Vec::new();
    let mut beats = Vec::new();
    for name in ["quick", "slow"] {
        let (id, view) = test_box.start_and_edit(&repo);
        // Each change writes bytes of its own: a second `src/new.rs` equal to the landed one
        // merges to trunk's own tree and ends `no_effect` with no check run (G213).
        std::fs::write(
            view.join(format!("src/{name}.rs")),
            format!("fn {name}() {{}}\n"),
        )
        .unwrap();
        let (code, submitted) = result_json(test_box.bay(&view).arg("land"));
        assert_eq!(code, 0, "{submitted}");
        let (logs, result) = log_spawns(&test_box, &executor, name);
        assert_eq!(result["detail"]["ran"][0]["decision"], "landed", "{result}");
        spawns.push(logs);
        beats.push(heartbeats(&test_box, &remote, &id));
    }
    assert!(
        beats[1] >= beats[0] + 3,
        "the slow check made heartbeats: {beats:?}"
    );
    // A walk that read each chain state would spawn one more `git log` per heartbeat.
    assert!(
        spawns[1] <= spawns[0] + 1,
        "git log spawns {spawns:?} with heartbeats {beats:?}"
    );
}
