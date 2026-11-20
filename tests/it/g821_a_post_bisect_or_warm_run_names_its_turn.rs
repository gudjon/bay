//! G821 (a fleet finding, 2026-11-19: of 409 runs of one post check in a day, 35 were bisect
//! steps, and its adapter could not tell them from runs on main's tip). A check request names its
//! run in `turn`: `"post"` for a post check's run on main's tip, `"bisect"` for a step of a red's
//! bisection, `"warm"` for a warm run, as `"head"` and `"ahead"` name a queued change's runs.
//! An adapter that runs only the tests a range of changed paths reaches can then keep its whole
//! suite for a bisect step, whose range is not the one it judges.
//!
//! Guard to mutate: in `src/bisect.rs` `run_at`, the request's `turn` (`None`: no request of a
//! post or bisect run names it).

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check suite\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

/// A trunk whose post check `suite` fails on every tree holding `src/l3.rs` and writes each
/// request it is sent, one a line, to the returned file, outside the check's sandbox.
fn trunk() -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let log = test_box.path().join("suite-requests.jsonl");
    let logged = test_box.relay(&format!("printf '%s\\n' \"$req\" >> '{}'", log.display()));
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let mut files = check_files(
        &format!(
            "if [ \"$1\" = suite ]; then\n{logged}fi >/dev/null\n[ \"$1\" = suite ] && [ -e src/l3.rs ] && {{ {failing}; exit 0; }}"
        ),
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
    (test_box, repo, log)
}

fn land(test_box: &TestBox, repo: &Path, path: &str) {
    let started = test_box.bay_ok(repo, &["start", path, "--paths", "src"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(path), format!("{path}\n")).unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
}

/// The turn of each request `suite` was sent so far, oldest first.
fn turns(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["turn"].clone())
        .collect()
}

#[test]
fn a_post_run_and_a_bisect_step_name_their_turn() {
    let (test_box, repo, log) = trunk();
    land(&test_box, &repo, "src/l0.rs");
    test_box.bay_ok(&repo, &["runner", "run", "--once"]);
    assert_eq!(turns(&log), [Value::from("post")], "the run on main's tip");

    for path in ["src/l1.rs", "src/l2.rs", "src/l3.rs", "src/l4.rs"] {
        land(&test_box, &repo, path);
    }
    // The run on the new tip finds main red, and the runner bisects the four landings.
    for _ in 0..4 {
        test_box.bay_ok(&repo, &["runner", "run", "--once"]);
    }
    let turns = turns(&log);
    assert_eq!(turns[1], "post", "{turns:?}");
    assert!(turns.len() > 2, "no bisect step ran: {turns:?}");
    for turn in &turns[2..] {
        assert_eq!(turn, "bisect", "{turns:?}");
    }
}
