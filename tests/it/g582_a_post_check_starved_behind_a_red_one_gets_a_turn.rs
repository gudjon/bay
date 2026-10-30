//! G582 (a fleet finding, 2026-10-30: one runner served the environment of edge-py-fast and
//! scripts-fast; edge-py-fast was red and each of its runs took ~30 min, so scripts-fast ran on no
//! tip for 49 landings and passed the red window, where a failure can no longer be ranged). A
//! red post check goes first on each new tip (G492), and a pass plans one post check an
//! environment (#123), so the other checks of its environment never ran. Now a check this box has
//! not answered for half the red window goes before the red one.
//!
//! Guard to mutate: the `starved` term of the sort key in `bisect::in_turn` (`src/bisect.rs`);
//! drop it and the red check runs on every tip, and `lint` on none.

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

/// A post check on `paths`, run as `scripts/bay-check <id>`.
fn post(id: &str, paths: &str) -> String {
    format!(
        "\n[[check]]\nid = \"{id}\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check {id}\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"{paths}\"]\n"
    )
}

/// Lands a change that writes `path` in `dir`.
fn land(test_box: &TestBox, repo: &Path, dir: &str, path: &str) {
    let started = test_box.bay_ok(repo, &["start", path, "--paths", dir]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(path), format!("{path}\n")).unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
}

/// One runner pass: the post check it ran, or `""`.
fn ran_post(test_box: &TestBox, repo: &Path) -> String {
    let ran = test_box.bay_ok(repo, &["runner", "run", "--once"]);
    ran["detail"]["ran"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|row| row.get("post"))
        .and_then(|post| post[0]["check"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

#[test]
fn a_check_unanswered_for_half_the_window_runs_before_a_red_one() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    // `suite` (on src/) fails on every tree; `lint` (on conf/) passes. Both run on `local`.
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let mut files = check_files(
        &format!("[ \"$1\" = suite ] && {{ {failing}; exit 0; }}"),
        &passing_result(1),
        1,
        60,
    );
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(&post("suite", "src/**"));
            text.push_str(&post("lint", "conf/**"));
        }
    }
    // A red window of 4 landings: a check unanswered for 2 is starved.
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\npost = true\nbisect_post = false\n\n[red]\nwindow = 4\n".to_owned(),
    ));
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("conf/readme", "conf\n")],
        &files,
    );
    let mut ran = Vec::new();
    // One landing on src/ wakes `suite`, and it turns main red.
    land(&test_box, &repo, "src", "src/s0.rs");
    ran.push(ran_post(&test_box, &repo));
    let status = test_box.bay_ok(&repo, &["status"]);
    assert_eq!(
        status["detail"]["red"][0]["check"], "suite",
        "suite is red: {ran:?}\n{status}"
    );
    // Then landings off suite's paths, so the red holds none of them, and one pass after each.
    for n in 1..=4 {
        land(&test_box, &repo, "conf", &format!("conf/c{n}"));
        ran.push(ran_post(&test_box, &repo));
    }
    // `lint` never ran, so it is starved on the first tip after the red; then the red check runs
    // again; then `lint` has gone 2 landings unanswered, and so on.
    assert_eq!(
        ran,
        ["suite", "lint", "suite", "lint", "suite"],
        "a red check went first on every tip and starved lint"
    );
}
