//! G568 and E36 (fleet findings, 2026-10-29). A fix answers a red post check only when it is
//! newer than the check's newest failure, and the check is then `pending_fix`, never green,
//! until a run at or after the fix answers. A fix may name any merge the red names.
//!
//! G568: a fix that names a red's culprit cleared it at its landing, and kept it clear when the
//! check then failed on the fix itself: the clear read every landing newer than the owner, not
//! newer than the newest failure. E36: with no culprit pinned, the owner is the newest failure,
//! so it moves at each failing run; a fix written for the owner of an hour before waited behind
//! the red it was written to clear (`MAIN_RED`), five times in 108 minutes.
//!
//! A fix may also name the check (`start --fixes <check>`), the one name that never moves: an
//! intermittent check that passes once between failures starts a new red, and the names of the
//! first red no longer answer it (ws-vr, 2026-10-29: three fixes for one cause, each name moved).
//!
//! Guards to mutate, in `src/red.rs`: `Red::answered_by` without `Suspect::failing` (the fix
//! for the earlier owner waits), its check-name match (the fix that names the check waits),
//! the `landings[..at]` bound of the fix search in `decide` (a fix older than the newest failure
//! clears it), the `PENDING_FIX` push (main reads green before a run answers the fix), and the
//! `fixing` push (E35: a change on the check's paths lands before the run on the fix). The
//! runner and the host's hook judge a repair by one function, `red::repairs`; the host judged
//! by the owner alone before, and refused the repair the runner ran from its own place. In
//! `src/publish.rs` `queue_gate`, the join of a repair with no place: it lands a pass later.
//! In `src/result.rs`, `main_say` out of the `Decision::Ok` sentences (status says nothing is
//! needed while main is red or a fix's run has not answered).

#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use serde_json::Value;
use support::queued::{Check, Held, OTHER_PRODUCER, Trust, finished, host_under};
use support::{PRODUCER, check_files, passing_result, result_json};

const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

/// A host whose post check `suite` on `src/**` is answered by records the test writes, as the
/// broad suite's own box writes them (G244), and an executor clone that runs no post check.
struct Suite {
    held: Held,
    exec: PathBuf,
    broad: PathBuf,
}

impl Suite {
    fn new() -> Self {
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
            // Unsigned, so the post records the suite box writes count.
            Trust::Unsigned,
        );
        let exec = held.executor("exec-a", PRODUCER);
        let suite = held.test_box.clone_of(&held.remote, "suite-box");
        let key = support::talk::fresh_key(&held.test_box, "suite-1");
        held.test_box
            .git_ok(&suite, &["config", "bay.producerName", "suite-1"]);
        held.test_box.git_ok(
            &suite,
            &["config", "bay.producerKey", key.to_str().unwrap()],
        );
        Self {
            held,
            exec,
            broad: suite,
        }
    }

    /// The suite answers `verdict` on main's tip.
    fn answer(&self, verdict: &str) {
        let test_box = &self.held.test_box;
        test_box.git_ok(&self.broad, &["fetch", "-q", "origin"]);
        let tip = test_box.git_bare(&self.held.remote, &["rev-parse", "main"]);
        let (code, written) = result_json(test_box.bay(&self.broad).args([
            "evidence",
            "record",
            "--kind",
            "check",
            "--check",
            "suite",
            "--environment",
            "local",
            "--verdict",
            verdict,
            "--commit",
            &tip,
        ]));
        assert_eq!(code, 0, "{written}");
    }

    /// Queues change `intent` on docs/, off the red's paths, and lands it.
    fn land_off_paths(&self, intent: &str) -> String {
        let file = format!("docs/{intent}.md");
        let id = self
            .held
            .queue_with(intent, &["docs"], &[(file.as_str(), "x\n")]);
        self.landed(&id);
        id
    }

    /// Submits a fix on src/ that names `fixes` in `--fixes`.
    fn submit_fix(&self, intent: &str, fixes: &str) -> String {
        let started = self.held.test_box.bay_ok(
            &self.held.repo,
            &["start", intent, "--paths", "src/", "--fixes", fixes],
        );
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(format!("src/{intent}.rs")), "fn fix() {}\n").unwrap();
        let (code, queued) = result_json(self.held.test_box.bay(&view).arg("merge"));
        assert_eq!(code, 0, "{queued}");
        id
    }

    /// Runs one pass and asserts that `id` is on main.
    fn landed(&self, id: &str) {
        let pass = self.held.pass(&self.exec);
        assert!(
            self.held
                .test_box
                .trunk_change_ids(&self.held.remote)
                .contains(&id.to_owned()),
            "{id} did not land: {pass}"
        );
    }

    fn status(&self) -> Value {
        self.held.test_box.bay_ok(&self.held.repo, &["status"])
    }
}

/// The `suite` rows of `status`'s `detail.red_pending`.
fn pending(status: &Value) -> Vec<Value> {
    status["detail"]["red_pending"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["check"] == "suite")
        .cloned()
        .collect()
}

