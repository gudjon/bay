//! G3x: a repository made on Git 3.0's defaults lands (`GIT-FEATURES` § 5).
//!
//! The box's global config sets every Git 3.0 default that Git 2.x can emulate: `main` as the
//! initial branch, SHA-256 objects, the reftable backend and `safe.bareRepository=explicit`.
//! A work repository is made with a plain `git init`; `bay host init`, run there with no
//! `--object-format`, makes a host that repository can push to, and outside any repository
//! takes Git's own default. Then the core flow runs: start, land through an executor, status,
//! watch, and the host hook still refuses a rewrite of trunk.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;

use support::{MINIMAL_CONFIG, TestBox, check_files, passing_result};

/// Every Git 3.0 default Git 2.x can be told to use.
const GIT_3_DEFAULTS: &str = "[init]\n\tdefaultBranch = main\n\tdefaultObjectFormat = sha256\n\
     \tdefaultRefFormat = reftable\n[safe]\n\tbareRepository = explicit\n";

fn git_3_box() -> TestBox {
    let test_box = TestBox::new();
    let mut config = std::fs::read_to_string(&test_box.global_config).unwrap();
    config.push_str(GIT_3_DEFAULTS);
    std::fs::write(&test_box.global_config, config).unwrap();
    test_box
}

/// `git init` with no options, as a user on Git 3.0 makes a repository.
fn plain_repo(test_box: &TestBox, name: &str) -> std::path::PathBuf {
    let dir = test_box.path().join(name);
    test_box.git_ok(test_box.path(), &["init", "-q", dir.to_str().unwrap()]);
    assert_eq!(
        test_box.git_ok(
            &dir,
            &["rev-parse", "--show-object-format", "--show-ref-format"]
        ),
        "sha256\nreftable"
    );
    assert_eq!(
        test_box.git_ok(&dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    TestBox::bay_config(&dir, MINIMAL_CONFIG);
    dir
}

fn host_init(test_box: &TestBox, dir: &Path, host: &Path) -> serde_json::Value {
    test_box.bay_ok(dir, &["host", "init", host.to_str().unwrap()])
}

#[test]
fn the_core_flow_lands_on_git_3_defaults() {
    let test_box = git_3_box();
    let repo = plain_repo(&test_box, "repo");
    let host = test_box.path().join("host.git");
    let made = host_init(&test_box, &repo, &host);
    assert_eq!(made["detail"]["object format"], "sha256", "{made}");
    assert_eq!(
        test_box.git_bare(
            &host,
            &["rev-parse", "--show-object-format", "--show-ref-format"]
        ),
        "sha256\nreftable"
    );
    test_box.add_origin(&repo, &host);
    let seed = test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let cursor = test_box.bay_ok(&repo, &["status", "--since"])["detail"]["cursor"]
        .as_str()
        .unwrap()
        .to_owned();

    let (id, view) = test_box.start_and_edit(&repo);
    let queued = test_box.bay_ok(&view, &["merge"]);
    assert_ne!(queued["decision"], "merged", "{queued}");
    let executor = test_box.clone_of(&host, "executor");
    let ran = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["decision"], "merged", "{ran}");
    let landing = ran["detail"]["ran"][0]["detail"]["commit"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(landing.len(), 64, "a SHA-256 id: {ran}");
    assert_eq!(test_box.git_bare(&host, &["rev-parse", "main"]), landing);
    assert_eq!(test_box.trunk_change_ids(&host), vec![id.clone()]);

    let status = test_box.bay_ok(&repo, &["status", &id]);
    let change = status["detail"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["change"] == id.as_str())
        .unwrap_or_else(|| panic!("{status}"));
    assert_eq!(change["state"], "merged", "{status}");
    assert_eq!(change["merged"], landing.as_str(), "{status}");

    let watched = test_box.bay_ok(&repo, &["status", "--since", &cursor, "--merged"]);
    let landings = watched["detail"]["merges"].as_array().unwrap();
    assert_eq!(landings.len(), 1, "{watched}");
    assert_eq!(landings[0]["change"], id.as_str(), "{watched}");
    assert_eq!(landings[0]["commit"], landing.as_str(), "{watched}");
    assert_eq!(landings[0]["base"], seed.as_str(), "{watched}");

    // The host hook still judges a push under `safe.bareRepository=explicit`.
    let out = test_box
        .git(&repo)
        .args(["push", "-q", "origin", &format!("+{seed}:refs/heads/main")])
        .output()
        .unwrap();
    assert!(!out.status.success(), "a rewrite of trunk was accepted");
    assert_eq!(test_box.git_bare(&host, &["rev-parse", "main"]), landing);
}

/// Outside any repository, `host init` takes Git's default object format, never a fixed one.
#[test]
fn host_init_outside_a_repository_takes_gits_default_object_format() {
    let test_box = git_3_box();
    let host = test_box.path().join("host.git");
    let made = host_init(&test_box, test_box.path(), &host);
    assert_eq!(made["detail"]["object format"], "sha256", "{made}");
    assert_eq!(
        test_box.git_bare(&host, &["rev-parse", "--show-object-format"]),
        "sha256"
    );
}

/// A repository on SHA-1 gets a SHA-1 host, so its pushes are accepted; `--object-format`
/// still decides when given.
#[test]
fn host_init_follows_the_repository_it_runs_in_unless_told() {
    let test_box = git_3_box();
    let repo = test_box.repo_without_remote("repo", "sha1", "reftable");
    let host = test_box.path().join("host.git");
    let made = host_init(&test_box, &repo, &host);
    assert_eq!(made["detail"]["object format"], "sha1", "{made}");
    test_box.add_origin(&repo, &host);
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);

    let told = test_box.path().join("told.git");
    let made = test_box.bay_ok(
        &repo,
        &[
            "host",
            "init",
            told.to_str().unwrap(),
            "--object-format",
            "sha256",
        ],
    );
    assert_eq!(made["detail"]["object format"], "sha256", "{made}");
}
