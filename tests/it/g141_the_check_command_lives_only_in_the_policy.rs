//! G141. The check command lives only in the policy.
//!
//! `[check] command` in `.bay/config.toml` is retired (ruling Q14): a config that still carries
//! it loads, nothing reads it, and `bay doctor` names it as ignored. The command a check runs
//! is the one its `[[check]]` entry in `.bay/policy.toml` names, judged at the accepted base and
//! hashed into its definition digest, so no second home outside the policy can change what runs.

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json, row, write_executable};

/// The command the retired key names. It fails every candidate and leaves a marker, so a bay
/// that ran it would refuse the landing.
const OTHER: &str = "scripts/other";

#[test]
fn land_runs_only_the_policys_command_and_doctor_names_the_retired_key() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let marker = test_box.path().join("other-ran");
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let other = check_files(
        &format!("touch '{}' 2>/dev/null", marker.display()),
        &failing,
        1,
        60,
    )
    .into_iter()
    .find(|(path, _)| path == "scripts/bay-check")
    .unwrap()
    .1;
    // Executable before the seed writes it, so its mode is committed.
    std::fs::create_dir_all(repo.join("scripts")).unwrap();
    write_executable(&repo.join(OTHER), &other);
    let mut files = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            // Both scripts are adapter files, so a bay that fell back to the config's command
            // would find it at the base.
            *text = text.replace(
                "adapter = [\"scripts/bay-check\"]",
                "adapter = [\"scripts/bay-check\", \"scripts/other\"]",
            );
        }
    }
    files.push((OTHER.to_owned(), other));
    files.push((
        ".bay/config.toml".to_owned(),
        format!("[check]\ncommand = \"{OTHER}\"\n"),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);

    // The config loads: status answers, not a config error.
    let status = test_box.bay_ok(&repo, &["status"]);
    assert_ne!(status["reason"], "CONFIG_INVALID", "{status}");

    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "landed", "{landed}");
    assert!(!marker.exists(), "the config's command ran");
    let remote = test_box.path().join("repo-remote.git");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);

    let (_, doctor) = result_json(test_box.bay(&repo).arg("doctor"));
    let config = row(&doctor, "config");
    assert_eq!(config["state"], "green", "{config}");
    assert_eq!(
        config["ignored"],
        serde_json::json!(["[check] command"]),
        "{config}"
    );
    assert!(
        config["note"]
            .as_str()
            .unwrap()
            .contains(".bay/policy.toml"),
        "{config}"
    );
}

/// A config without the retired key has no ignored keys to name.
#[test]
fn doctor_names_nothing_ignored_without_the_retired_key() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (_, doctor) = result_json(test_box.bay(&repo).arg("doctor"));
    let ignored = doctor["detail"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["name"] == "config" && row.get("ignored").is_some());
    assert!(!ignored, "{doctor}");
}
