//! G63. Doctor states whether this box can execute.
//!
//! `bay doctor --role executor` judges the box as an executor box whatever role doctor would
//! infer: the executor's Git floor (2.50, `GIT-FEATURES` § 1), a check sandbox or the box's
//! choice to run without one (ruling R14), the remote, each `env` name of a required check set
//! here, and, under a registry, the executor and producer keys set and enrolled. Without
//! `--role` an unsigned box is reported as an agent view, and the result says the role was
//! inferred.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, result_json, row};

#[test]
fn git_2_49_is_red_for_a_stated_executor_and_the_inferred_role_says_so() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.fake_git_version("2.49.0");

    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["detail"]["role"], "agent-view", "{result}");
    assert_eq!(result["detail"]["role_source"], "inferred", "{result}");
    assert_eq!(row(&result, "git")["floor"], "2.40", "{result}");

    let (code, result) = result_json(test_box.bay(&repo).args(["doctor", "--role", "executor"]));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["decision"], "blocked");
    assert_eq!(result["reason"], "TOOL_BELOW_FLOOR");
    assert_eq!(result["detail"]["role"], "executor", "{result}");
    assert_eq!(result["detail"]["role_source"], "stated", "{result}");
    let git = row(&result, "git");
    assert_eq!(git["role"], "executor");
    assert_eq!(git["floor"], "2.50");
    assert_eq!(git["state"], "red");

    test_box.fake_git_version("2.50.0");
    let (code, result) = result_json(test_box.bay(&repo).args(["doctor", "--role", "executor"]));
    assert_eq!(code, 0, "{result}");
    assert_eq!(row(&result, "git")["state"], "green");
}

/// The sandbox is hidden by a `bwrap` on `PATH` that fails its probe: Linux only.
#[cfg(target_os = "linux")]
#[test]
fn a_stated_executor_with_no_sandbox_and_no_choice_is_red() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.no_sandbox();
    test_box.choose_unsandboxed(false);

    // An unsigned box holds no executor key, so its inferred role runs no checks.
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    assert_eq!(row(&result, "sandbox")["required"], false, "{result}");

    let (code, result) = result_json(test_box.bay(&repo).args(["doctor", "--role", "executor"]));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "SANDBOX_ABSENT", "{result}");
    let sandbox = row(&result, "sandbox");
    assert_eq!(sandbox["required"], true, "{result}");
    assert_eq!(sandbox["state"], "red", "{result}");
    let recovery = sandbox["recovery"].as_str().unwrap();
    assert!(!recovery.contains("AppArmor"), "{result}");

    // G62b: under the kernel's user-namespace restriction, the red row names its fix, and still
    // the choice to run without a sandbox.
    test_box.userns_restricted(true);
    let (_, result) = result_json(test_box.bay(&repo).args(["doctor", "--role", "executor"]));
    let recovery = row(&result, "sandbox")["recovery"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(recovery.contains("AppArmor"), "{result}");
    assert!(recovery.contains("/fakebin/bwrap"), "{result}");
    assert!(recovery.contains("bay.executorSandbox none"), "{result}");
    test_box.userns_restricted(false);

    test_box.choose_unsandboxed(true);
    let (code, result) = result_json(test_box.bay(&repo).args(["doctor", "--role", "executor"]));
    assert_eq!(code, 0, "{result}");
}

#[test]
fn a_required_checks_env_name_unset_here_is_red_and_set_is_green() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let mut check = support::check_files("", &support::passing_result(1), 1, 30);
    check[1].1.push_str("env = [\"CHECK_TOKEN\"]\n");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &check);

    // An agent view runs no checks, so the variable is not its concern.
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");

    let (code, result) = result_json(test_box.bay(&repo).args(["doctor", "--role", "executor"]));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "CHECK_ENV_UNSET", "{result}");
    let env = row(&result, "check-env");
    assert_eq!(env["state"], "red", "{result}");
    assert_eq!(
        env["unset"],
        serde_json::json!([{"check": "tests", "environment": "local", "env": "CHECK_TOKEN"}]),
        "{result}"
    );

    let (code, result) = result_json(
        test_box
            .bay(&repo)
            .env("CHECK_TOKEN", "set")
            .args(["doctor", "--role", "executor"]),
    );
    assert_eq!(code, 0, "a ready executor box: {result}");
    assert_eq!(row(&result, "check-env")["state"], "green", "{result}");
    assert_eq!(row(&result, "git")["role"], "executor", "{result}");
}

#[test]
fn under_a_registry_a_stated_executor_needs_its_keys_enrolled() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);

    let (code, result) = result_json(test_box.bay(&repo).args(["doctor", "--role", "executor"]));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "RUNNER_KEY_MISSING", "{result}");
    let row_keys = row(&result, "executor-keys");
    assert_eq!(row_keys["state"], "red", "{result}");
    assert_eq!(row_keys["runner"], "not set", "{result}");
    assert_eq!(row_keys["producer"], "not set", "{result}");

    test_box.use_keys(&repo, &keys);
    let (code, result) = result_json(test_box.bay(&repo).args(["doctor", "--role", "executor"]));
    assert_eq!(code, 0, "{result}");
    let row_keys = row(&result, "executor-keys");
    assert_eq!(row_keys["runner"], "enrolled", "{result}");
    assert_eq!(row_keys["producer"], "enrolled", "{result}");

    // A name the registry does not enrol is red too.
    test_box.git_ok(&repo, &["config", "bay.producerName", "ci-9"]);
    let (code, result) = result_json(test_box.bay(&repo).args(["doctor", "--role", "executor"]));
    assert_eq!(code, 10, "{result}");
    assert_eq!(
        row(&result, "executor-keys")["producer"],
        "not enrolled",
        "{result}"
    );
}
