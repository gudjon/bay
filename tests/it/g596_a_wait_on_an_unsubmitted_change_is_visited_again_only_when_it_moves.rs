//! G596 (BACKLOG 157; a fleet finding, 2026-10-30: an operation waited on a change no merge was
//! asked for, and 39% of one runner's journal lines were that wait, each saying only that it
//! waits). A change it lands after that has no operation under way is not submitted: the wait's
//! `say`, its `next_step`, its `detail.pending` and the runner's journal line name that change
//! and its actor, who must `bay merge` it. A looping runner visits such a wait once, and again
//! only once a chain it read moved: when that change is submitted, the wait is visited, and
//! the dependent lands after it.
//!
//! Guards to mutate: `src/executor.rs` `pass`: drop the `parked.holds` filter, and the wait is
//! visited each pass; `Parked::keep`: keep every wait, and the dependent never lands;
//! `src/after.rs` `submitted`: answer `true`, and nothing names the change as not submitted;
//! `src/result.rs` `Reason::say`: drop the `DependencyPending` branch, and the say names nobody.

#![cfg(unix)]

use crate::support;

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use support::queued::{Check, Held, host};
use support::{PRODUCER, result_json};

/// Starts change `intent` on `src/` landing after each of `after`, and writes `file` in its
/// view. Returns (id, view).
fn start_writing(held: &Held, intent: &str, file: &str, after: &[&str]) -> (String, PathBuf) {
    let mut args = vec!["start", intent, "--paths", "src/"];
    for dependency in after {
        args.extend(["--after", *dependency]);
    }
    let started = held.test_box.bay_ok(&held.repo, &args);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(file), format!("// {file}\n")).unwrap();
    (started["change"].as_str().unwrap().to_owned(), view)
}

/// The change record at the host's head of change `id`'s chain.
fn host_record(held: &Held, id: &str) -> Value {
    let text = held.test_box.git_bare(
        &held.remote,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    serde_json::from_str(&text).unwrap()
}

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
fn a_wait_on_a_change_nobody_submitted_names_it_and_is_visited_again_once_it_moves() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let (first, first_view) = start_writing(&held, "CARD-5961", "src/first.rs", &[]);
    let (second, second_view) =
        start_writing(&held, "CARD-5962", "src/second.rs", &[first.as_str()]);
    let actor = host_record(&held, &first)["actor"]["name"]
        .as_str()
        .unwrap()
        .to_owned();

    // The dependent is submitted; its dependency is not.
    let (code, waiting) = result_json(held.test_box.bay(&second_view).arg("merge"));
    assert_eq!(code, 0, "{waiting}");
    assert_eq!(waiting["reason"], "DEPENDENCY_PENDING", "{waiting}");
    let named = &waiting["detail"]["pending"][0];
    assert_eq!(named["change"], first.as_str(), "{waiting}");
    assert_eq!(named["submitted"], false, "{waiting}");
    assert_eq!(named["actor"], actor.as_str(), "{waiting}");
    let say = waiting["say"].as_str().unwrap_or_default();
    assert!(
        say.contains(&first) && say.contains(&actor) && say.contains("bay merge"),
        "the say names the change nobody submitted and who sends it: {waiting}"
    );
    assert!(
        waiting["next_step"]
            .as_str()
            .is_some_and(|step| step.contains("submitted: false")),
        "{waiting}"
    );
    let op_id = waiting["op_id"].as_str().unwrap().to_owned();

    // A looping runner visits the wait once, naming the change and its actor, and not again
    // while nothing it read moves.
    let runner = held.executor("runner", PRODUCER);
    let (mut child, journal) = looping(&held, &runner);
    let visited = || !lines_of(&journal, &op_id).is_empty();
    assert!(
        until(Duration::from_secs(60), visited),
        "the runner never visited the wait: {:?}",
        journal.lock().unwrap()
    );
    std::thread::sleep(Duration::from_secs(8));
    let visits = lines_of(&journal, &op_id);
    assert_eq!(
        visits.len(),
        1,
        "a wait nothing can free was visited again: {visits:?}"
    );
    assert!(
        visits[0].contains("DEPENDENCY_PENDING")
            && visits[0].contains(&format!(
                "{first} is not submitted: its actor {actor} must run bay merge for it"
            )),
        "{visits:?}"
    );

    // The dependency is submitted: its chain moves, the wait is visited again, and the
    // dependent lands after it.
    let (code, submitted) = result_json(held.test_box.bay(&first_view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");
    let landed = until(Duration::from_secs(120), || {
        let ids = held.test_box.trunk_change_ids(&held.remote);
        ids.len() >= 2 && ids[..2] == [second.as_str(), first.as_str()]
    });
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        landed,
        "the dependent did not land after its dependency: {:?} {:?}",
        held.test_box.trunk_change_ids(&held.remote),
        journal.lock().unwrap()
    );
    assert!(
        lines_of(&journal, &op_id).len() > 1,
        "the wait was not visited again: {:?}",
        journal.lock().unwrap()
    );
}