#[test]
fn a_fix_for_an_earlier_owner_lands_after_the_owner_moves() {
    let suite = Suite::new();
    // `broke` lands on src/ and the suite fails; `one` lands off its paths and the suite fails
    // again, so `one` owns the red.
    let broke = suite.held.queue("broke", "src");
    suite.landed(&broke);
    suite.answer("product_failure");
    let one = suite.land_off_paths("one");
    suite.answer("product_failure");
    let red = suite.status()["detail"]["red"][0].clone();
    assert_eq!(
        red["change"],
        one.as_str(),
        "the newest failure owns it: {red}"
    );
    assert_eq!(
        red["fixes"],
        broke.as_str(),
        "a fix names the first failure, which stays: {red}"
    );

    // A seat writes its fix for the owner it read, `one`. Before it reaches the line, `two`
    // lands off the red's paths and the suite fails again: the owner moves to `two`.
    let two = suite.land_off_paths("two");
    suite.answer("product_failure");
    let red = suite.status()["detail"]["red"][0].clone();
    assert_eq!(red["change"], two.as_str(), "the owner moved: {red}");
    assert_eq!(red["fixes"], broke.as_str(), "the name to fix stays: {red}");
    // 0.6.3: a status read that describes no one change says main is broken, never that
    // nothing is needed.
    let observer = suite.held.test_box.clone_of(&suite.held.remote, "observer");
    let said = || -> String {
        let status = suite.held.test_box.bay_ok(&observer, &["status"]);
        assert!(status["change"].is_null(), "{status}");
        status["say"].as_str().unwrap().to_owned()
    };
    let say = said();
    assert!(say.starts_with("Main is broken on suite;"), "{say}");
    let fix = suite.submit_fix("fix", &one);
    suite.landed(&fix);

    // Its landing answers the red, and the check is pending on the fix's run, never green.
    let status = suite.status();
    assert_eq!(status["detail"]["red"], serde_json::json!([]), "{status}");
    let rows = pending(&status);
    assert_eq!(rows.len(), 1, "{status}");
    assert_eq!(rows[0]["state"], "pending_fix", "{status}");
    assert_eq!(rows[0]["fix"]["change"], fix.as_str(), "{status}");
    let say = said();
    assert!(say.starts_with("A fix for suite is on main"), "{say}");

    // The suite passes on the fix: main is green.
    suite.answer("pass");
    let status = suite.status();
    assert_eq!(status["detail"]["red"], serde_json::json!([]), "{status}");
    assert!(pending(&status).is_empty(), "{status}");
    let say = said();
    assert!(!say.contains("suite"), "{say}");
}

#[test]
fn a_fix_the_check_fails_on_leaves_main_red() {
    let suite = Suite::new();
    // The suite passes on `base`, so the failure on `broke` pins it as the culprit. The seed has
    // no parent, so no record judges it.
    suite.land_off_paths("base");
    suite.answer("pass");
    let broke = suite.held.queue("broke", "src");
    suite.landed(&broke);
    suite.answer("product_failure");
    let red = suite.status()["detail"]["red"][0].clone();
    assert_eq!(red["culprit"]["change"], broke.as_str(), "{red}");
    assert_eq!(red["fixes"], broke.as_str(), "{red}");

    let fix = suite.submit_fix("fix", &broke);
    suite.landed(&fix);
    let status = suite.status();
    assert_eq!(status["detail"]["red"], serde_json::json!([]), "{status}");
    assert_eq!(pending(&status)[0]["state"], "pending_fix", "{status}");

    // The suite fails on the fix itself. The fix landed before that run, so it answers nothing,
    // and main is red again.
    suite.answer("product_failure");
    let status = suite.status();
    let red = &status["detail"]["red"];
    assert_eq!(
        red.as_array().map(Vec::len),
        Some(1),
        "a fix older than the newest failure cleared the red: {status}"
    );
    assert_eq!(red[0]["check"], "suite", "{status}");
    assert!(pending(&status).is_empty(), "{status}");

    // A second fix, newer than the failure on the first, answers it.
    let again = suite.submit_fix("again", &broke);
    suite.landed(&again);
    let status = suite.status();
    assert_eq!(status["detail"]["red"], serde_json::json!([]), "{status}");
    assert_eq!(
        pending(&status)[0]["fix"]["change"],
        again.as_str(),
        "{status}"
    );
}

