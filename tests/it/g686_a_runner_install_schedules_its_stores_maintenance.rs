//! G686 (a fleet finding, 2026-11-09: one runner's store was repacked by every fetch until 556
//! git processes held 20 GB; another's was not repacked for 10 days behind a stale lock). The one
//! command `bay runner install` prints starts Git's scheduled maintenance of the repository
//! (`git maintenance start`: hourly, one run at a time, incremental) before it loads the
//! service, so the store is kept on a schedule and never by its fetches (G685).
//!
//! Run here against Git's test scheduler (`GIT_TEST_MAINT_SCHEDULER`), so nothing is scheduled.
//!
//! Guard to mutate: in `src/service.rs` `install_in`, load the service alone (the command starts
//! no maintenance, and the store is not registered).

#![cfg(any(target_os = "macos", target_os = "linux"))]

use crate::support;

use std::path::PathBuf;

use support::{TestBox, result_json, unchecked_files};

#[test]
fn a_runner_install_schedules_its_stores_maintenance() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let common = PathBuf::from(test_box.git_ok(
        &repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ));
    let root = common.parent().unwrap().to_path_buf();

    // GIVEN the command `bay runner install` prints
    let (code, installed) = result_json(test_box.bay(&root).args(["runner", "install"]));
    assert_eq!(code, 0, "{installed}");
    let load = installed["detail"]["load"].as_str().unwrap().to_owned();
    let (maintain, _) = load
        .split_once(" && ")
        .unwrap_or_else(|| panic!("the command starts no maintenance: {load}"));
    assert!(maintain.ends_with(" maintenance start"), "{load}");

    // WHEN its maintenance part runs (on Git's test scheduler)
    let ran = std::process::Command::new("sh")
        .args(["-c", maintain])
        .env("HOME", &test_box.home)
        .env("XDG_CONFIG_HOME", test_box.home.join(".config"))
        .env(
            "GIT_TEST_MAINT_SCHEDULER",
            "launchctl:true,systemctl:true,crontab:true",
        )
        .output()
        .unwrap();
    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );

    // THEN the store is registered for hourly, incremental maintenance
    let global = std::process::Command::new("git")
        .args(["config", "--global", "--get-all", "maintenance.repo"])
        .env("HOME", &test_box.home)
        .env("XDG_CONFIG_HOME", test_box.home.join(".config"))
        .output()
        .unwrap();
    let registered = String::from_utf8_lossy(&global.stdout).into_owned();
    let real = |path: &str| std::fs::canonicalize(path).unwrap_or_else(|_| path.into());
    assert!(
        registered
            .lines()
            .any(|line| real(line) == real(&root.display().to_string())),
        "{} is not registered: {registered}",
        root.display()
    );
    assert_eq!(
        test_box.git_ok(&root, &["config", "maintenance.strategy"]),
        "incremental"
    );
}
