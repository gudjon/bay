//! G926 (a pre-deploy gate: a promotion takes the newest tip of main whose post checks all
//! passed, so it never promotes a red tip): `bay status --green-tip` names the newest landing at
//! which every post check is green. A landing whose post run has not answered, or one the red
//! sits on, is never named; a fix off the check's paths is named once its run passes.
//!
//! Guards to mutate, in `src/red.rs` `Answers::green_at`: read a landing that requires the
//! check unanswered as green, or a failure as green.

// Each case runs a shell adapter, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;

use support::{TestBox, check_files, passing_result, result_json};

/// A post check `suite` on `src/**` whose tests read `conf/ok`, a file off its paths.
const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

/// `suite` fails while `conf/ok` is missing; every other check passes.
const BEFORE: &str =
    r#"fail=no; case "$req" in *'"plan":{"id":"suite"'*) [ -f conf/ok ] || fail=yes ;; esac"#;

/// Main's tip on `remote`.
fn main_tip(test_box: &TestBox, remote: &Path) -> String {
    test_box
        .git_bare(remote, &["rev-parse", "refs/heads/main"])
        .trim()
        .to_owned()
}

/// What `bay status --green-tip` names: its `detail.green_tip`.
fn green_tip(test_box: &TestBox, repo: &Path) -> Value {
    let read = test_box.bay_ok(repo, &["status", "--green-tip"]);
    read["detail"]["green_tip"].clone()
}

#[test]
fn a_green_tip_read_never_names_a_tip_with_a_red_or_pending_post_check() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let pass = passing_result(1);
    let fail = pass.replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    assert_ne!(pass, fail, "the failing result must differ: {fail}");
    let result = format!("if [ \"$fail\" = yes ]; then {fail}; else {pass}; fi");
    let mut files = check_files(BEFORE, &result, 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(POST);
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\npost = true\nbisect_post = false\n".to_owned(),
    ));
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("conf/readme", "conf\n")],
        &files,
    );
    let seed = main_tip(&test_box, &remote);
    let submit = |name: &str, dir: &str, file: &str| {
        let started = test_box.bay_ok(&repo, &["start", name, "--paths", dir]);
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(file), "x\n").unwrap();
        let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
        assert_eq!(code, 0, "{merged}");
    };

    // GIVEN `one` landed on src/, its post run not yet answered
    submit("one", "src", "src/one.rs");
    let one = main_tip(&test_box, &remote);
    assert_ne!(one, seed);
    // THEN the green tip is the seed: `one` requires suite, unanswered
    let tip = green_tip(&test_box, &repo);
    assert_eq!(tip["commit"], seed.as_str(), "{tip}");
    assert_eq!(tip["above"], 1, "{tip}");

    // WHEN the post work answers suite red on `one`
    let ran = test_box.bay_ok(&repo, &["runner", "run", "--once"]);
    let status = test_box.bay_ok(&repo, &["status"]);
    assert_eq!(
        status["detail"]["red"][0]["check"], "suite",
        "suite is red: {ran}\n{status}"
    );
    // THEN the red tip is never named
    assert_eq!(green_tip(&test_box, &repo)["commit"], seed.as_str());

    // WHEN a fix lands off suite's paths, its run not yet answered
    submit("fix", "conf", "conf/ok");
    let fix = main_tip(&test_box, &remote);
    assert_ne!(fix, one);
    // THEN the fix is not green: suite's newest answer below it failed
    assert_eq!(green_tip(&test_box, &repo)["commit"], seed.as_str());

    // WHEN the post work runs suite again on the fix, and it passes
    let ran = test_box.bay_ok(&repo, &["runner", "run", "--once"]);
    let status = test_box.bay_ok(&repo, &["status"]);
    assert_eq!(
        status["detail"]["red"],
        serde_json::json!([]),
        "suite passed on the fix: {ran}\n{status}"
    );
    // THEN the fix is the green tip
    let tip = green_tip(&test_box, &repo);
    assert_eq!(tip["commit"], fix.as_str(), "{tip}");
    assert_eq!(tip["above"], 0, "{tip}");
}
