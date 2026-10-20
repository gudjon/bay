//! Ruling R14: a check never runs unconfined unless the box chose it.
//!
//! On a machine with no check sandbox, a check is `infra_failure` and an input provider does
//! not run, unless the box has chosen `git config --global bay.executorSandbox none`; each
//! record of a run under that choice says `sandbox: none`. A repository cannot make that
//! choice: it is the box's, like its update pin. Doctor's sandbox row is red on a box that runs
//! checks (`bay.executorKey` or `bay.producerKey`) and has neither a sandbox nor the choice,
//! and outside a repository the result still names `REMOTE_UNREACHABLE` first.
//!
//! The sandbox is hidden by a `bwrap` on `PATH` that fails its probe, so these cases run on
//! Linux only, with or without a real `bwrap`.

#![cfg(target_os = "linux")]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json, row};

/// A trunk whose one check passes every candidate, and a change on `src/` with one new file.
/// Returns (box, view, remote).
fn submitted() -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let check = check_files("", &passing_result(1), 1, 60);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &check);
    let (_id, view) = test_box.start_and_edit(&repo);
    (test_box, view, remote)
}

fn land(test_box: &TestBox, view: &Path) -> (i32, serde_json::Value) {
    result_json(test_box.bay(view).args(["land", "--wait"]))
}

#[test]
fn a_passing_check_with_no_sandbox_and_no_choice_is_infra_failure() {
    let (test_box, view, remote) = submitted();
    test_box.no_sandbox();
    test_box.choose_unsandboxed(false);
    let (code, result) = land(&test_box, &view);
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "CHECK_UNKNOWN", "{result}");
    let check = &result["detail"]["checks"][0];
    assert_eq!(check["verdict"], "infra_failure", "{result}");
    assert!(
        result.to_string().contains("bay.executorSandbox"),
        "the refusal does not name the box's choice: {result}"
    );
    assert_eq!(test_box.trunk_change_ids(&remote), Vec::<String>::new());
}

#[test]
fn the_boxs_choice_runs_the_check_and_its_record_says_none() {
    let (test_box, view, remote) = submitted();
    test_box.no_sandbox();
    test_box.choose_unsandboxed(true);
    let (code, result) = land(&test_box, &view);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["decision"], "landed", "{result}");
    assert_eq!(test_box.trunk_change_ids(&remote).len(), 1);
    let notes = test_box.git_ok(
        &view,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/notes/bay/evidence/",
        ],
    );
    let records: String = notes
        .lines()
        .flat_map(|notes_ref| {
            test_box
                .git_ok(
                    &view,
                    &["ls-tree", "-r", "--format=%(objectname)", notes_ref],
                )
                .lines()
                .map(|blob| test_box.git_ok(&view, &["cat-file", "blob", blob]))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(
        records.contains("\"sandbox\": \"none\""),
        "no record says sandbox none: {records}"
    );
}

#[test]
fn a_value_other_than_none_is_never_the_choice() {
    let (test_box, view, remote) = submitted();
    test_box.no_sandbox();
    test_box.git_ok(
        test_box.path(),
        &["config", "--global", "bay.executorSandbox", "off"],
    );
    let (code, result) = land(&test_box, &view);
    assert_eq!(code, 10, "{result}");
    assert_eq!(
        result["detail"]["checks"][0]["verdict"], "infra_failure",
        "{result}"
    );
    assert_eq!(test_box.trunk_change_ids(&remote), Vec::<String>::new());
}

#[test]
fn a_repository_cannot_choose_to_run_checks_unconfined() {
    // The committed config asks for no sandbox; the box has not chosen it. Bay refuses the
    // config itself, so no change starts and nothing runs under it.
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let mut check = check_files("", &passing_result(1), 1, 60);
    check.push((
        ".bay/config.toml".to_owned(),
        "[executor]\nsandbox = \"none\"\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &check);
    test_box.no_sandbox();
    test_box.choose_unsandboxed(false);
    let (code, result) = result_json(
        test_box
            .bay(&repo)
            .args(["start", "CARD-1", "--paths", "src/"]),
    );
    assert_ne!(code, 0, "{result}");
    assert!(
        result.to_string().contains("bay.executorSandbox"),
        "the refusal does not name the box's own setting: {result}"
    );
}

/// A box in the executor role: `bay.executorKey` in its global Git config.
fn executor_box() -> TestBox {
    let test_box = TestBox::new();
    let key = test_box.path().join("executor-key");
    test_box.git_ok(
        test_box.path(),
        &[
            "config",
            "--global",
            "bay.executorKey",
            key.to_str().unwrap(),
        ],
    );
    test_box
}

#[test]
fn doctor_is_red_for_an_executor_with_no_sandbox_and_no_choice() {
    let test_box = executor_box();
    let repo = test_box.repo_any("repo");
    test_box.no_sandbox();
    test_box.choose_unsandboxed(false);
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "SANDBOX_ABSENT", "{result}");
    let sandbox = row(&result, "sandbox");
    assert_eq!(sandbox["state"], "red", "{result}");
    assert_eq!(sandbox["kind"], "none", "{result}");

    test_box.choose_unsandboxed(true);
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    assert_eq!(row(&result, "sandbox")["state"], "green", "{result}");
}

#[test]
fn doctor_outside_a_repository_names_the_remote_first() {
    let test_box = executor_box();
    test_box.no_sandbox();
    test_box.choose_unsandboxed(false);
    let outside = test_box.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let (code, result) = result_json(test_box.bay(&outside).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "REMOTE_UNREACHABLE", "{result}");
    assert_eq!(row(&result, "sandbox")["state"], "red", "{result}");
    // Outside a repository the box's choice still clears the row.
    test_box.choose_unsandboxed(true);
    let (_, result) = result_json(test_box.bay(&outside).arg("doctor"));
    assert_eq!(row(&result, "sandbox")["state"], "green", "{result}");
}

#[test]
fn an_agent_view_box_with_no_sandbox_is_green() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.no_sandbox();
    test_box.choose_unsandboxed(false);
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    assert_eq!(row(&result, "sandbox")["state"], "green", "{result}");
}
