//! Item 98 (a fleet finding, 2026-11-25: checks moved from before the merge to after it, and a
//! red main then stood until the session whose change broke it came back): with `[red]
//! undo_after_s`, a runner undoes a red main's culprit on its own when no fix comes in time.
//!
//! The undo is an ordinary `bay undo` the runner opens under its own name; the line merges it,
//! and main turns green. Only a culprit the evidence pins is undone, once, after its session was
//! told it broke the check and the wait has passed; never while a fix of it is in line, never an
//! undo, and never when its undo conflicts, which is told once instead. Off by default.
//!
//! Guards to mutate, in `src/red_undo.rs` `undone`: the `held` test (a culprit is undone again),
//! the `record.reverts` test (an undo is undone), the `answered_by` test (a culprit is undone
//! while its fix is in line) and the wait (a culprit is undone at once). The `read_main` test,
//! main moved under the decision, is G943's. In
//! `name_in`: a row that names no undo (BACKLOG 322: `bay status` names the undo, or when it is due).

#![cfg(unix)]

use crate::g244d_a_red_post_check_names_its_culprit_by_bisection::{Trunk, trunk_with};
use crate::support::result_json;

use std::path::PathBuf;

use serde_json::Value;

/// The wake adapter: appends each request to `$HOME/woken`. bay gives an adapter only PATH and
/// HOME.
const WAKE: &str = "#!/bin/sh\ncat >> \"$HOME/woken\"\necho >> \"$HOME/woken\"\nprintf '{\"schema\":\"bay.wake-result/1\",\"result\":\"woken\",\"reason\":null}'\n";

/// A trunk that bisects its red post check and wakes its sessions; `undo_after_s`, when given,
/// turns the automatic undo on.
fn trunk(undo_after_s: Option<u64>) -> Trunk {
    let red = undo_after_s
        .map(|after| format!("\n[red]\nundo_after_s = {after}\n"))
        .unwrap_or_default();
    let config = format!(
        "[runner]\nbisect_post = true\n\n[plugin.wake]\ncommand = \"scripts/bay-wake\"\n{red}"
    );
    trunk_with(&config, &[("scripts/bay-wake", WAKE)])
}

/// Every red wake the trunk's sessions were sent, oldest first.
fn red_wakes(trunk: &Trunk) -> Vec<Value> {
    std::fs::read_to_string(trunk.test_box.home.join("woken"))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|request| request["cause"] == "red")
        .collect()
}

/// The red wakes that tell what a runner did about a culprit no fix came for.
fn undo_wakes(trunk: &Trunk) -> Vec<Value> {
    red_wakes(trunk)
        .into_iter()
        .filter(|wake| {
            wake["line"]
                .as_str()
                .is_some_and(|line| line.contains("no fix came in"))
        })
        .collect()
}

