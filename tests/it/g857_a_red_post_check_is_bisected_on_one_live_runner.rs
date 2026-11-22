//! G857 (a fleet finding, 2026-11-22: two runners that served one environment each ran the same
//! bisect runs of a red post check, about 40 % of its bisect runs in an afternoon). Of the live
//! runners that serve a red post check's environment, one bisects it: the one G822 chooses to run
//! the check on main's tip. Another bisects it only when that runner's presence reads stale, or
//! when no producer answered the check in the red's range for twice its timeout.
//!
//! Guards to mutate: the `bisected_by` call in `bisect::culprits` (`src/bisect.rs`); drop it and
//! the runner that does not own the check bisects beside its live owner. The timeout test in
//! `bisect::bisected_by`; drop it and a red its live owner left unanswered waits on that owner.

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{EXECUTOR, result_json};

use crate::g244d_a_red_post_check_names_its_culprit_by_bisection::{Trunk, trunk_with};
use crate::g822_a_post_check_runs_on_one_live_runner::weight;

/// A trunk whose post check `suite` (timeout 60 s) fails from its fourth landing on, its box, and
/// a second runner clone beside it, both serving the check's environment. The suite passed on the
/// first of five landings; nothing has failed yet.
struct Two {
    trunk: Trunk,
    /// The runner that owns `suite` (G822), its clone and its name.
    owner: (PathBuf, &'static str),
    /// The runner that does not.
    other: PathBuf,
    landings: Vec<(String, String)>,
}

fn two() -> Two {
    let trunk = trunk_with("[runner]\nbisect_post = true\n", &[]);
    let remote = trunk.test_box.path().join("repo-remote.git");
    trunk
        .test_box
        .git_ok(&trunk.repo, &["config", "bay.runnerName", EXECUTOR]);
    let second = trunk.test_box.clone_of(&remote, "other");
    trunk
        .test_box
        .git_ok(&second, &["config", "bay.runnerName", "other"]);
    trunk
        .test_box
        .git_ok(&second, &["config", "bay.producerName", "other-checks"]);
    let landings: Vec<(String, String)> = (0..5)
        .map(|n| trunk.land(&format!("src/l{n}.rs")))
        .collect();
    trunk.suite(&landings[0].1, "pass");
    let (owner, other) = if weight(EXECUTOR) > weight("other") {
        ((trunk.repo.clone(), EXECUTOR), second)
    } else {
        ((second, "other"), trunk.repo.clone())
    };
    Two {
        trunk,
        owner,
        other,
        landings,
    }
}

impl Two {
    /// The owner writes its presence, seconds between its passes `interval_s`; then the suite
    /// fails on the newest landing, so the red's range is the four after the first.
    fn red_after_the_owner_was_seen(&self, interval_s: u64) {
        assert_eq!(self.pass(&self.owner.0, 0, interval_s), None, "no red yet");
        self.trunk.suite(&self.landings[4].1, "product_failure");
    }

    /// One runner pass in `dir`, its reader's clock `skew_s` ahead: its `bisect` report.
    fn pass(&self, dir: &Path, skew_s: u64, interval_s: u64) -> Option<Value> {
        let interval = interval_s.to_string();
        let (code, ran) = result_json(
            self.trunk
                .test_box
                .bay(dir)
                .args(["runner", "run", "--once", "--interval-s", &interval])
                .env("BAY_TEST_PRESENCE_CLOCK_SKEW_S", skew_s.to_string()),
        );
        assert_eq!(code, 0, "{ran}");
        ran["detail"]["ran"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|entry| entry.get("bisect").cloned())
            .filter(|report| report.as_array().is_some_and(|rows| !rows.is_empty()))
    }
}

#[test]
fn a_red_is_bisected_by_its_owner_alone_while_the_owner_is_live() {
    let two = two();
    two.red_after_the_owner_was_seen(10);

    // WHEN the runner that does not own the check passes
    // THEN it leaves the bisect to its live owner, and says so
    let report = two.pass(&two.other, 0, 10).expect("a bisect row");
    assert_eq!(report[0]["outcome"], "not_run", "{report}");
    assert_eq!(
        report[0]["why"],
        format!("runner {} bisects it", two.owner.1),
        "{report}"
    );
    assert_eq!(report[0]["runs"], 0, "{report}");

    // AND the owner bisects it
    let report = two.pass(&two.owner.0, 0, 10).expect("the owner bisects");
    assert_eq!(report[0]["outcome"], "culprit", "{report}");
    assert_eq!(report[0]["runs"], 3, "{report}");
}

#[test]
fn a_red_is_bisected_by_another_runner_once_its_owner_reads_stale() {
    let two = two();
    two.red_after_the_owner_was_seen(10);

    // WHEN 70 s on the owner's presence (10 s cadence, live for a minute) reads stale, inside the
    // check's two timeouts
    // THEN the other runner bisects the red
    let report = two.pass(&two.other, 70, 10).expect("the other bisects");
    assert_eq!(report[0]["outcome"], "culprit", "{report}");
}

#[test]
fn a_red_its_live_owner_left_unanswered_for_two_timeouts_is_bisected_by_another_runner() {
    let two = two();
    // The owner's 100 s cadence keeps its presence live for 330 s.
    two.red_after_the_owner_was_seen(100);

    // WHEN 130 s on, past the check's two 60 s timeouts with no answer in the range
    // THEN the other runner bisects the red beside its live owner
    let report = two.pass(&two.other, 130, 10).expect("the other bisects");
    assert_eq!(report[0]["outcome"], "culprit", "{report}");
}
