//! G140. A reviewer key signs only review records (ruled 2026-10-19, rulings-open-questions.md
//! Q12).
//!
//! The registry holds a fifth class file, `reviewer`. A record of kind `review` verifies only
//! against it; every other kind verifies only against the producer file. So a `check` pass
//! signed by a reviewer key never counts and is reported unverifiable, and a producer key's
//! `review` record is reported unverifiable too. The host hook refuses both, and a reviewer key
//! writes nothing but its own notes ref. A registry that puts one key in the reviewer and
//! another file, or one name in the reviewer and the producer file (they share the evidence-ref
//! namespace) or the agent file (Q7: an author never reviews itself under one name), is refused
//! where the registry is read.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use bay::evidence::{self, EvidenceRecord, Verdict};
use bay::git::Git;
use bay::registry::Registry;
use support::talk::fresh_key;
use support::{
    AGENT, EXECUTOR, PRODUCER, REGISTRAR, TestBox, TestKeys, check_files, key_line, passing_result,
    result_json,
};

const REVIEWER: &str = "review-1";

/// The registry files of the test registry, with `reviewers` as the whole reviewer file.
fn files(keys: &TestKeys, reviewers: &str) -> Vec<(&'static str, String)> {
    vec![
        ("agent", key_line(AGENT, &keys.agent)),
        ("executor", key_line(EXECUTOR, &keys.executor)),
        ("producer", key_line(PRODUCER, &keys.producer)),
        ("reviewer", reviewers.to_owned()),
        ("registry", key_line(REGISTRAR, &keys.registry)),
    ]
}

/// A box with a registry that enrols reviewer `review-1`, on a bay host (with its hook) when
/// `hosted`, else on a plain bare remote. Returns the box, the host or remote, the repo, the
/// keys and the reviewer's key.
fn reviewed(hosted: bool, collected: u32) -> (TestBox, PathBuf, PathBuf, TestKeys, PathBuf) {
    let test_box = TestBox::new();
    let check = check_files("true", &passing_result(collected), 1, 60);
    let (remote, repo) = if hosted {
        let host = test_box.path().join("host.git");
        test_box.host_init(&host);
        let repo = test_box.repo_without_remote_any("repo");
        test_box.add_origin(&repo, &host);
        (host, repo)
    } else {
        let repo = test_box.repo_any("repo");
        (test_box.path().join("repo-remote.git"), repo)
    };
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &check);
    let keys = test_box.keys();
    let reviewer = fresh_key(&test_box, "reviewer");
    test_box.enroll_files(&repo, &keys, &files(&keys, &key_line(REVIEWER, &reviewer)));
    test_box.use_keys(&repo, &keys);
    (test_box, remote, repo, keys, reviewer)
}

/// A record of `kind` under `name`, about `tree` and `commit`, signed with `key`.
fn record(
    name: &str,
    id: &str,
    kind: &str,
    (tree, commit): (&str, &str),
    key: &Path,
) -> EvidenceRecord {
    let mut parsed: EvidenceRecord = serde_json::from_value(serde_json::json!({
        "schema": "bay.evidence/1", "id": format!("{name}/{id}"), "kind": kind,
        "subject": {"tree": tree, "commit": commit, "base_tree": tree},
        "check": {"id": "tests", "environment": "local"}, "policy_revision": 1,
        "manifest_digest": "sha256:x", "producer": {"name": name, "version": "1"},
        "verdict": "pass", "at": "2026-10-19T00:00:00Z"}))
    .unwrap();
    parsed.sign(key).unwrap();
    parsed
}

/// How `bay status` names each record of change `id`, by record id.
fn verified(test_box: &TestBox, repo: &Path, id: &str, record: &str) -> serde_json::Value {
    let status = test_box.bay_ok(repo, &["status", id]);
    let evidence = &status["detail"]["changes"][0]["evidence"];
    evidence["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == record)
        .map_or(serde_json::Value::Null, |row| row["verified"].clone())
}

#[test]
fn a_check_pass_signed_by_a_reviewer_key_never_counts() {
    // The check collects nothing, so only another record could make it pass.
    let (test_box, _remote, repo, keys, reviewer) = reviewed(false, 0);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, blocked) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{blocked}");
    assert_eq!(blocked["reason"], "CHECK_UNKNOWN");
    let git = Git::new(&repo);
    let template = {
        let change = test_box.git_ok(
            &repo,
            &["show", &format!("refs/bay/changes/{id}:change.json")],
        );
        let change: serde_json::Value = serde_json::from_str(&change).unwrap();
        let tree = change["operations"][0]["attempts"][0]["tree"]
            .as_str()
            .unwrap()
            .to_owned();
        evidence::producer_records(&git, PRODUCER, &tree)
            .unwrap()
            .into_iter()
            .next()
            .expect("the producer signed the unknown record")
    };
    let pass = |name: &str, suffix: &str, key: &Path| {
        let mut forged = template.clone();
        forged.id = format!("{name}/{suffix}");
        forged.producer.name = name.to_owned();
        forged.verdict = Verdict::Pass;
        forged.why = None;
        forged.units = Vec::new();
        forged.sign(key).unwrap();
        forged
    };

    // A pass for the required check, signed by the reviewer key in the reviewer's own ref.
    evidence::append(&git, REVIEWER, &pass(REVIEWER, "forged", &reviewer)).unwrap();
    let (code, still) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{still}");
    assert_eq!(still["reason"], "CHECK_UNKNOWN", "{still}");
    assert_eq!(
        verified(&test_box, &repo, &id, &format!("{REVIEWER}/forged")),
        "unverifiable"
    );

    // Control: the same pass under the producer key counts.
    evidence::append(&git, PRODUCER, &pass(PRODUCER, "signed", &keys.producer)).unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged");
}

