//! G444. A box that serves none of the policy's post checks judges nothing for post work, and
//! still refuses a change on a red main.
//!
//! After its queue's work, every runner ran the post work (`crate::bisect::tips` and the culprit
//! finding `crate::bisect::run`): each judged main's red over the whole window, and only then
//! skipped every post check whose environment the box does not serve. On a fleet where one runner
//! serves the post checks, the three that serve none spent 140-166 s of every pass there
//! (2026-10-28, BACKLOG 113), and a newly submitted change waited for that pass to end. Now a box
//! that serves no post check's environment judges nothing there.
//!
//! The post work was also the only reader of the post records on such a box: a head's red gate
//! judges main from the box's own copy of them. So the pass now reads them once before any visit,
//! and a red another runner recorded since this box's last pass refuses a change on its paths.
//!
//! The guards to mutate: the `serves_a_post_check` returns in `bisect::tip_runs` and
//! `bisect::all` (`src/bisect.rs`; the first case judges), and the pass's `fetch_evidence` before
//! its visits in `executor::pass` (`src/executor.rs`; the second case lands on a red main).

// Each case runs shell scripts (the adapter), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::talk::fresh_key;
use support::{TestBox, check_files, passing_result, result_json};

/// A post check `suite` in the environment `post`, on the paths under `src/`.
const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"post\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

/// A trunk with the pre check `tests` (environment `local`) and the post check `suite`
/// (environment `post`), the post work on; returns the box, the main checkout and the remote.
fn trunk() -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
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
    (test_box, repo, remote)
}

/// Starts change `name` on `src/`, writes `src/<name>.rs` and submits it; returns its id.
fn submit(test_box: &TestBox, repo: &Path, name: &str, wait: bool) -> String {
    let started = test_box.bay_ok(repo, &["start", name, "--paths", "src"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(format!("src/{name}.rs")), "fn x() {}\n").unwrap();
    let mut args = vec!["merge"];
    if wait {
        args.push("--wait");
    }
    let (code, submitted) = result_json(test_box.bay(&view).args(&args));
    assert_eq!(code, 0, "{submitted}");
    id
}

/// The red judgements one `bay runner run --once` in `repo` made, from the test build's trace.
fn judgements(test_box: &TestBox, repo: &Path, name: &str) -> Vec<String> {
    let trace = test_box.path().join(format!("{name}-red-trace"));
    let (code, ran) = result_json(
        test_box
            .bay(repo)
            .args(["runner", "run", "--once"])
            .env("BAY_TEST_RED_TRACE", &trace),
    );
    assert_eq!(code, 0, "{ran}");
    std::fs::read_to_string(&trace)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_box_that_serves_no_post_check_judges_nothing_for_post_work() {
    let (test_box, repo, _) = trunk();
    submit(&test_box, &repo, "one", true);
    // Control: a box that serves the post check judges main in its post work.
    let serving = judgements(&test_box, &repo, "serving");
    assert!(
        !serving.is_empty(),
        "the post work of a serving box made no judgement: the trace sees none"
    );
    // This box serves the pre check's environment only, so the post check is never its to run.
    test_box.git_ok(&repo, &["config", "bay.checkEnvironment", "local"]);
    let pre_only = judgements(&test_box, &repo, "pre-only");
    assert!(
        pre_only.is_empty(),
        "post work judged main on a box that serves no post check: {pre_only:?}"
    );
}

#[test]
fn a_box_that_serves_no_post_check_refuses_a_change_on_a_red_main_recorded_since() {
    let (test_box, repo, remote) = trunk();
    submit(&test_box, &repo, "one", true);
    // This box runs the pre check, so only main's red keeps a change on `src/` off main.
    test_box.git_ok(&repo, &["config", "bay.checkEnvironment", "local"]);
    // A pass of this box reads the post records as they are: none yet.
    let _ = test_box.bay_ok(&repo, &["runner", "run", "--once"]);
    // Another box answers the post check on main's tip: a product failure.
    let producer = test_box.clone_of(&remote, "suite-box");
    let key = fresh_key(&test_box, "suite-1");
    test_box.git_ok(&producer, &["config", "bay.producerName", "suite-1"]);
    test_box.git_ok(
        &producer,
        &["config", "bay.producerKey", key.to_str().unwrap()],
    );
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let (code, written) = result_json(test_box.bay(&producer).args([
        "evidence",
        "record",
        "--kind",
        "check",
        "--check",
        "suite",
        "--environment",
        "post",
        "--verdict",
        "product_failure",
        "--commit",
        tip.trim(),
    ]));
    assert_eq!(code, 0, "{written}");
    // A change on the red paths reaches this box's next pass.
    let id = submit(&test_box, &repo, "two", false);
    let ran = test_box.bay_ok(&repo, &["runner", "run", "--once"]);
    assert!(
        !test_box.trunk_change_ids(&remote).contains(&id),
        "a change landed on a red main: {ran}"
    );
    assert!(ran.to_string().contains("MAIN_RED"), "{ran}");
}
