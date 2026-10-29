//! G373 (a fleet finding, 2026-10-25: a lint suite moved after merge would run nowhere). The
//! runner runs post checks on main's tip.
//!
//! With `[runner] post = true`, after its queue's work the runner runs each post check
//! (`stage = "post"`) once on main's newest tip that holds no record of it, on a box that serves
//! the check's environment. So many landings cost one run of a whole-tree check, a red tip is
//! named, and `[runner] bisect_post` narrows it to its culprit, with no outside post producer.
//!
//! Guard to mutate: `src/bisect.rs` `plan`: drop the `tips` stage (no post check runs, and main
//! is never found red). G433: the check the box ran longest ago goes first; order by pending
//! alone in `bisect::in_turn` and a check that never answers runs on every tip.

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check suite\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

/// A trunk whose pre check passes and whose post check fails on every tree holding `src/l3.rs`.
fn trunk() -> (TestBox, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let mut files = check_files(
        &format!("[ \"$1\" = suite ] && [ -e src/l3.rs ] && {{ {failing}; exit 0; }}"),
        &passing_result(1),
        1,
        60,
    );
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(POST);
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\npost = true\nbisect_post = true\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    (test_box, repo)
}

/// Lands a change that writes `path`: its change id.
fn land(test_box: &TestBox, repo: &Path, path: &str) -> String {
    let started = test_box.bay_ok(repo, &["start", path, "--paths", "src"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(path), format!("{path}\n")).unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    id
}

/// One runner pass: what it ran.
fn pass(test_box: &TestBox, repo: &Path) -> Vec<Value> {
    let ran = test_box.bay_ok(repo, &["runner", "run", "--once"]);
    ran["detail"]["ran"].as_array().cloned().unwrap_or_default()
}

#[test]
fn a_red_tip_is_found_and_its_culprit_named_with_no_outside_producer() {
    let (test_box, repo) = trunk();
    land(&test_box, &repo, "src/l0.rs");
    let first = pass(&test_box, &repo);
    let post = first
        .iter()
        .find_map(|row| row.get("post"))
        .unwrap_or_else(|| panic!("no post check ran: {first:?}"));
    assert_eq!(post[0]["verdict"], "pass", "{post}");
    // The same tip is not run twice.
    let again = pass(&test_box, &repo);
    assert!(
        again.iter().all(|row| row.get("post").is_none()),
        "the tip was run again: {again:?}"
    );

    let breaking = ["src/l1.rs", "src/l2.rs", "src/l3.rs", "src/l4.rs"]
        .iter()
        .map(|path| land(&test_box, &repo, path))
        .nth(2)
        .unwrap();
    // One run on the newest tip finds main red; the bisect names the landing that broke it.
    let second = pass(&test_box, &repo);
    let post = second.iter().find_map(|row| row.get("post")).cloned();
    assert_eq!(
        post.as_ref().map(|post| post[0]["verdict"].clone()),
        Some(Value::from("product_failure")),
        "{second:?}"
    );
    let mut red = Value::Null;
    for _ in 0..4 {
        red = test_box.bay_ok(&repo, &["status"])["detail"]["red"].clone();
        if red[0]["culprit"].is_object() {
            break;
        }
        pass(&test_box, &repo);
    }
    assert_eq!(red[0]["check"], "suite", "{red}");
    assert_eq!(red[0]["culprit"]["change"], breaking.as_str(), "{red}");
}

/// A fleet finding (2026-10-27): every post check on a tip took one evidence id, so the second
/// one's record was refused and no post check after the first ever ran (four of five silent for
/// 22 hours). Two post checks on one tip both run, each with its own record, one a pass.
#[test]
fn every_post_check_on_a_tip_runs_not_only_the_first() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(POST);
            text.push_str(
                &POST
                    .replace("\"suite\"", "\"lint\"")
                    .replace("bay-check suite", "bay-check lint"),
            );
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\npost = true\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    land(&test_box, &repo, "src/l0.rs");
    // A pass runs one post check, so the line is never held behind them; the next pass runs
    // the next (a fleet finding, 2026-10-27: five in one pass held the head 40 minutes).
    let mut checks = Vec::new();
    for _ in 0..2 {
        let ran = pass(&test_box, &repo);
        let post = ran
            .iter()
            .find_map(|row| row.get("post"))
            .unwrap_or_else(|| panic!("no post check ran: {ran:?}"));
        let rows = post.as_array().unwrap();
        assert_eq!(rows.len(), 1, "one post check a pass: {post}");
        assert_eq!(rows[0]["verdict"], "pass", "{post}");
        checks.push(rows[0]["check"].as_str().unwrap().to_owned());
    }
    assert_eq!(checks, ["suite", "lint"]);
}

