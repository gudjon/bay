//! G373 (a fleet finding, 2026-10-25: a lint suite moved after merge would run nowhere). The
//! runner runs post checks on main's tip.
//!
//! With `[runner] post = true`, after its queue's work the runner runs each post check
//! (`stage = "post"`) once on main's newest tip that holds no record of it, on a box that serves
//! the check's environment. So many landings cost one run of a whole-tree check, a red tip is
//! named, and `[runner] bisect_post` narrows it to its culprit, with no outside post producer.
//!
//! Guard to mutate: `src/executor.rs` `pass`: drop the `crate::bisect::tips` call (no post check
//! runs, and main is never found red).

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
