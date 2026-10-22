//! G305 (owner ruling 2026-10-21, red only on evidence): bay says whether it guards a
//! repository, and doctor is red when the trunk is changed around it.
//!
//! With no agent adapter declared (`[agent] product`), bay's hook never judges an agent's Git
//! command, so an agent can push the trunk around bay. `bay doctor`'s `hooks` row is then
//! `off`, never green, with the step that turns it on. It is advisory while nothing went around
//! bay: the decision stays `ok`, and `say` names that bay does not guard the project yet. Once
//! the trunk bay lands on also holds landings that went around it, the row is red, the decision
//! `blocked` with `HOOKS_MISSING`, and `say` counts them. `bay status` names the same fact as
//! `detail.guard.hook`, beside `detail.unattributed`, which now also counts the window's
//! landings that came through bay (`through_bay`).
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/doctor.rs` `hooks_row`: `unguarded(git, config, report);` to a push of the old green
//!   row (the first case reads `green`).
//! - `src/doctor.rs` `unguarded`: `&& through > 0` dropped (the first case, a trunk bay never
//!   landed on, turns red).
//! - `src/doctor.rs` `unguarded`: `&& count > 0` to `&& false` (the second case stays `ok`).
//! - `src/result.rs` `Reason::say`: drop the G305 branch (the second case's `say` names no
//!   count).
//! - `src/status.rs`: drop `"guard"` (the first case's status has no guard).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json, row};

/// `bay doctor` in `dir`, and its `hooks` row.
fn doctor(test_box: &TestBox, dir: &Path) -> (i32, Value, Value) {
    let (code, result) = result_json(test_box.bay(dir).arg("doctor"));
    let hooks = row(&result, "hooks").clone();
    (code, result, hooks)
}

#[test]
fn a_repository_bay_does_not_guard_says_so_and_stays_ok_while_nothing_went_around_it() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    test_box.seed_trunk(&repo, &[("src/b.rs", "fn b() {}\n")]);

    let (code, result, hooks) = doctor(&test_box, &repo);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["decision"], "ok", "{result}");
    assert_eq!(hooks["state"], "off", "{hooks}");
    assert!(
        hooks["recovery"]
            .as_str()
            .is_some_and(|step| step.contains("[agent] product")),
        "{hooks}"
    );
    assert_eq!(result["detail"]["guard"]["hook"], "off", "{result}");
    let say = result["say"].as_str().unwrap_or_default();
    assert!(say.contains("does not guard this project yet"), "{result}");
    assert!(!say.contains("nothing else is needed"), "{result}");

    let status = test_box.bay_ok(&repo, &["status"]);
    assert_eq!(status["detail"]["guard"]["hook"], "off", "{status}");
}

#[test]
fn a_trunk_changed_around_bay_turns_doctor_red_with_the_count() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    // One change lands through bay.
    let started = test_box.bay_ok(&repo, &["start", "CARD-305", "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/through.rs"), "fn through() {}\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    // Before anything goes around bay, doctor stays ok.
    test_box.git_ok(&repo, &["fetch", "-q", "origin"]);
    let (code, result, hooks) = doctor(&test_box, &repo);
    assert_eq!(code, 0, "{result}");
    assert_eq!(hooks["state"], "off", "{hooks}");

    // Then a commit is pushed straight to the trunk, around bay.
    test_box.git_ok(&repo, &["merge", "-q", "--ff-only", "origin/main"]);
    test_box.seed_trunk(&repo, &[("src/around.rs", "fn around() {}\n")]);
    test_box.git_ok(&repo, &["fetch", "-q", "origin"]);
    let (code, result, hooks) = doctor(&test_box, &repo);
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["decision"], "blocked", "{result}");
    assert_eq!(result["reason"], "HOOKS_MISSING", "{result}");
    assert_eq!(hooks["state"], "red", "{hooks}");
    assert_eq!(hooks["around_bay"]["count"], 1, "{hooks}");
    assert_eq!(hooks["around_bay"]["of"], 2, "{hooks}");
    let say = result["say"].as_str().unwrap_or_default();
    assert!(say.contains("1 of the last 2 changes"), "{result}");

    let status = test_box.bay_ok(&repo, &["status"]);
    assert_eq!(status["detail"]["unattributed"]["count"], 1, "{status}");
    assert_eq!(
        status["detail"]["unattributed"]["through_bay"], 1,
        "{status}"
    );
}
