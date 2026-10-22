//! G301. `bay abandon` gives a change up.
//!
//! `bay abandon` submits the abandon request `bay abandon` submits, by one code path:
//! with no argument, the change of the view it runs in; with an argument, that change, from
//! anywhere in the repository. The change's state becomes `abandoned`, in the result and in
//! `bay status`. `bay abandon` still works.
//!
//! Guards to mutate (each makes a case here fail):
//!
//! - `src/main.rs` `fn abandon`: pass `false` as the last argument of
//!   `bay::publish::land(dir, change, false, None, true)`:
//!   `bay_abandon_in_a_view_abandons_its_change` and
//!   `bay_abandon_names_a_change_from_the_repository` fail (a landing is submitted instead).
//! - `src/main.rs` `fn abandon`: pass `None` for `change`:
//!   `bay_abandon_names_a_change_from_the_repository` fails (no view, so no change).
//! - `src/publish.rs` `land`: drop the `if abandon { ... }` branch:
//!   `bay_land_abandon_still_works` fails, with every case above.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

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

/// A started change, with work in its view: its id and its view.
fn started(test_box: &TestBox, repo: &Path, intent: &str) -> (String, PathBuf) {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/half.rs"), "fn half() {}\n").unwrap();
    (id, view)
}

/// Asserts `abandoned` gave change `id` up, and `bay status` says so.
fn assert_abandoned(test_box: &TestBox, repo: &Path, id: &str, abandoned: &serde_json::Value) {
    assert_eq!(abandoned["change"], id, "{abandoned}");
    assert_eq!(abandoned["detail"]["state"], "abandoned", "{abandoned}");
    let status = test_box.bay_ok(repo, &["status", id]);
    let change = &status["detail"]["changes"][0];
    assert_eq!(change["state"], "abandoned", "{status}");
}

#[test]
fn bay_abandon_in_a_view_abandons_its_change() {
    let (test_box, repo) = setup();
    let (id, view) = started(&test_box, &repo, "CARD-310");
    let (code, abandoned) = result_json(test_box.bay(&view).arg("abandon"));
    assert_eq!(code, 0, "{abandoned}");
    assert_abandoned(&test_box, &repo, &id, &abandoned);
    assert!(view.join("src/half.rs").is_file(), "the view remains");
}

#[test]
fn bay_abandon_names_a_change_from_the_repository() {
    let (test_box, repo) = setup();
    let (id, _view) = started(&test_box, &repo, "CARD-311");
    let (code, abandoned) = result_json(test_box.bay(&repo).args(["abandon", &id]));
    assert_eq!(code, 0, "{abandoned}");
    assert_abandoned(&test_box, &repo, &id, &abandoned);
}

#[test]
fn bay_land_abandon_still_works() {
    let (test_box, repo) = setup();
    let (id, view) = started(&test_box, &repo, "CARD-312");
    let (code, abandoned) = result_json(test_box.bay(&view).args(["abandon"]));
    assert_eq!(code, 0, "{abandoned}");
    assert_abandoned(&test_box, &repo, &id, &abandoned);
}
