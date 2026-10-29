//! G532 (bay 0.6.1, B5 with 0.6.0's S4). The read past a red check's window for its last pass
//! is paid once: a later judgement in the same runner keeps the answers of the landings past
//! the window whose notes did not move, as it does inside the window (G482), and reads only the
//! new landings and those whose notes moved.
//!
//! The cost, from the trace's `read N` (S4): a window of 3 that holds no pass, the pass nine
//! landings below main's tip. Before B5 the first head read the 3 landings of the window and
//! named no range end; with B5 it reads the 9 back to the pass, and the next head reads at most
//! 2, not the 9 again.
//!
//! Guards to mutate, in `src/red.rs` `Answers::past`: keep no answers past the window (the next
//! head reads the range again); keep the answers of a landing whose notes moved (covered by
//! G482's second case for the window, the same filter).

// Each case runs a shell adapter, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// Three post checks on `src/**`; `suite` fails on any tree holding `src/bad.rs`.
fn post(id: &str) -> String {
    format!(
        "\n[[check]]\nid = \"{id}\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check {id}\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n"
    )
}

/// Lands a change that writes `path` (under `src/` or `docs/`), with `bay merge --wait`: its
/// commit on main.
fn land(test_box: &TestBox, repo: &Path, path: &str) -> String {
    let dir = path.split('/').next().unwrap();
    let started = test_box.bay_ok(repo, &["start", path, "--paths", &format!("{dir}/")]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(path), format!("{path}\n")).unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    landed["detail"]["commit"].as_str().unwrap().to_owned()
}

/// G482's trunk with `[red] window = 3`: runners run post checks.
fn trunk() -> (TestBox, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let mut files = check_files(
        &format!("[ \"$1\" = suite ] && [ -e src/bad.rs ] && {{ {failing}; exit 0; }}"),
        &passing_result(1),
        1,
        60,
    );
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            for id in ["suite", "lint", "style"] {
                text.push_str(&post(id));
            }
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[red]\nwindow = 3\n[runner]\npost = true\n".to_owned(),
    ));
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("docs/readme.md", "readme\n")],
        &files,
    );
    (test_box, repo)
}

/// Runner passes in `repo` until `done` holds of `bay status`, at most `passes`.
fn passes_until(test_box: &TestBox, repo: &Path, passes: usize, done: impl Fn(&Value) -> bool) {
    for _ in 0..passes {
        let (code, ran) = result_json(test_box.bay(repo).args(["runner", "run", "--once"]));
        assert_eq!(code, 0, "{ran}");
        if done(&test_box.bay_ok(repo, &["status"])) {
            return;
        }
    }
    panic!(
        "not reached in {passes} passes: {}",
        test_box.bay_ok(repo, &["status"])
    );
}

#[test]
fn the_next_head_does_not_read_past_the_window_again() {
    let (test_box, repo) = trunk();
    // Every post check passes on `good`; eight landings later `suite` fails on main's tip.
    let good = land(&test_box, &repo, "src/good.rs");
    passes_until(&test_box, &repo, 6, |status| {
        status["detail"]["red_pending"]
            .as_array()
            .is_some_and(Vec::is_empty)
    });
    land(&test_box, &repo, "src/bad.rs");
    for n in 0..7 {
        land(&test_box, &repo, &format!("src/more{n}.rs"));
    }
    passes_until(&test_box, &repo, 6, |status| {
        status["detail"]["red"]
            .as_array()
            .is_some_and(|red| !red.is_empty())
    });
    // Two docs changes, which the red does not hold, merge in one pass: the second head judges
    // main from the first one's judgement, one landing later.
    for name in ["d1", "d2"] {
        let started = test_box.bay_ok(&repo, &["start", name, "--paths", "docs/"]);
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(format!("docs/{name}.md")), "doc\n").unwrap();
        let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
        assert_eq!(code, 0, "{queued}");
    }
    let trace = test_box.path().join("red-trace");
    let (code, ran) = result_json(
        test_box
            .bay(&repo)
            .args(["runner", "run", "--once"])
            .env("BAY_TEST_RED_TRACE", &trace),
    );
    assert_eq!(code, 0, "{ran}");
    let merged = ran["detail"]["ran"].as_array().map_or(0, |ran| {
        ran.iter().filter(|op| op["decision"] == "merged").count()
    });
    assert_eq!(merged, 2, "both docs changes merged in the pass: {ran}");
    let judged = std::fs::read_to_string(&trace).unwrap_or_default();
    let count_of = |line: &str| -> usize {
        line.split_whitespace()
            .skip_while(|word| *word != "read")
            .nth(1)
            .and_then(|count| count.parse().ok())
            .unwrap_or_else(|| panic!("a judgement names no landings read: {judged}"))
    };
    let lines: Vec<&str> = judged.lines().collect();
    assert!(lines.len() >= 2, "{judged}");
    let (first, second) = (count_of(lines[0]), count_of(lines[1]));
    eprintln!(
        "G532: window 3, no pass in it: the first head read {first} landings, the next {second}"
    );
    let status = test_box.bay_ok(&repo, &["status"]);
    let red = &status["detail"]["red"][0];
    assert_eq!(
        red["suspect"]["last_passed"]["merge"],
        good.as_str(),
        "the range's good end is the pass past the window: {red}"
    );
    assert!(
        first >= 9,
        "the first judgement did not read back to the pass: {judged}"
    );
    assert!(
        second <= 2,
        "the second head read {second} landings, the first {first}: {judged}"
    );
}
