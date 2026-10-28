//! G492 (a fleet finding, 2026-10-28: scripts-fast stayed red for three hours after both of its
//! causes were fixed in files off its paths, and every change on its paths waited on it). A red
//! post check runs again on a new tip of main even when nothing on its paths changed, so a fix
//! that landed off its paths clears it.
//!
//! A post check runs on a tip only when something on its `when_paths` changed since it last
//! answered (G439). That is right for a green check. A red one holds every change on its paths
//! (`MAIN_RED`), so nothing on its paths can change, and it never ran again.
//!
//! Guards to mutate, in `src/bisect.rs` `tip_runs`: the `is_red` exception to the `unwoken`
//! skip (main stays red and the next change on its paths waits).

#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use support::{TestBox, check_files, passing_result, result_json};

/// A post check `suite` on `src/**` whose tests read `conf/ok`, a file off its paths.
const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

/// `suite` fails while `conf/ok` is missing; every other check passes.
const BEFORE: &str =
    r#"fail=no; case "$req" in *'"plan":{"id":"suite"'*) [ -f conf/ok ] || fail=yes ;; esac"#;

#[test]
fn a_fix_off_the_red_checks_paths_clears_it_on_the_next_tip() {
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
    let submit = |name: &str, dir: &str, file: &str| -> String {
        let started = test_box.bay_ok(&repo, &["start", name, "--paths", dir]);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(file), "x\n").unwrap();
        let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
        assert_eq!(code, 0, "{merged}");
        id
    };

    // `one` lands on src/, and the post work answers `suite` red on it: conf/ok is missing.
    submit("one", "src", "src/one.rs");
    let ran = test_box.bay_ok(&repo, &["runner", "run", "--once"]);
    let status = test_box.bay_ok(&repo, &["status"]);
    assert_eq!(
        status["detail"]["red"][0]["check"], "suite",
        "suite is red: {ran}\n{status}"
    );

    // The fix lands off suite's paths, so it is never held by the red.
    let fix = submit("fix", "conf", "conf/ok");
    assert!(
        test_box.trunk_change_ids(&remote).contains(&fix),
        "a change off the red's paths lands"
    );
    // The post work on the new tip runs the red check again, and it passes.
    let ran = test_box.bay_ok(&repo, &["runner", "run", "--once"]);
    let status = test_box.bay_ok(&repo, &["status"]);
    assert_eq!(
        status["detail"]["red"],
        serde_json::json!([]),
        "the red check ran again on the new tip: {ran}\n{status}"
    );
    let two = submit("two", "src", "src/two.rs");
    assert!(
        test_box.trunk_change_ids(&remote).contains(&two),
        "the next change on suite's paths lands"
    );
}