/// Lands a change that writes `text` to `path`, or removes it when `text` is `None`: its change
/// id and its commit on trunk.
fn land(trunk: &Trunk, intent: &str, path: &str, text: Option<&str>) -> (String, String) {
    let started = trunk
        .test_box
        .bay_ok(&trunk.repo, &["start", intent, "--paths", "src"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    match text {
        Some(text) => std::fs::write(view.join(path), text).unwrap(),
        None => std::fs::remove_file(view.join(path)).unwrap(),
    }
    let (code, landed) = result_json(trunk.test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    (id, landed["detail"]["commit"].as_str().unwrap().to_owned())
}

/// Runner passes, at most four, until `change` is main's newest landing.
fn passes_until_landed(trunk: &Trunk, change: &str) {
    for _ in 0..4 {
        if trunk.test_box.trunk_change_ids(&trunk.remote)[0] == change {
            return;
        }
        trunk.executor_pass();
    }
    assert_eq!(
        trunk.test_box.trunk_change_ids(&trunk.remote)[0],
        change,
        "the line merges the undo"
    );
}

fn main_tip(trunk: &Trunk) -> String {
    trunk
        .test_box
        .git_bare(&trunk.remote, &["rev-parse", "main"])
        .trim()
        .to_owned()
}

#[test]
fn a_culprit_no_fix_came_for_is_undone_and_main_turns_green() {
    // GIVEN a red the evidence pins on the fourth landing, and no wait.
    let trunk = trunk(Some(0));
    let landings = trunk.red_over_four();
    let culprit = landings[3].0.as_str();
    let report = trunk.executor_pass().expect("the executor bisects");
    assert_eq!(report[0]["outcome"], "culprit", "{report}");

    // WHEN the next pass tells the culprit it broke the check.
    trunk.executor_pass();

    // THEN the runner opened its undo, and told the culprit's session so, once.
    let wakes = undo_wakes(&trunk);
    assert_eq!(wakes.len(), 1, "{wakes:?}");
    assert_eq!(wakes[0]["change"], culprit, "{wakes:?}");
    let undo = wakes[0]["undo"]
        .as_str()
        .expect("the wake names the undo")
        .to_owned();
    assert_ne!(undo, culprit);
    let line = wakes[0]["line"].as_str().unwrap();
    assert!(
        line.contains(&format!("so bay opened its undo, change {undo}")),
        "{line}"
    );
    // AND the red's status row names the undo.
    let red = trunk.red();
    assert_eq!(red[0]["undo"]["change"], undo.as_str(), "{red}");
    assert!(
        red[0]["say"]
            .as_str()
            .unwrap()
            .contains(&format!("bay opened its undo, change {undo}")),
        "{red}"
    );

    // AND the line merges the undo: an ordinary change that reverts the culprit.
    passes_until_landed(&trunk, &undo);
    let tip = main_tip(&trunk);
    let message = trunk
        .test_box
        .git_bare(&trunk.remote, &["log", "-1", "--format=%B", &tip]);
    assert!(
        message.contains(&format!("Reverts: {culprit}")),
        "{message}"
    );
    let tree = trunk
        .test_box
        .git_bare(&trunk.remote, &["ls-tree", "-r", "--name-only", &tip]);
    assert!(!tree.lines().any(|path| path == "src/l3.rs"), "{tree}");

    // AND main is green once its post check passes there; no second undo is opened.
    trunk.suite(&tip, "pass");
    let red = trunk.red();
    assert_eq!(red.as_array().map(Vec::len), Some(0), "{red}");
    trunk.executor_pass();
    assert_eq!(undo_wakes(&trunk).len(), 1, "one decision per culprit");
}

#[test]
fn without_the_key_no_culprit_is_undone() {
    let trunk = trunk(None);
    let landings = trunk.red_over_four();
    for _ in 0..3 {
        trunk.executor_pass();
    }
    assert_eq!(undo_wakes(&trunk), Vec::<Value>::new());
    let red = trunk.red();
    assert_eq!(red[0]["culprit"]["change"], landings[3].0.as_str(), "{red}");
    assert!(red[0].get("undo").is_none(), "{red}");
    assert_eq!(
        trunk.test_box.trunk_change_ids(&trunk.remote)[0],
        landings[4].0
    );
}

#[test]
fn a_culprit_is_not_undone_before_the_wait_has_passed() {
    let trunk = trunk(Some(3600));
    let landings = trunk.red_over_four();
    for _ in 0..3 {
        trunk.executor_pass();
    }
    // The culprit was told it broke the check, and nothing was undone.
    let told = red_wakes(&trunk);
    assert!(
        told.iter()
            .any(|wake| wake["change"] == landings[3].0.as_str()
                && wake["line"].as_str().unwrap().contains("broke it")),
        "{told:?}"
    );
    assert_eq!(undo_wakes(&trunk), Vec::<Value>::new());
    assert_eq!(
        trunk.test_box.trunk_change_ids(&trunk.remote)[0],
        landings[4].0
    );
    // AND the red's status row says when a runner undoes it.
    let red = trunk.red();
    assert_eq!(red[0]["undo"]["culprit"], landings[3].0.as_str(), "{red}");
    assert!(red[0]["undo"]["due_at"].is_string(), "{red}");
    assert!(
        red[0]["say"]
            .as_str()
            .unwrap()
            .contains("Unless a fix comes first, a runner undoes it at"),
        "{red}"
    );
}

#[test]
fn a_culprit_whose_fix_is_in_line_is_not_undone() {
    let trunk = trunk(Some(0));
    let landings = trunk.red_over_four();
    let culprit = landings[3].0.as_str();
    trunk.executor_pass().expect("the executor bisects");

    // GIVEN a fix of the culprit, in line and not merged.
    let started = trunk.test_box.bay_ok(
        &trunk.repo,
        &["start", "the fix", "--paths", "src", "--fixes", culprit],
    );
    let fix = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::remove_file(view.join("src/l3.rs")).unwrap();
    let (code, queued) = result_json(trunk.test_box.bay(&view).args(["merge"]));
    assert_eq!(code, 0, "{queued}");

    // WHEN the runner passes, the fix merges, and nothing was undone.
    passes_until_landed(&trunk, &fix);
    trunk.executor_pass();
    assert_eq!(undo_wakes(&trunk), Vec::<Value>::new());
}

#[test]
fn a_culprit_whose_undo_conflicts_is_told_once_and_nothing_is_opened() {
    // GIVEN the culprit added src/l3.rs, and a later merge changed it.
    let trunk = trunk(Some(0));
    let landings: Vec<(String, String)> = (0..4)
        .map(|n| {
            land(
                &trunk,
                &format!("l{n}"),
                &format!("src/l{n}.rs"),
                Some("x\n"),
            )
        })
        .collect();
    let edited = land(&trunk, "edit l3", "src/l3.rs", Some("changed\n"));
    trunk.suite(&landings[0].1, "pass");
    trunk.suite(&edited.1, "product_failure");
    trunk.executor_pass().expect("the executor bisects");
    let red = trunk.red();
    assert_eq!(red[0]["culprit"]["change"], landings[3].0.as_str(), "{red}");

    // WHEN the next passes see the culprit.
    trunk.executor_pass();
    trunk.executor_pass();

    // THEN its session is told once that its undo conflicts, and no undo was opened.
    let wakes = undo_wakes(&trunk);
    assert_eq!(wakes.len(), 1, "{wakes:?}");
    assert_eq!(wakes[0]["change"], landings[3].0.as_str(), "{wakes:?}");
    assert_eq!(wakes[0].get("undo"), None, "{wakes:?}");
    let line = wakes[0]["line"].as_str().unwrap();
    assert!(
        line.contains("its undo conflicts with later merges"),
        "{line}"
    );
    assert!(
        line.ends_with(&format!("--fixes {}", landings[3].0)),
        "{line}"
    );
    assert_eq!(trunk.test_box.trunk_change_ids(&trunk.remote)[0], edited.0);
}

#[test]
fn an_undo_that_broke_main_is_never_undone_automatically() {
    // GIVEN the culprit is an undo: it brought src/l3.rs back.
    let trunk = trunk(Some(0));
    for n in 0..4 {
        land(
            &trunk,
            &format!("l{n}"),
            &format!("src/l{n}.rs"),
            Some("x\n"),
        );
    }
    let removed = land(&trunk, "drop l3", "src/l3.rs", None);
    let (code, undone) = result_json(
        trunk
            .test_box
            .bay(&trunk.repo)
            .args(["undo", &removed.0, "--wait"]),
    );
    assert_eq!(code, 0, "{undone}");
    let undo = undone["change"].as_str().unwrap().to_owned();
    let commit = main_tip(&trunk);
    trunk.suite(&removed.1, "pass");
    trunk.suite(&commit, "product_failure");

    // WHEN the runner passes.
    for _ in 0..3 {
        trunk.executor_pass();
    }

    // THEN the undo owns the red, is told it broke the check, and is not undone.
    let red = trunk.red();
    assert_eq!(red[0]["culprit"]["change"], undo.as_str(), "{red}");
    assert!(
        red_wakes(&trunk)
            .iter()
            .any(|wake| wake["change"] == undo.as_str()),
        "the culprit is told"
    );
    assert_eq!(undo_wakes(&trunk), Vec::<Value>::new());
    assert_eq!(trunk.test_box.trunk_change_ids(&trunk.remote)[0], undo);
}