#[test]
fn a_review_record_verifies_only_under_the_reviewer_file() {
    let (test_box, _remote, repo, keys, reviewer) = reviewed(false, 1);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let commit = landed["detail"]["commit"].as_str().unwrap().to_owned();
    let tree = test_box.git_ok(&repo, &["rev-parse", &format!("{commit}^{{tree}}")]);
    let subject = (tree.as_str(), commit.as_str());
    let git = Git::new(&repo);

    let review = record(REVIEWER, "r1", "review", subject, &reviewer);
    evidence::append(&git, REVIEWER, &review).unwrap();
    let by_producer = record(PRODUCER, "r1", "review", subject, &keys.producer);
    evidence::append(&git, PRODUCER, &by_producer).unwrap();

    assert_eq!(
        verified(&test_box, &repo, &id, &format!("{REVIEWER}/r1")),
        "verified"
    );
    assert_eq!(
        verified(&test_box, &repo, &id, &format!("{PRODUCER}/r1")),
        "unverifiable"
    );

    // The evidence query judges a `review` record the same way: the reviewer's covers the
    // landing, the producer's is named unverifiable.
    let missing = |producer: &str| {
        let result = test_box.bay_ok(
            &repo,
            &[
                "evidence",
                "missing",
                "--kind",
                "review",
                "--producer",
                producer,
            ],
        );
        result["detail"]["missing"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["commit"] == commit.as_str())
            .map(|row| row["trust"].clone())
    };
    assert_eq!(missing(REVIEWER), None);
    assert_eq!(missing(PRODUCER), Some(serde_json::json!("unverifiable")));
}

#[test]
fn one_key_or_one_name_in_the_reviewer_file_and_another_is_refused() {
    let (test_box, _host, repo, keys, reviewer) = reviewed(true, 1);
    let git = Git::new(&repo);
    let second = fresh_key(&test_box, "reviewer-2");
    for (reviewers, named) in [
        (
            key_line(REVIEWER, &keys.producer),
            "already in producer under ci-1",
        ),
        (
            key_line(PRODUCER, &second),
            "reviewer ci-1 is also enrolled in producer",
        ),
        (
            key_line(AGENT, &second),
            "reviewer agent is also enrolled in agent",
        ),
    ] {
        let commit = test_box.registry_commit(&repo, &keys, &files(&keys, &reviewers));
        // Refused where the registry is read, with the reason named.
        let why = Registry::load_at(&git, &commit).unwrap_err();
        assert!(why.contains(named), "{why}");
        // And at the host's push.
        let out = test_box
            .git(&repo)
            .args(["push", "origin", &format!("{commit}:refs/bay/registry")])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "accepted: {stderr}");
        assert!(stderr.contains(named), "{stderr}");
    }

    // Control: a second reviewer under its own name and key is accepted.
    let both = format!(
        "{}{}",
        key_line(REVIEWER, &reviewer),
        key_line("review-2", &second)
    );
    let commit = test_box.registry_commit(&repo, &keys, &files(&keys, &both));
    Registry::load_at(&git, &commit).unwrap();
    test_box.git_ok(
        &repo,
        &[
            "push",
            "-q",
            "origin",
            &format!("{commit}:refs/bay/registry"),
        ],
    );
}

