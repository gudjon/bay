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
//! Guards to mutate, in `src/red.rs`: `Red::answered_by` without `Suspect::failing` (the fix
//! for the earlier owner waits), the `landings[..at]` bound of the fix search in `decide` (a
//! fix older than the newest failure clears it), and the `PENDING_FIX` push (main reads green
//! before a run answers the fix).

#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use serde_json::Value;
use support::queued::{Check, Held, Trust, host_under};
use support::{PRODUCER, check_files, passing_result, result_json};

const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

/// A host whose post check `suite` on `src/**` is answered by records the test writes, as the
/// broad suite's own box writes them (G244), and an executor clone that runs no post check.
struct Suite {
    held: Held,
    exec: PathBuf,
    suite: PathBuf,
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
        Self { held, exec, suite }
    }

    /// The suite answers `verdict` on main's tip.
    fn answer(&self, verdict: &str) {
        let test_box = &self.held.test_box;
        test_box.git_ok(&self.suite, &["fetch", "-q", "origin"]);
        let tip = test_box.git_bare(&self.held.remote, &["rev-parse", "main"]);
        let (code, written) = result_json(test_box.bay(&self.suite).args([
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
    assert_eq!(red["change"], one.as_str(), "the newest failure owns it: {red}");
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
    let fix = suite.submit_fix("fix", &one);
    suite.landed(&fix);

    // Its landing answers the red, and the check is pending on the fix's run, never green.
    let status = suite.status();
    assert_eq!(status["detail"]["red"], serde_json::json!([]), "{status}");
    let rows = pending(&status);
    assert_eq!(rows.len(), 1, "{status}");
    assert_eq!(rows[0]["state"], "pending_fix", "{status}");
    assert_eq!(rows[0]["fix"]["change"], fix.as_str(), "{status}");

    // The suite passes on the fix: main is green.
    suite.answer("pass");
    let status = suite.status();
    assert_eq!(status["detail"]["red"], serde_json::json!([]), "{status}");
    assert!(pending(&status).is_empty(), "{status}");
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
    assert_eq!(pending(&status)[0]["fix"]["change"], again.as_str(), "{status}");
}
