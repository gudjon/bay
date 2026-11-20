//! G814 (a fleet measurement, 2026-11-20: two runners on one box visited each waiting change
//! that names one it supersedes, fixes or reverts every pass, a visit every 100 s, 53 of 119
//! visits in an hour from 8 of 31 waiting changes, while main held no red any of them
//! answered). A looping runner parks such a change answered `QUEUED` as any other, and visits
//! it again once main holds a red it answers: then it runs ahead of the line, while the head
//! still checks.
//!
//! Guards to mutate, in `src/executor.rs`: the pass's parked filter exempting a change that
//! names a repair (it is visited every pass); `answered` returning `false` (the red main never
//! frees it, and it waits behind the head).

#![cfg(unix)]

use crate::support;

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use support::queued::{Check, Held, OTHER_PRODUCER, Trust, finished, host_under};
use support::{PRODUCER, check_files, passing_result, result_json};

const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

/// The journal lines so far that name operation `op_id`.
fn lines_of(journal: &Mutex<Vec<String>>, op_id: &str) -> Vec<String> {
    journal
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains(&format!("{op_id} ")))
        .cloned()
        .collect()
}

/// Waits up to `limit` for `done`.
fn until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let began = Instant::now();
    while began.elapsed() < limit {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    done()
}

/// A looping runner in `clone`, its journal (stderr) gathered line by line.
fn looping(held: &Held, clone: &Path) -> (std::process::Child, Arc<Mutex<Vec<String>>>) {
    let mut child = held
        .test_box
        .bay_process(clone)
        .args(["--json", "runner", "run", "--interval-s", "1"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let journal = Arc::new(Mutex::new(Vec::new()));
    let gathered = Arc::clone(&journal);
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            gathered.lock().unwrap().push(line);
        }
    });
    (child, journal)
}

#[test]
fn a_waiting_repair_is_visited_again_only_when_main_holds_a_red_it_answers() {
    let policy = check_files("x", &passing_result(1), 1, 120)
        .into_iter()
        .find(|(path, _)| path == ".bay/policy.toml")
        .map(|(_, text)| format!("{text}{POST}"))
        .unwrap();
    let held = host_under(
        &Check {
            config: "[runner]\nlease_s = 60\nheartbeat_s = 1\nahead = 4\n",
            policy: Some(&policy),
            seed: &[("src/a.rs", "fn a() {}\n"), ("docs/a.md", "a\n")],
            hold_first: false,
            fails: "false",
        },
        // Unsigned, so the post record a third clone writes counts, as an unsigned host takes it.
        Trust::Unsigned,
    );
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);

    // `broke` lands; main is green: no post record names it yet.
    let broke = held.queue("broke", "src");
    let pass = held.pass(&a);
    assert_eq!(
        held.test_box.trunk_change_ids(&held.remote),
        vec![broke.clone()],
        "{pass}"
    );
    let landing = held.test_box.git_bare(&held.remote, &["rev-parse", "main"]);

    // A change that names `broke` as the one it fixes waits behind a head.
    let slow = held.queue_with("slow", &["docs"], &[("docs/slow.md", "slow\n")]);
    let started = held.test_box.bay_ok(
        &held.repo,
        &["start", "fix", "--paths", "src/", "--fixes", &broke],
    );
    let fix = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/fix.rs"), "fn fix() {}\n").unwrap();
    let (code, queued) = result_json(held.test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let op_id = queued["op_id"].as_str().unwrap().to_owned();
    let opened = bay::repo::Repo::open(&held.repo).unwrap();
    bay::queue::join(&opened, None, &op_id, &fix).unwrap();

    // One runner holds the head: its check waits until the test lets it go. Its pass places the
    // fix before it, so no arrival the head's check places moves the fix's chain later.
    let runs = std::fs::read_to_string(&held.runs).map_or(0, |text| text.lines().count());
    let head_run = u32::try_from(runs).unwrap() + 1;
    held.hold_run(head_run);
    let runner_a = held.spawn(&a);
    held.wait_held_run(head_run);

    // WHEN a second runner loops while the head checks and main holds no red
    let (mut child, journal) = looping(&held, &b);
    assert!(
        until(Duration::from_secs(60), || !lines_of(&journal, &op_id)
            .is_empty()),
        "the runner never visited the waiting fix: {:?}",
        journal.lock().unwrap()
    );
    std::thread::sleep(Duration::from_secs(8));

    // THEN it visited the fix once, though it passed many times
    let visits = lines_of(&journal, &op_id);
    assert_eq!(
        visits.len(),
        1,
        "a waiting fix was visited again while nothing moved: {visits:?}"
    );
    assert!(visits[0].contains("QUEUED"), "{visits:?}");

    // AND once main holds a red the fix answers, a suite's post check failed on `broke`'s
    // landing, it is visited again and lands ahead of the head, which still checks
    let suite = held.test_box.clone_of(&held.remote, "suite-box");
    let key = support::talk::fresh_key(&held.test_box, "suite-1");
    held.test_box
        .git_ok(&suite, &["config", "bay.producerName", "suite-1"]);
    held.test_box.git_ok(
        &suite,
        &["config", "bay.producerKey", key.to_str().unwrap()],
    );
    let (code, written) = result_json(held.test_box.bay(&suite).args([
        "evidence",
        "record",
        "--kind",
        "check",
        "--check",
        "suite",
        "--environment",
        "local",
        "--verdict",
        "product_failure",
        "--commit",
        &landing,
    ]));
    assert_eq!(code, 0, "{written}");
    let landed = until(Duration::from_secs(60), || {
        held.test_box.trunk_change_ids(&held.remote).contains(&fix)
    });
    let merged = held.test_box.trunk_change_ids(&held.remote);
    held.go_run(head_run);
    let _ = finished(runner_a);
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        landed,
        "the fix waited behind the head though main held a red it answers: {merged:?} {:?}",
        journal.lock().unwrap()
    );
    // The landings are listed newest first: the head, once let go, lands after the fix.
    let at = |id: &String| merged.iter().position(|landed| landed == id);
    assert!(
        at(&slow).is_none_or(|slow| at(&fix).is_some_and(|fix| fix > slow)),
        "the fix did not land ahead of the head: {merged:?}"
    );
}
