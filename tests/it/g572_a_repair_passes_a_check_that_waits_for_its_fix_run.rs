//! G572 (a fleet finding, 2026-10-29: a fix for records-refs waited `MAIN_RED` behind
//! scripts-fast, whose own fix was on main and waited only for the run on it, and that run
//! waited for a worker). A repair, a change that answers one red post check, passes a check
//! that is `pending_fix` and does not hold it: that check's fix is on main already, and only its
//! run decides. It still waits for a check that is red, and a change that repairs nothing still
//! waits for a `pending_fix` check (E35).
//!
//! `suite` (on `src/**`) and `docs` (on `docs/**`) are both red. A repair of `docs` on both
//! trees waits for `suite`. A fix of `suite` lands, so `suite` is `pending_fix`: the repair of
//! `docs` lands before any run of `suite`, and a plain change on `src/` still waits for one.
//!
//! Guards to mutate, in `src/red.rs` `blocking`: drop the `repair &&` (the plain change lands
//! before the run on the fix), or drop the `red.fix.is_some()` (the repair lands while `suite`
//! is red); in `src/publish.rs` `red_gate`, filter by `answered_by` alone (the repair waits).

#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use serde_json::Value;
use support::queued::{Check, Held, Trust, host_under};
use support::{PRODUCER, check_files, passing_result, result_json};

/// The post checks `suite` on `src/**` and `docs` on `docs/**`.
const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n\n[[check]]\nid = \"docs\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"docs/**\"]\n";

/// A host whose post checks are answered by records the test writes, as the broad suite's own
/// box writes them (G244), and an executor clone that runs no post check.
struct Line {
    held: Held,
    exec: PathBuf,
    broad: PathBuf,
}

impl Line {
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
        let broad = held.test_box.clone_of(&held.remote, "suite-box");
        let key = support::talk::fresh_key(&held.test_box, "suite-1");
        held.test_box
            .git_ok(&broad, &["config", "bay.producerName", "suite-1"]);
        held.test_box.git_ok(
            &broad,
            &["config", "bay.producerKey", key.to_str().unwrap()],
        );
        Self { held, exec, broad }
    }

    /// Post check `check` answers `verdict` on main's tip. A `check` record's id names the
    /// commit and the version, not the check, so each check records under its own version.
    fn answer(&self, check: &str, verdict: &str) {
        let test_box = &self.held.test_box;
        test_box.git_ok(&self.broad, &["fetch", "-q", "origin"]);
        let tip = test_box.git_bare(&self.held.remote, &["rev-parse", "main"]);
        let (code, written) = result_json(test_box.bay(&self.broad).args([
            "evidence",
            "record",
            "--kind",
            "check",
            "--check",
            check,
            "--environment",
            "local",
            "--verdict",
            verdict,
            "--version",
            check,
            "--commit",
            tip.trim(),
        ]));
        assert_eq!(code, 0, "{written}");
    }

    /// Submits change `intent` on `dirs`, writing `<dir>/<intent>.x` in each, with `--fixes
    /// fixes` when it names one.
    fn submit(&self, intent: &str, dirs: &[&str], fixes: Option<&str>) -> String {
        let mut args = vec!["start", intent, "--paths"];
        let paths: Vec<String> = dirs.iter().map(|dir| format!("{dir}/")).collect();
        args.extend(paths.iter().map(String::as_str));
        if let Some(fixes) = fixes {
            args.extend(["--fixes", fixes]);
        }
        let started = self.held.test_box.bay_ok(&self.held.repo, &args);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        for dir in dirs {
            std::fs::write(view.join(format!("{dir}/{intent}.x")), "x\n").unwrap();
        }
        let (code, queued) = result_json(self.held.test_box.bay(&view).arg("merge"));
        assert_eq!(code, 0, "{queued}");
        id
    }

    fn on_main(&self, id: &str) -> bool {
        self.held
            .test_box
            .trunk_change_ids(&self.held.remote)
            .contains(&id.to_owned())
    }

    /// Runs up to three passes, until `id` is on main: whether it landed, and the passes.
    fn lands(&self, id: &str) -> (bool, Vec<Value>) {
        let mut ran = Vec::new();
        for _ in 0..3 {
            ran.push(self.held.pass(&self.exec));
            if self.on_main(id) {
                return (true, ran);
            }
        }
        (false, ran)
    }

    fn status(&self) -> Value {
        self.held.test_box.bay_ok(&self.held.repo, &["status"])
    }

    /// The text of `bay status <id>`.
    fn status_of(&self, id: &str) -> String {
        self.held
            .test_box
            .bay_ok(&self.held.repo, &["status", id])
            .to_string()
    }
}

/// The `check` row of `status`'s `detail.red_pending`.
fn pending(status: &Value, check: &str) -> Option<Value> {
    status["detail"]["red_pending"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row["check"] == check)
        .cloned()
}

/// The checks of `status`'s `detail.red`, sorted.
fn red(status: &Value) -> Vec<String> {
    status["detail"]["red"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| row["check"].as_str().unwrap().to_owned())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[test]
fn a_repair_passes_a_check_that_waits_for_its_fix_run_and_waits_for_a_red_one() {
    let line = Line::new();
    // GIVEN `broke` on src/, which fails `suite`, and `odd` on docs/, which fails `docs`.
    let base = line.submit("base", &["docs"], None);
    assert!(line.lands(&base).0);
    line.answer("suite", "pass");
    line.answer("docs", "pass");
    let broke = line.submit("broke", &["src"], None);
    assert!(line.lands(&broke).0);
    line.answer("suite", "product_failure");
    let odd = line.submit("odd", &["docs"], None);
    assert!(line.lands(&odd).0, "docs/ is off the red's paths");
    line.answer("docs", "product_failure");
    let status = line.status();
    assert_eq!(red(&status), ["docs", "suite"], "{status}");

    // WHEN a repair of `docs` on both trees is submitted while `suite` is red THEN it waits.
    let repair = line.submit("repair", &["src", "docs"], Some("docs"));
    let ran = line.held.pass(&line.exec);
    assert!(
        !line.on_main(&repair),
        "a repair landed on a red check it does not answer: {ran}"
    );
    let waiting = line.status_of(&repair);
    assert!(waiting.contains("MAIN_RED"), "{waiting}");

    // WHEN a fix of `suite` lands THEN `suite` waits only for the run on it.
    let fix = line.submit("fix", &["src"], Some(&broke));
    let (landed, ran) = line.lands(&fix);
    assert!(landed, "{ran:?}");
    let status = line.status();
    assert_eq!(
        pending(&status, "suite").map(|row| row["state"].clone()),
        Some(Value::from("pending_fix")),
        "{status}"
    );

    // AND a plain change on src/ is submitted.
    let plain = line.submit("plain", &["src"], None);

    // THEN the repair of `docs` lands before any run of `suite`, and the plain change waits.
    let (landed, ran) = line.lands(&repair);
    assert!(
        landed,
        "the repair waited for a check whose fix is on main: {ran:?}"
    );
    assert!(
        !line.on_main(&plain),
        "a change that repairs nothing landed before the run on the fix: {ran:?}"
    );
    let waiting = line.status_of(&plain);
    assert!(waiting.contains("MAIN_RED"), "{waiting}");

    // WHEN `suite` passes on the fix THEN the plain change lands.
    line.answer("suite", "pass");
    let (landed, ran) = line.lands(&plain);
    assert!(landed, "{ran:?}");
}
