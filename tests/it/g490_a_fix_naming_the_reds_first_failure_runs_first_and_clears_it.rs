//! G490 (a fleet finding, 2026-10-28 22:45Z: nothing landed for 25 minutes while the only
//! changes waiting were three fixes held by the red main). A fix that names the red's first
//! failure runs ahead of the line, and its landing clears the red.
//!
//! With no culprit pinned, a red post check is owned by its newest failure: the landing the
//! check last ran on, which moves at every run. The first failure of its suspect range, the
//! change bay names as the suspect, does not. A fix that named it waited behind the red it was
//! written to clear (`MAIN_RED`), and nothing else could clear that red.
//!
//! Guards to mutate: `src/red.rs` `Red::answered_by` without the first failure (the fix waits),
//! and `red`'s clear by a later landing that names the first failure (main stays red).

#![cfg(unix)]

use crate::support;

use support::queued::{Check, Trust, host_under};
use support::{PRODUCER, check_files, passing_result, result_json};

const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

#[test]
fn a_fix_naming_the_first_failure_lands_ahead_and_main_is_green() {
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
            fails: "false",
        },
        // Unsigned, so the post records the suite box writes count, as an unsigned host takes them.
        Trust::Unsigned,
    );
    let a = held.executor("exec-a", PRODUCER);
    // The broad suite runs elsewhere and writes its records as producer `suite-1` (G244).
    let suite = held.test_box.clone_of(&held.remote, "suite-box");
    let key = support::talk::fresh_key(&held.test_box, "suite-1");
    held.test_box
        .git_ok(&suite, &["config", "bay.producerName", "suite-1"]);
    held.test_box.git_ok(
        &suite,
        &["config", "bay.producerKey", key.to_str().unwrap()],
    );
    let fail_at_tip = || {
        held.test_box.git_ok(&suite, &["fetch", "-q", "origin"]);
        let tip = held.test_box.git_bare(&held.remote, &["rev-parse", "main"]);
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
            &tip,
        ]));
        assert_eq!(code, 0, "{written}");
    };

    // `broke` lands on src/ and its post check fails; then `later` lands on docs/, off the red's
    // paths, and the suite fails again on that tip. The newest failure, `later`, owns the red;
    // the first failure is `broke`.
    let broke = held.queue("broke", "src");
    let pass = held.pass(&a);
    assert_eq!(
        held.test_box.trunk_change_ids(&held.remote),
        vec![broke.clone()],
        "{pass}"
    );
    fail_at_tip();
    let later = held.queue_with("later", &["docs"], &[("docs/later.md", "later\n")]);
    let pass = held.pass(&a);
    assert!(
        held.test_box
            .trunk_change_ids(&held.remote)
            .contains(&later),
        "a change off the red's paths lands: {pass}"
    );
    fail_at_tip();
    let status = held.test_box.bay_ok(&held.repo, &["status"]);
    let red = &status["detail"]["red"][0];
    assert_eq!(
        red["change"],
        later.as_str(),
        "the newest failure owns it: {red}"
    );
    assert_eq!(
        red["suspect"]["first_failed"]["change"],
        broke.as_str(),
        "the first failure is broke: {red}"
    );

    // The fix names the first failure.
    let started = held.test_box.bay_ok(
        &held.repo,
        &["start", "fix", "--paths", "src/", "--fixes", &broke],
    );
    let fix = started["change"].as_str().unwrap().to_owned();
    let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/fix.rs"), "fn fix() {}\n").unwrap();
    let (code, queued) = result_json(held.test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let pass = held.pass(&a);
    assert!(
        held.test_box.trunk_change_ids(&held.remote).contains(&fix),
        "the fix waited behind the red it clears: {pass}"
    );
    let status = held.test_box.bay_ok(&held.repo, &["status"]);
    assert_eq!(
        status["detail"]["red"],
        serde_json::json!([]),
        "the fix's landing clears the red: {status}"
    );
}
