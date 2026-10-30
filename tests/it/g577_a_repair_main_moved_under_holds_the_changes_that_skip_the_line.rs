//! G577 (BACKLOG 139, a fleet finding, 2026-10-30: the repair 3t7jrfsw of a red post check was
//! refused "main moved" on four tries while documents and changes past the red's places landed
//! under it). Once main moved under a running repair, a change that needs no check keeps its
//! place until the repair lands, as G552 holds it for the head.
//!
//! The head waits on the red, so G552 held nothing: its push was never refused. The repair runs
//! from its own place behind it, and each document that landed during its check moved main
//! under it again.
//!
//! Guard to mutate, in `src/publish.rs`: `repair_moved_under` returning `false` (the second
//! document lands while the repair checks again).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use support::queued::{Check, Held, OTHER_PRODUCER, Trust, finished, host_under};
use support::{PRODUCER, check_files, passing_result, result_json};

/// The post check `web` on `web/**`.
const POST: &str = "\n[[check]]\nid = \"web\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"web/**\"]\n";

/// Post check `web` answers `verdict` on main's tip, as its own box records it (G244).
fn answer(held: &Held, broad: &Path, verdict: &str) {
    let test_box = &held.test_box;
    test_box.git_ok(broad, &["fetch", "-q", "origin"]);
    let tip = test_box.git_bare(&held.remote, &["rev-parse", "main"]);
    let (code, written) = result_json(test_box.bay(broad).args([
        "evidence",
        "record",
        "--kind",
        "check",
        "--check",
        "web",
        "--environment",
        "local",
        "--verdict",
        verdict,
        "--version",
        "web",
        "--commit",
        tip.trim(),
    ]));
    assert_eq!(code, 0, "{written}");
}

/// Submits a change on `web/` and `src/` that fixes `fixes`, and puts it in the queue behind
/// every place already there, as an executor would. Returns its change id.
fn repair(held: &Held, fixes: &str) -> String {
    let started = held.test_box.bay_ok(
        &held.repo,
        &[
            "start", "repair", "--paths", "web/", "src/", "--fixes", fixes,
        ],
    );
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("web/repair.ts"), "x\n").unwrap();
    std::fs::write(view.join("src/repair.rs"), "fn repair() {}\n").unwrap();
    let (code, submitted) = result_json(held.test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");
    let op_id = submitted["op_id"].as_str().unwrap();
    let opened = bay::repo::Repo::open(&held.repo).unwrap();
    bay::queue::join(&opened, None, op_id, &id).unwrap();
    id
}

fn on_main(held: &Held, id: &str) -> bool {
    held.test_box
        .trunk_change_ids(&held.remote)
        .contains(&id.to_owned())
}

#[test]
fn a_document_waits_for_a_repair_that_main_moved_under() {
    // One pre-merge check, required only for src/, and the post check `web` on web/.
    let policy = check_files("x", &passing_result(1), 1, 120)
        .into_iter()
        .find(|(path, _)| path == ".bay/policy.toml")
        .map(|(_, text)| {
            let text = text.replacen("timeout_s", "when_paths = [\"src/**\"]\ntimeout_s", 1);
            format!("{text}{POST}")
        })
        .unwrap();
    let held = host_under(
        &Check {
            config: "[runner]\nlease_s = 30\nheartbeat_s = 1\nahead = 4\n",
            policy: Some(&policy),
            seed: &[
                ("src/a.rs", "fn a() {}\n"),
                ("web/a.ts", "a\n"),
                ("docs/a.md", "a\n"),
            ],
            hold_first: true,
            fails: "false",
        },
        // Unsigned, so the post records the web box writes count.
        Trust::Unsigned,
    );
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    let broad = held.test_box.clone_of(&held.remote, "web-box");
    let key = support::talk::fresh_key(&held.test_box, "web-1");
    held.test_box
        .git_ok(&broad, &["config", "bay.producerName", "web-1"]);
    held.test_box.git_ok(
        &broad,
        &["config", "bay.producerKey", key.to_str().unwrap()],
    );

    // GIVEN `broke`, on web/, turned `web` red after it passed on the landing before.
    let base = held.queue_with("base", &["docs"], &[("docs/base.md", "base\n")]);
    let pass = held.pass(&b);
    assert!(on_main(&held, &base), "{pass}");
    answer(&held, &broad, "pass");
    let broke = held.queue_with("broke", &["web"], &[("web/broke.ts", "broke\n")]);
    let pass = held.pass(&b);
    assert!(on_main(&held, &broke), "{pass}");
    answer(&held, &broad, "product_failure");

    // AND a head on web/ that waits on the red, and a repair of it whose check is held.
    let waits = held.queue_with("waits", &["web"], &[("web/waits.ts", "waits\n")]);
    let fix = repair(&held, &broke);
    held.hold_run(2);
    let runner_a = held.spawn(&a);
    held.wait_held();
    std::thread::sleep(Duration::from_secs(1));

    // WHEN a document lands during the repair's first check THEN main moved under the repair.
    let first = held.queue_with("first", &["docs"], &[("docs/first.md", "first\n")]);
    let pass = held.pass(&b);
    assert!(
        on_main(&held, &first),
        "the first document waited for a repair main had not moved under: {pass}"
    );
    held.go();
    // The repair's push found main moved: it gave its claim back and kept its place.
    let ran = finished(runner_a);
    assert!(
        !on_main(&held, &fix),
        "the repair landed on its first check: {ran}"
    );
    let runner_a = held.spawn(&a);
    held.wait_held_run(2);

    // THEN a second document keeps its place while the repair checks again.
    let second = held.queue_with("second", &["docs"], &[("docs/second.md", "second\n")]);
    let pass = held.pass(&b);
    assert!(
        !on_main(&held, &second),
        "the second document moved main under the repair again: {pass}"
    );
    held.go_run(2);
    let ran = finished(runner_a);
    assert!(
        on_main(&held, &fix),
        "the repair did not land on its second check: {ran}"
    );
    assert!(!on_main(&held, &waits), "the head landed on a red check");

    // AND once the repair left the line, the second document lands.
    let pass = held.pass(&b);
    assert!(
        on_main(&held, &second),
        "the second document never landed: {pass}"
    );
}
