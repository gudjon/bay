mod support;

use predicates::str::contains;
use support::{TestBox, result_json};

#[test]
fn doctor_sha1_files() {
    let test_box = TestBox::new();
    let repo = test_box.repo("repo", "sha1", "files");
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["detail"]["object format"], "sha1");
    assert_eq!(result["detail"]["ref format"], "files");
    assert_eq!(result["detail"]["branch"], "main");
}

/// The Git 3.0 defaults.
#[test]
fn doctor_sha256_reftable() {
    let test_box = TestBox::new();
    let repo = test_box.repo("repo", "sha256", "reftable");
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["detail"]["object format"], "sha256");
    assert_eq!(result["detail"]["ref format"], "reftable");
    assert_eq!(result["detail"]["branch"], "main");
}

#[test]
fn doctor_outside_repo_fails() {
    let test_box = TestBox::new();
    test_box
        .bay(test_box.path())
        .arg("doctor")
        .assert()
        .code(20)
        .stdout(contains("\"decision\":\"failed\""))
        .stdout(contains("not a git repository"))
        .stderr(contains("not a git repository"));
}

/// Stdout is always the `bay.result/1` document (`INTERFACES` § 1); `--json` prints the same
/// document and no human text.
#[test]
fn doctor_json_is_bay_result() {
    let test_box = TestBox::new();
    let repo = test_box.repo("repo", "sha1", "files");
    test_box
        .bay(&repo)
        .arg("--json")
        .arg("doctor")
        .assert()
        .success()
        .stdout(contains("\"schema\":\"bay.result/1\""))
        .stdout(contains("\"decision\":\"ok\""))
        .stderr("");
}

/// The human status line is on stderr; stdout stays the result document.
#[test]
fn doctor_human_rendering_is_one_status_line() {
    let test_box = TestBox::new();
    let repo = test_box.repo("repo", "sha1", "files");
    let out = test_box.bay(&repo).arg("doctor").output().unwrap();
    let document: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(document["schema"], "bay.result/1");
    let text = String::from_utf8(out.stderr).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.starts_with("bay "), "{text}");
    assert!(text.trim_end().ends_with("| doctor ready"), "{text}");
}
