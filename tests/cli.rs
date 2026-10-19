mod support;

use predicates::str::contains;
use support::{TestBox, result_json, row};

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

/// Outside a Git repository doctor still returns one `bay.result/1` document with the
/// machine-level rows (the Git floor of any machine running bay, Claude Code, the installed
/// skill). No remote is configured there, so the box is not ready (`DESIGN` § 15.2): the
/// decision is `blocked`, exit 10, and the repository fields are `null`. No raw "not a git
/// repository" fault.
#[test]
fn doctor_outside_a_repository_reports_the_machine_rows() {
    let test_box = TestBox::new();
    let (code, result) = result_json(test_box.bay(test_box.path()).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["schema"], "bay.result/1");
    assert_eq!(result["decision"], "blocked");
    assert_eq!(result["reason"], "REMOTE_UNREACHABLE");
    for key in [
        "repository",
        "object format",
        "ref format",
        "branch",
        "role",
    ] {
        let value = result["detail"].get(key);
        assert!(
            value.is_some_and(serde_json::Value::is_null),
            "{key}: {result}"
        );
    }
    let git = row(&result, "git");
    assert!(git["role"].is_null(), "{result}");
    assert_eq!(git["floor"], "2.40");
    assert_eq!(git["state"], "green");
    assert_eq!(row(&result, "claude")["state"], "green");
    let skill = row(&result, "skill");
    assert_eq!(skill["state"], "missing", "{result}");
    assert_eq!(skill["required"], false);
    assert_eq!(
        skill["path"],
        test_box
            .home
            .join(".claude/skills/bay")
            .display()
            .to_string()
    );
    assert_eq!(
        row(&result, "remote")["state"],
        "not configured",
        "{result}"
    );
    assert_eq!(row(&result, "hooks")["state"], "not applicable", "{result}");
    let out = test_box
        .bay(test_box.path())
        .arg("doctor")
        .output()
        .unwrap();
    let text = String::from_utf8(out.stderr).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(!text.contains("not a git repository"), "{text}");
    assert!(
        text.trim_end()
            .ends_with("| doctor not ready (not in a git repository)"),
        "{text}"
    );
}

/// A damaged repository is not "outside": Git prints its discovery message for a `.git` whose
/// objects are gone, and a gitfile can point at an admin directory that was pruned. Doctor's
/// probe cannot finish its read, so the result is an operation fault (exit 20, `DESIGN`
/// § 15.2), never the machine rows with a verdict.
#[test]
fn doctor_in_a_damaged_repository_is_a_fault_not_outside() {
    let test_box = TestBox::new();
    let broken = test_box.repo_any("broken");
    std::fs::remove_dir_all(broken.join(".git/objects")).unwrap();
    let pruned = test_box.path().join("pruned");
    std::fs::create_dir_all(&pruned).unwrap();
    let gone = test_box.path().join("gone/.git/worktrees/x");
    std::fs::write(pruned.join(".git"), format!("gitdir: {}\n", gone.display())).unwrap();
    for dir in [broken.clone(), broken.join("sub"), pruned] {
        std::fs::create_dir_all(&dir).unwrap();
        let (code, result) = result_json(test_box.bay(&dir).arg("doctor"));
        assert_eq!(code, 20, "{}: {result}", dir.display());
        assert_eq!(result["decision"], "failed", "{result}");
    }
}

/// The machine rows are still judged outside a repository: Git below the floor is red.
#[test]
fn doctor_outside_a_repository_is_red_below_the_git_floor() {
    let test_box = TestBox::new();
    test_box.fake_git_version("2.39.0");
    let (code, result) = result_json(test_box.bay(test_box.path()).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "GIT_BELOW_FLOOR");
    assert_eq!(row(&result, "git")["recovery"], "upgrade git");
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
