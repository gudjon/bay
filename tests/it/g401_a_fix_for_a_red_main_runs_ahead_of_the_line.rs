//! G401 (the trust-agents ruling, 2026-10-26; a fleet finding: the fix for a red main sat at
//! place 115, about 119 hours out, while every change on the red paths was refused). Repair
//! first: while main is red, a change that names the red's owning change runs as a head from
//! its own place and publishes ahead of the line, and the host takes that publication. Main's
//! state chooses it, never a priority list.
//!
//! Guard to mutate, in `src/publish.rs`: `repairs_red` returning `false`.

#![cfg(unix)]

use crate::support;

use std::time::Duration;

use support::queued::{Check, OTHER_PRODUCER, Trust, finished, host_under};
use support::{PRODUCER, check_files, passing_result, result_json};

const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

#[test]
fn a_change_that_fixes_the_red_publishes_while_the_head_still_runs() {
    let policy = check_files("x", &passing_result(1), 1, 120)
        .into_iter()
        .find(|(path, _)| path == ".bay/policy.toml")
        .map(|(_, text)| format!("{text}{POST}"))
        .unwrap();
    let held = host_under(
        &Check {
            config: "[runner]\nlease_s = 30\nheartbeat_s = 1\n",
            policy: Some(&policy),
            seed: &[("src/a.rs", "fn a() {}\n"), ("docs/a.md", "a\n")],
            hold_first: false,
            // A candidate holding docs/slow.md takes a while, so its head is busy when the fix comes.
            fails: "{ [ -f docs/slow.md ] && sleep 20; false; }",
        },
        // Unsigned, so the post record any clone writes counts, as an unsigned host takes it.
        Trust::Unsigned,
    );
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);

    // `broke` lands, and its post check fails: main is red on src/, owned by `broke`.
    let broke = held.queue("broke", "src");
    let pass = held.pass(&a);
    assert_eq!(
        held.test_box.trunk_change_ids(&held.remote),
        vec![broke.clone()],
        "{pass}"
    );
    let landing = held.test_box.git_bare(&held.remote, &["rev-parse", "main"]);
    // The broad suite runs elsewhere and writes its records as producer `suite-1` (G244).
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
    let status = held.test_box.bay_ok(&held.repo, &["status"]);
    assert_eq!(
        status["detail"]["red"][0]["change"],
        broke.as_str(),
        "main is red, owned by broke: red {} pending {} written {written}",
        status["detail"]["red"],
        status["detail"]["red_pending"]
    );

    // The line: a slow head, another change, and the fix at the back.
    let slow = held.queue_with("slow", &["docs"], &[("docs/slow.md", "slow\n")]);
    let _other = held.queue_with("other", &["docs"], &[("docs/other.md", "other\n")]);
    let started = held.test_box.bay_ok(
        &held.repo,
        &["start", "fix", "--paths", "src/", "--fixes", &broke],
    );
    let fix = started["change"].as_str().unwrap().to_owned();
    let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/fix.rs"), "fn fix() {}\n").unwrap();
    let (code, queued) = result_json(held.test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let op = queued["op_id"].as_str().unwrap().to_owned();
    let opened = bay::repo::Repo::open(&held.repo).unwrap();
    bay::queue::join(&opened, None, &op, &fix).unwrap();

    let runner_a = held.spawn(&a);
    std::thread::sleep(Duration::from_secs(3));
    let pass = held.pass(&b);
    let merged = held.test_box.trunk_change_ids(&held.remote);
    assert!(
        merged.contains(&fix),
        "the fix waited behind the line: {pass}"
    );
    assert!(
        !merged.contains(&slow),
        "the head was still running: {merged:?}"
    );
    let _ = finished(runner_a);
}
