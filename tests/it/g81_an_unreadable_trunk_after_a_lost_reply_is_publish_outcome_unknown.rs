//! G81: a publishing push whose reply is lost, with trunk then unreadable, is
//! `MERGE_OUTCOME_UNKNOWN`, never `failed` (`DESIGN` § 9, `INTERFACES` § 2).
//!
//! The remote moved trunk and the connection dropped before its report, then the remote went
//! away. bay cannot know whether C landed, so it says so and acts on nothing; once trunk can be
//! read again, the next land of the change reconciles: the landing is found, never repeated.

// Each case runs shell scripts (adapters, a remote hook), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_lost_reply_then_an_unreadable_trunk_is_reconciled_never_failed() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let away = test_box.path().join("repo-remote.git.away");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, submitted) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");

    // Once trunk's update is committed, the remote drops the connection before its report and
    // goes away.
    let hooks = remote.join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("reference-transaction");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\n[ \"$1\" = committed ] || exit 0\ngrep -q ' refs/heads/main$' || exit 0\nrm -f \"$0\"\nmv '{remote}' '{away}'\nkill -9 $PPID\n",
            remote = remote.display(),
            away = away.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let executor = test_box.clone_of(&remote, "executor");
    let (_, ran) = result_json(test_box.bay(&executor).args(["runner", "run", "--once"]));
    let result = &ran["detail"]["ran"][0];
    assert_eq!(result["reason"], "MERGE_OUTCOME_UNKNOWN", "{ran}");
    assert_eq!(result["decision"], "blocked", "never failed: {ran}");
    assert!(away.exists(), "the remote moved trunk before it went away");

    // The remote is back: the change's next land finds its landing and publishes nothing more.
    std::fs::rename(&away, &remote).unwrap();
    let landed_commit = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let (code, again) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["decision"], "merged", "{again}");
    assert_eq!(again["detail"]["commit"], landed_commit, "{again}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}
