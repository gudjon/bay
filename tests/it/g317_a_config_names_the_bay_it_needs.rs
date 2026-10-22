//! G317. A config names the bay it needs.
//!
//! `.bay/config.toml` may name the oldest bay it needs, `bay = "<version>"`. A bay older than
//! that refuses every command that reads the config, before anything else, with
//! `TOOL_BELOW_FLOOR`, the next step `bay update` and one plain sentence. The floor is read
//! loosely before the strict parse, so a config that also carries a key this bay does not know
//! still gets the floor refusal, never a config error. A bay that meets the floor works as
//! before, and `bay doctor` names the floor in its config row and whether this machine meets it.
//! A bay older than the release that brought `bay` cannot read it: a team sets `bay` together
//! with the first key the floor protects.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/repo.rs` `Repo::open_with`: drop `floor_met(&text, &common_dir, &tracking)?;`, the
//!   refusal (`Config::parse` still refuses, but as a
//!   failure with no reason: every refusal case fails).
//! - `src/config.rs` `unmet_floor`: make its last line `(running() > needed).then_some(floor)`
//!   (the 99.0.0 cases are no longer refused, and the 0.0.1 case is).
//! - `src/doctor.rs` `loaded_row`: drop `row["meets_floor"] = json!(true);` (the doctor case of
//!   a met floor fails).

use crate::support;

use std::path::PathBuf;

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json, row};

/// A repository whose trunk holds `config` as its `.bay/config.toml`, and one required check.
fn seeded(test_box: &TestBox, name: &str, config: &str) -> PathBuf {
    let repo = test_box.repo_any(name);
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((".bay/config.toml".to_owned(), config.to_owned()));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    repo
}

/// `result` is the floor refusal naming `floor`.
fn assert_below_floor(result: &Value, floor: &str) {
    assert_eq!(result["decision"], "blocked", "{result}");
    assert_eq!(result["reason"], "TOOL_BELOW_FLOOR", "{result}");
    assert_eq!(result["detail"]["tool"], "bay", "{result}");
    assert_eq!(
        result["say"], "This project needs a newer bay; update it, then try again.",
        "{result}"
    );
    assert_eq!(
        result["next_step"],
        "Update the tool `detail.tool` names to `detail.floor` (`bay update` for bay), or remove its `[agent]` product, then run the command again.",
        "{result}"
    );
    assert_eq!(result["detail"]["floor"], floor, "{result}");
}

#[test]
fn a_bay_older_than_the_floor_refuses_and_says_to_update() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box, "repo", "bay = \"99.0.0\"\n");
    let (code, status) = result_json(test_box.bay(&repo).arg("status"));
    assert_eq!(code, 10, "{status}");
    assert_below_floor(&status, "99.0.0");
    // A command that writes refuses the same way, before it starts anything.
    let (code, started) = result_json(
        test_box
            .bay(&repo)
            .args(["start", "CARD-1", "--paths", "src/"]),
    );
    assert_eq!(code, 10, "{started}");
    assert_below_floor(&started, "99.0.0");

    let (_, doctor) = result_json(test_box.bay(&repo).arg("doctor"));
    let config = row(&doctor, "config");
    assert_eq!(config["floor"], "99.0.0", "{config}");
    assert_eq!(config["meets_floor"], false, "{config}");
    assert_eq!(config["state"], "red", "{config}");
    assert_eq!(doctor["reason"], "TOOL_BELOW_FLOOR", "{doctor}");
    assert_eq!(doctor["detail"]["tool"], "bay", "{doctor}");
}

#[test]
fn a_bay_that_meets_the_floor_works_and_doctor_names_it() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box, "repo", "bay = \"0.0.1\"\n");
    let status = test_box.bay_ok(&repo, &["status"]);
    assert_ne!(status["reason"], "TOOL_BELOW_FLOOR", "{status}");

    let (_, doctor) = result_json(test_box.bay(&repo).arg("doctor"));
    let config = row(&doctor, "config");
    assert_eq!(config["floor"], "0.0.1", "{config}");
    assert_eq!(config["meets_floor"], true, "{config}");
    assert_eq!(config["state"], "green", "{config}");
}

#[test]
fn the_floor_is_read_before_a_key_this_bay_does_not_know() {
    let test_box = TestBox::new();
    let repo = seeded(
        &test_box,
        "repo",
        "bay = \"99.0.0\"\nkey_from_a_later_bay = true\n\n[table_from_a_later_bay]\nkey = 1\n",
    );
    let (code, status) = result_json(test_box.bay(&repo).arg("status"));
    assert_eq!(code, 10, "{status}");
    assert_below_floor(&status, "99.0.0");
    let text = status.to_string();
    assert!(!text.contains("unknown field"), "{status}");
}

/// G330 (0.3.1, the site's finding): a floor refusal read from a stale copy of main is checked
/// against main once more. Before, a clone refused for a floor never fetched, so it kept
/// refusing after main lowered the floor, until someone ran `git fetch` by hand.
///
/// Guard to mutate: `src/repo.rs` `open_with`: `if refused && prefetched.is_none()` to
/// `if false` (the clone still refuses after main is fixed).
#[test]
fn a_floor_main_has_since_lowered_is_read_from_main() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box, "repo", "bay = \"99.0.0\"\n");
    let (code, refused) = result_json(test_box.bay(&repo).arg("status"));
    assert_eq!(code, 10, "{refused}");
    assert_below_floor(&refused, "99.0.0");

    // Someone fixes main: the floor goes.
    let remote = test_box.path().join("repo-remote.git");
    let fixer = test_box.clone_of(&remote, "fixer");
    std::fs::write(fixer.join(".bay/config.toml"), "bay = \"0.0.1\"\n").unwrap();
    test_box.git_ok(&fixer, &["commit", "-qam", "lower the floor"]);
    test_box.git_ok(&fixer, &["push", "-q", "origin", "main"]);

    let (code, status) = result_json(test_box.bay(&repo).arg("status"));
    assert_eq!(code, 0, "the clone still reads the old floor: {status}");
}

/// 0.3.1: a config that names another main than this clone's says the flag that exists,
/// `bay install --main`.
#[test]
fn a_config_naming_another_main_names_install_main() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box, "repo", "trunk = \"other\"\n");
    let (_, status) = result_json(test_box.bay(&repo).arg("status"));
    let text = status.to_string();
    assert!(text.contains("bay install --main other"), "{status}");
}