/// A fleet finding (2026-10-27, 0.5.28): post work ran only within the pass bound, and with a
/// deep queue every pass reached it, so no post check ran at all. A pass past its bound still
/// runs its one post check.
#[test]
fn a_pass_past_its_bound_still_runs_one_post_check() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(POST);
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\npost = true\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    land(&test_box, &repo, "src/l0.rs");
    let (code, result) = result_json(
        test_box
            .bay(&repo)
            .env("BAY_TEST_PASS_BOUND_S", "0")
            .args(["runner", "run", "--once"]),
    );
    assert_eq!(code, 0, "{result}");
    let post = result["detail"]["ran"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|row| row.get("post"))
        .unwrap_or_else(|| panic!("no post check past the bound: {result}"));
    assert_eq!(post[0]["verdict"], "pass", "{post}");
}

/// A fleet finding (2026-10-27, 0.5.29): with one post check a pass and main moving every few
/// minutes, each new tip began again at the policy's first post check, so the later ones never
/// ran. A post check no landing answers goes before one answered a landing ago.
#[test]
fn a_post_check_nothing_answered_goes_before_one_answered_on_an_older_tip() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(POST);
            text.push_str(
                &POST
                    .replace("\"suite\"", "\"lint\"")
                    .replace("bay-check suite", "bay-check lint"),
            );
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\npost = true\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    land(&test_box, &repo, "src/l0.rs");
    let first = pass(&test_box, &repo);
    let ran_first = first
        .iter()
        .find_map(|row| row.get("post"))
        .and_then(|post| post[0]["check"].as_str().map(str::to_owned));
    assert_eq!(ran_first.as_deref(), Some("suite"), "{first:?}");
    // Main moves on: on the new tip, suite answered a landing ago and lint never did.
    land(&test_box, &repo, "src/l1.rs");
    let second = pass(&test_box, &repo);
    let ran_second = second
        .iter()
        .find_map(|row| row.get("post"))
        .and_then(|post| post[0]["check"].as_str().map(str::to_owned));
    assert_eq!(ran_second.as_deref(), Some("lint"), "{second:?}");
}

/// G433 (a fleet finding, 2026-10-28): a post check that only ever answers `unknown` stays
/// pending, so it went first on every new tip and the checks after it in the policy never ran
/// (edge-py-fast, fifth on its box, ran on no tip for eight hours). The check this box ran
/// longest ago goes first, so each runs in turn.
#[test]
fn a_post_check_that_never_answers_starves_no_other() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    let named = |id: &str| {
        POST.replace("\"suite\"", &format!("\"{id}\""))
            .replace("bay-check suite", &format!("bay-check {id}"))
    };
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            // `vague` expects two units and the adapter sends one: unknown on every tip.
            text.push_str(&named("vague").replace("expected_minimum = 1", "expected_minimum = 2"));
            text.push_str(POST);
            text.push_str(&named("lint"));
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\npost = true\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let mut ran = Vec::new();
    for n in 0..3 {
        land(&test_box, &repo, &format!("src/l{n}.rs"));
        let rows = pass(&test_box, &repo);
        ran.push(
            rows.iter()
                .find_map(|row| row.get("post"))
                .and_then(|post| post[0]["check"].as_str().map(str::to_owned))
                .unwrap_or_default(),
        );
    }
    assert_eq!(
        ran,
        ["vague", "suite", "lint"],
        "a check that never answers went first on every tip"
    );
}
