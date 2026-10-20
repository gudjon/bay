//! G194. The registry, the policy and the config at a commit never change, so one bay process
//! reads each of them at one commit at most once, however many of its parts need it. The memo
//! is keyed by the commit's id, never by a ref, so a moved ref is read afresh. Held against a
//! `git` wrapper on `PATH` that logs every call's arguments (G171): `bay status` on a repository
//! with a registry, a policy, a config and an open change reads each registry class file, the
//! policy and the config at the trunk commit at most once.

// Each case runs a shell script (a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;

use support::{TestBox, unchecked_files};

/// The arguments of every call the wrapper logged.
fn calls(log: &Path) -> Vec<Vec<String>> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    text.split('\x1e')
        .filter(|call| !call.is_empty())
        .map(|call| {
            call.strip_suffix('\x1f')
                .unwrap_or(call)
                .split('\x1f')
                .map(str::to_owned)
                .collect()
        })
        .collect()
}

/// How many `cat-file` calls name one of `objects`.
fn reads(calls: &[Vec<String>], objects: &[&str]) -> usize {
    calls
        .iter()
        .filter(|args| {
            args.iter().any(|arg| arg == "cat-file")
                && args.iter().any(|arg| objects.contains(&arg.as_str()))
        })
        .count()
}

#[test]
fn a_status_reads_the_registry_policy_and_config_at_trunk_once() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let trunk = test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let keys = test_box.keys();
    let registry = test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    // An open change: status judges its profile and dimensions at trunk too.
    let (id, _view) = test_box.start_and_edit(&repo);
    let config_blob = test_box.git_ok(&repo, &["rev-parse", &format!("{trunk}:.bay/config.toml")]);

    let log = test_box.path().join("git-calls.log");
    test_box.log_git_calls(&log);
    let status = test_box.bay_ok(&repo, &["status"]);
    assert_eq!(status["detail"]["trunk"], trunk.as_str(), "{status}");
    let listed = status["detail"]["changes"].as_array().unwrap();
    assert!(
        listed.iter().any(|change| change["change"] == id.as_str()),
        "{status}"
    );

    let calls = calls(&log);
    let policy = format!("{trunk}:.bay/policy.toml");
    let config = format!("{trunk}:.bay/config.toml");
    let counted = [
        ("policy", reads(&calls, &[policy.as_str()])),
        (
            "config",
            reads(&calls, &[config.as_str(), config_blob.as_str()]),
        ),
    ]
    .into_iter()
    .chain(
        // The class files the test registry enrols; it has no `reviewer` file, so none is read.
        ["agent", "executor", "producer", "registry"].map(|class| {
            let path = format!("{registry}:{class}");
            (class, reads(&calls, &[path.as_str()]))
        }),
    );
    for (what, count) in counted {
        // At least once, or the count names nothing status reads; at most once, the promise.
        assert_eq!(
            count, 1,
            "{what} read {count} times at one commit:\n{calls:#?}"
        );
    }
}