/// Pushes `notes` from `repo`, forced like a producer's own notes push; true when accepted.
fn push_notes(test_box: &TestBox, repo: &Path, notes: &str) -> (bool, String) {
    let out = test_box
        .git(repo)
        .args(["push", "origin", &format!("+{notes}:{notes}")])
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Resets `repo`'s `notes` to the host's, or deletes it when the host has none.
fn reset_notes(test_box: &TestBox, repo: &Path, notes: &str) {
    let remote = test_box.git_ok(repo, &["ls-remote", "origin", notes]);
    match remote.split_whitespace().next() {
        Some(_) => {
            test_box.git_ok(
                repo,
                &["fetch", "-q", "origin", &format!("+{notes}:{notes}")],
            );
        }
        None => {
            let _ = test_box
                .git(repo)
                .args(["update-ref", "-d", notes])
                .output();
        }
    }
}

/// A hosted [`reviewed`] box with one change landed: the change id, and trunk's commit and tree.
struct Landed {
    test_box: TestBox,
    repo: PathBuf,
    keys: TestKeys,
    reviewer: PathBuf,
    id: String,
    commit: String,
    tree: String,
}

fn landed_on_host() -> Landed {
    let (test_box, _host, repo, keys, reviewer) = reviewed(true, 1);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    test_box.git_ok(&repo, &["fetch", "-q", "origin"]);
    let commit = test_box.git_ok(&repo, &["rev-parse", "origin/main"]);
    let tree = test_box.git_ok(&repo, &["rev-parse", "origin/main^{tree}"]);
    Landed {
        test_box,
        repo,
        keys,
        reviewer,
        id,
        commit,
        tree,
    }
}

#[test]
fn the_hook_lets_a_reviewer_key_write_only_review_records_in_its_own_ref() {
    let Landed {
        test_box,
        repo,
        keys,
        reviewer,
        commit,
        tree,
        ..
    } = landed_on_host();
    let subject = (tree.as_str(), commit.as_str());
    let git = Git::new(&repo);
    let own = evidence::notes_ref(REVIEWER);
    let producers = evidence::notes_ref(PRODUCER);

    // A review record, reviewer-signed, in the reviewer's own ref: accepted.
    evidence::append(
        &git,
        REVIEWER,
        &record(REVIEWER, "r1", "review", subject, &reviewer),
    )
    .unwrap();
    let (ok, why) = push_notes(&test_box, &repo, &own);
    assert!(ok, "a reviewer's review record is accepted: {why}");

    // A check record, reviewer-signed, in the reviewer's own ref: refused.
    evidence::append(
        &git,
        REVIEWER,
        &record(REVIEWER, "c1", "check", subject, &reviewer),
    )
    .unwrap();
    let (ok, why) = push_notes(&test_box, &repo, &own);
    assert!(!ok, "a reviewer's check record is accepted: {why}");
    assert!(
        why.contains("of kind check is not signed by producer review-1's key"),
        "{why}"
    );
    reset_notes(&test_box, &repo, &own);

    // A review record, producer-signed, in the producer's ref: refused.
    reset_notes(&test_box, &repo, &producers);
    evidence::append(
        &git,
        PRODUCER,
        &record(PRODUCER, "r1", "review", subject, &keys.producer),
    )
    .unwrap();
    let (ok, why) = push_notes(&test_box, &repo, &producers);
    assert!(!ok, "a producer's review record is accepted: {why}");
    assert!(
        why.contains("of kind review is not signed by reviewer ci-1's key"),
        "{why}"
    );

    // The reviewer key writes another producer's ref: refused, whatever the kind.
    for kind in ["review", "check"] {
        reset_notes(&test_box, &repo, &producers);
        evidence::append(
            &git,
            PRODUCER,
            &record(
                PRODUCER,
                &format!("by-reviewer-{kind}"),
                kind,
                subject,
                &reviewer,
            ),
        )
        .unwrap();
        let (ok, why) = push_notes(&test_box, &repo, &producers);
        assert!(!ok, "the reviewer key wrote {producers}: {why}");
        assert!(why.contains("is not signed by"), "{why}");
    }
}

#[test]
fn a_reviewer_key_cannot_push_trunk_or_a_change_chain() {
    let Landed {
        test_box,
        repo,
        reviewer,
        id,
        commit,
        tree,
        ..
    } = landed_on_host();
    let signed = |tree: &str, parent: &str, message: &str| {
        let signing = format!("user.signingkey={}", reviewer.display());
        test_box.git_ok(
            &repo,
            &[
                "-c",
                "gpg.format=ssh",
                "-c",
                &signing,
                "commit-tree",
                "-S",
                tree,
                "-p",
                parent,
                "-m",
                message,
            ],
        )
    };
    let trunk = signed(&tree, &commit, "by a reviewer");
    let out = test_box
        .git(&repo)
        .args(["push", "origin", &format!("{trunk}:refs/heads/main")])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a reviewer-signed trunk: {stderr}");
    assert!(
        stderr.contains("trunk takes only commits signed by an executor key"),
        "{stderr}"
    );
    let chain_ref = format!("refs/bay/changes/{id}");
    test_box.git_ok(
        &repo,
        &[
            "fetch",
            "-q",
            "origin",
            &format!("+{chain_ref}:{chain_ref}"),
        ],
    );
    let chain_tip = test_box.git_ok(&repo, &["rev-parse", &chain_ref]);
    // A state commit: the chain tip's own tree, which carries the change record.
    let chain_tree = test_box.git_ok(&repo, &["rev-parse", &format!("{chain_tip}^{{tree}}")]);
    let chain = signed(&chain_tree, &chain_tip, "by a reviewer");
    let out = test_box
        .git(&repo)
        .args(["push", "origin", &format!("{chain}:{chain_ref}")])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a reviewer-signed change chain: {stderr}"
    );
    assert!(
        stderr.contains("is not signed by an agent, executor or registry key"),
        "{stderr}"
    );
}