/// E35 (a fleet finding, 2026-10-29: a fix that answered one failing test of seven let every
/// change on the check's paths land unchecked until the next run, 34 minutes later, failed
/// again). While the check is `pending_fix`, a change on its paths that does not name the red
/// waits for the run on the fix; a change off its paths lands; after a pass, the change lands.
#[test]
fn a_change_on_the_red_paths_waits_for_the_run_on_the_fix() {
    let suite = Suite::new();
    suite.land_off_paths("base");
    suite.answer("pass");
    let broke = suite.held.queue("broke", "src");
    suite.landed(&broke);
    suite.answer("product_failure");
    let fix = suite.submit_fix("fix", &broke);
    suite.landed(&fix);
    assert_eq!(pending(&suite.status())[0]["state"], "pending_fix");

    let next = suite.held.queue("next", "src");
    let pass = suite.held.pass(&suite.exec);
    let test_box = &suite.held.test_box;
    assert!(
        !test_box
            .trunk_change_ids(&suite.held.remote)
            .contains(&next),
        "a change on the check's paths landed before the run on its fix: {pass}"
    );
    let waiting = test_box.bay_ok(&suite.held.repo, &["status", &next]);
    assert!(
        waiting.to_string().contains("MAIN_RED"),
        "the change does not say main's check holds it: {waiting}"
    );
    suite.land_off_paths("meanwhile");

    suite.answer("pass");
    suite.landed(&next);
}

#[test]
fn a_fix_that_names_the_check_runs_from_its_place_after_a_pass_moves_the_red() {
    let suite = Suite::new();
    suite.land_off_paths("base");
    suite.answer("pass");
    let broke = suite.held.queue("broke", "src");
    suite.landed(&broke);
    suite.answer("product_failure");
    assert_eq!(suite.status()["detail"]["red"][0]["fixes"], broke.as_str());
    // The check passes once between failures, so a new red starts at `two`: every name the
    // first red gave has moved.
    suite.land_off_paths("one");
    suite.answer("pass");
    let two = suite.land_off_paths("two");
    suite.answer("product_failure");
    let red = suite.status()["detail"]["red"][0].clone();
    assert_eq!(red["check"], "suite", "{red}");
    assert_eq!(red["fixes"], two.as_str(), "the red's name moved: {red}");

    // A change on the red's paths waits at the head of the line for main to be green.
    let waits = suite.held.queue("waits", "src");
    let pass = suite.held.pass(&suite.exec);
    let trunk = || suite.held.test_box.trunk_change_ids(&suite.held.remote);
    assert!(
        !trunk().contains(&waits),
        "a change on a red main merged: {pass}"
    );

    // The fix that names the check takes a place behind it and runs from there in the same
    // visit, and the host takes it.
    let fix = suite.submit_fix("fix", "suite");
    suite.landed(&fix);
    let status = suite.status();
    assert_eq!(status["detail"]["red"], serde_json::json!([]), "{status}");
    let rows = pending(&status);
    assert_eq!(rows[0]["state"], "pending_fix", "{status}");
    assert_eq!(rows[0]["fix"]["change"], fix.as_str(), "{status}");
}

#[test]
fn the_host_takes_a_fix_named_by_its_check_past_a_head_in_its_check() {
    let suite = Suite::new();
    suite.land_off_paths("base");
    suite.answer("pass");
    let broke = suite.held.queue("broke", "src");
    suite.landed(&broke);
    suite.answer("product_failure");
    // The head is off the red's paths and in its check on one runner, so main's red holds no
    // place ahead of the fix: only the repair rule lets the host take the fix past it.
    let ahead = suite
        .held
        .queue_with("ahead", &["docs"], &[("docs/ahead.md", "x\n")]);
    let runs = std::fs::read_to_string(&suite.held.runs).unwrap_or_default();
    let n = u32::try_from(runs.lines().count()).unwrap() + 1;
    suite.held.hold_run(n);
    let runner = suite.held.spawn(&suite.exec);
    suite.held.wait_held_run(n);
    let fix = suite.submit_fix("fix", "suite");
    let other = suite.held.executor("exec-b", OTHER_PRODUCER);
    let pass = suite.held.pass(&other);
    let trunk = suite.held.test_box.trunk_change_ids(&suite.held.remote);
    suite.held.go_run(n);
    let first = finished(runner);
    assert!(
        trunk.contains(&fix),
        "the host refused the fix past a head in its check: {pass}"
    );
    assert!(!trunk.contains(&ahead), "{first}");
}

#[test]
fn a_fix_names_a_post_check_of_mains_policy_or_nothing_is_created() {
    let suite = Suite::new();
    // An unknown name, and the policy's pre-merge check `tests`: a red is only a post check's.
    for named in ["no-such-check", "tests"] {
        let (code, refused) = result_json(
            suite
                .held
                .test_box
                .bay(&suite.held.repo)
                .args(["start", "fix", "--paths", "src/", "--fixes", named]),
        );
        assert_eq!(code, 20, "{refused}");
        let detail = refused["detail"].as_str().unwrap_or_default();
        assert!(
            detail.contains("no post check by that name") && detail.contains("suite"),
            "the refusal does not name main's post checks: {refused}"
        );
    }
    let fixed = suite.held.test_box.bay_ok(
        &suite.held.repo,
        &["start", "fix", "--paths", "src/", "--fixes", "suite"],
    );
    assert_eq!(fixed["detail"]["fixes"], "suite", "{fixed}");
}
