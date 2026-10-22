//! G300. `bay start` names a change to pick it up.
//!
//! One verb picks up work: `bay start <CHANGE>`, where the positional is the id of a change
//! whose ref is here or on the remote, does what `bay start <CHANGE>` does. It returns
//! the same change, decision and view, and checkpoints what the view holds. A positional that
//! names no change is an intent, as before, even when it has the shape of a change id.
//! `--resume <CHANGE>` still works, and returns the same shape.
//!
//! Guards to mutate (each makes a case here fail):
//!
//! - `src/start.rs` `start`: drop `.or_else(|| names_change(&repo, request))`, so the
//!   positional is always an intent: `start_with_a_change_id_resumes_that_change` fails (a new
//!   change opens instead of the named one being picked up).
//! - `src/start.rs` `names_change`: replace
//!   `(here || repo.fetch_refs(&[change_ref(id)]).is_ok())` with `true`:
//!   `an_intent_shaped_like_a_change_id_still_opens_a_change` fails (the start tries to resume
//!   a change that does not exist).
//! - `src/start.rs` `names_change`: replace `filter(|id| is_change_id(id))` with
//!   `filter(|_| false)`: `start_with_a_change_id_resumes_that_change` fails.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::collections::BTreeSet;
use std::path::PathBuf;

use support::{TestBox, check_files, passing_result, result_json};

fn setup() -> (TestBox, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    (test_box, repo)
}

/// The keys of a result's detail, which make its shape.
fn detail_keys(result: &serde_json::Value) -> BTreeSet<String> {
    result["detail"]
        .as_object()
        .unwrap_or_else(|| panic!("no detail: {result}"))
        .keys()
        .cloned()
        .collect()
}

#[test]
fn start_with_a_change_id_resumes_that_change() {
    let (test_box, repo) = setup();
    let started = test_box.bay_ok(&repo, &["start", "CARD-300", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = started["detail"]["view"].as_str().unwrap().to_owned();
    // The view is left with work in it, as an agent leaves it.
    std::fs::write(PathBuf::from(&view).join("src/half.rs"), "fn half() {}\n").unwrap();

    let (code, named) = result_json(test_box.bay(&repo).args(["start", &id]));
    assert_eq!(code, 0, "{named}");
    assert_eq!(named["change"], id.as_str(), "{named}");
    assert_eq!(named["decision"], "ok", "{named}");
    assert_eq!(named["detail"]["view"], view.as_str(), "{named}");
    assert_eq!(named["detail"]["checkpointed"], true, "{named}");
    assert!(
        named["detail"].get("adopted").is_none(),
        "a resume, not an open: {named}"
    );

    // `--resume` still works, and returns the same shape.
    let (code, resumed) = result_json(test_box.bay(&repo).args(["start", &id]));
    assert_eq!(code, 0, "{resumed}");
    assert_eq!(resumed["change"], named["change"], "{resumed}");
    assert_eq!(resumed["decision"], named["decision"], "{resumed}");
    assert_eq!(
        resumed["detail"]["view"], named["detail"]["view"],
        "{resumed}"
    );
    assert_eq!(
        detail_keys(&resumed),
        detail_keys(&named),
        "{resumed} / {named}"
    );
}

#[test]
fn an_intent_that_names_no_change_still_opens_one() {
    let (test_box, repo) = setup();
    let first = test_box.bay_ok(&repo, &["start", "CARD-301", "--paths", "src/"]);
    let other = test_box.bay_ok(&repo, &["start", "CARD-302", "--paths", "src/"]);
    assert_ne!(first["change"], other["change"], "{first} / {other}");
    assert!(
        bay::ids::is_change_id(other["change"].as_str().unwrap()),
        "{other}"
    );
}

#[test]
fn an_intent_shaped_like_a_change_id_still_opens_a_change() {
    let (test_box, repo) = setup();
    let intent = "card2345";
    assert!(
        bay::ids::is_change_id(intent),
        "the case needs an id-shaped intent"
    );
    let (code, opened) = result_json(
        test_box
            .bay(&repo)
            .args(["start", intent, "--paths", "src/"]),
    );
    assert_eq!(code, 0, "{opened}");
    assert_eq!(opened["decision"], "ok", "{opened}");
    let id = opened["change"].as_str().unwrap();
    assert_ne!(id, intent, "{opened}");
    assert!(
        PathBuf::from(opened["detail"]["view"].as_str().unwrap()).is_dir(),
        "{opened}"
    );
}
