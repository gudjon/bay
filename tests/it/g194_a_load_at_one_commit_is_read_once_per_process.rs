//! G194. The registry, the policy and the config at a commit never change, so one bay process
//! reads each of them at one commit at most once, however many of its parts need it. The memo
//! is keyed by the commit's id, never by a ref, so a moved ref is read afresh. Held against a
//! `git` wrapper on `PATH` that logs every call's arguments and the process that made it (G171):
//! `bay status` on a repository with a registry, a policy, a config and an open change reads each
//! registry class file, the policy and the config at the trunk commit at most once in each of its
//! processes. A status that sweeps the remote runs its sweep as a process of its own (G619).

// Each case runs a shell script (a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::collections::BTreeMap;
use std::path::Path;

use support::{TestBox, unchecked_files};

/// The calls the wrapper logged, each as the pid of the process that made it, then its arguments.
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

/// How many `cat-file` calls name one of `objects`, by the process that made them.
fn reads(calls: &[Vec<String>], objects: &[&str]) -> BTreeMap<String, usize> {
    let mut by_process = BTreeMap::new();
    for call in calls {
        let (pid, args) = call.split_first().expect("a logged call names its process");
        if args.iter().any(|arg| arg == "cat-file")
            && args.iter().any(|arg| objects.contains(&arg.as_str()))
        {
            *by_process.entry(pid.clone()).or_insert(0) += 1;
        }
    }
    by_process
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
    let log_text = log.display();
    // One record is one write: a status and its sweep run Git side by side, and a record written
    // in two parts could be split by another process's.
    test_box.wrap_git(&format!(
        "record=$(printf '%s\\037' \"$PPID\" \"$@\"); printf '%s\\036' \"$record\" >> '{log_text}'"
    ));
    let status = test_box.bay_ok(&repo, &["status"]);
    assert_eq!(status["detail"]["main"], trunk.as_str(), "{status}");
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
        ["agent", "runner", "producer", "registry"].map(|class| {
            let path = format!("{registry}:{class}");
            (class, reads(&calls, &[path.as_str()]))
        }),
    );
    for (what, by_process) in counted {
        // At least once, or the count names nothing status reads; at most once in a process, the
        // promise.
        assert!(!by_process.is_empty(), "{what} never read:\n{calls:#?}");
        for (pid, count) in by_process {
            assert_eq!(
                count, 1,
                "{what} read {count} times at one commit by {pid}:\n{calls:#?}"
            );
        }
    }
}
