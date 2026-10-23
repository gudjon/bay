//! Runtime evidence kinds and completion profiles (`DESIGN` § 7 completion dimensions;
//! `INTERFACES` § 4 runtime adapter, § 6, § 7; ruling R12; `ROADMAP` § 2 increment 3).
//!
//! `running` and `effective` are read from runtime records on the landed commit's tree, and
//! only a producer-signed one counts: a record an agent or an executor signed never confirms a
//! dimension. `[profile.<name>] require` in the policy says which dimensions a change needs and
//! `[policy] profile` selects one; status names each missing dimension and never says "done".

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use bay::evidence::{self, EvidenceRecord};
use bay::git::Git;
use support::{PRODUCER, TestBox, TestKeys, check_files, passing_result, result_json};

const ARTIFACT: &str = "sha256:0d1ca7e5b0c4f1e2d3a4b5c6d7e8f90112233445566778899aabbccddeeff001";
const OTHER_ARTIFACT: &str =
    "sha256:1d1ca7e5b0c4f1e2d3a4b5c6d7e8f90112233445566778899aabbccddeeff001";

/// A signed box whose trunk carries `policy_extra` after its one check and `config` as its
/// `.bay/config.toml`, with one change landed. Returns the box, the repo, its keys, the change
/// id, the landed commit and the producer's check record on the landed tree.
fn landed(
    policy_extra: &str,
    config: &str,
) -> (TestBox, PathBuf, TestKeys, String, String, EvidenceRecord) {
    landed_as(policy_extra, config, true)
}

/// [`landed`], with a registry enrolled only when `enroll` is true.
fn landed_as(
    policy_extra: &str,
    config: &str,
    enroll: bool,
) -> (TestBox, PathBuf, TestKeys, String, String, EvidenceRecord) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(policy_extra);
        }
    }
    files.push((".bay/config.toml".to_owned(), config.to_owned()));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let keys = test_box.keys();
    if enroll {
        test_box.enroll(&repo, &keys);
        test_box.use_keys(&repo, &keys);
    }
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let commit = landed["detail"]["commit"].as_str().unwrap().to_owned();
    let tree = test_box.git_ok(&repo, &["rev-parse", &format!("{commit}^{{tree}}")]);
    // Without keys a machine records evidence as producer `local`.
    let producer = if enroll { PRODUCER } else { "local" };
    let records = evidence::producer_records(&Git::new(&repo), producer, &tree).unwrap();
    let template = records.into_iter().next().expect("the check's record");
    (test_box, repo, keys, id, commit, template)
}

/// A runtime record of `kind` about the landed commit, under producer `ci-1`, naming
/// `artifacts`, signed with `key`.
fn runtime_record(
    template: &EvidenceRecord,
    name: &str,
    kind: &str,
    artifacts: &[&str],
    key: &Path,
) -> EvidenceRecord {
    let mut record = template.clone();
    record.id = format!("{}/{name}", template.producer.name);
    kind.clone_into(&mut record.kind);
    record
        .unknown
        .insert("artifacts".to_owned(), serde_json::json!(artifacts));
    record.units = Vec::new();
    record.sandbox = None;
    record.seconds = None;
    record.sign(key).unwrap();
    record
}

fn completion(test_box: &TestBox, repo: &Path, id: &str) -> serde_json::Value {
    let status = test_box.bay_ok(repo, &["status", id]);
    let completion = status["detail"]["changes"][0]["completion"].clone();
    assert!(!status.to_string().contains("\"done\""), "{status}");
    completion
}

#[test]
fn a_producer_signed_running_record_confirms_running_and_an_agent_signed_one_does_not() {
    let (test_box, repo, keys, id, _, template) = landed("", "");
    let git = Git::new(&repo);
    let before = completion(&test_box, &repo, &id);
    assert_eq!(before["running"], "unconfirmed", "{before}");
    assert_eq!(before["effective"], "unconfirmed", "{before}");

    // Signed by keys of the other classes, under the producer's name and notes ref.
    for (name, key) in [("by-agent", &keys.agent), ("by-executor", &keys.executor)] {
        let forged = runtime_record(&template, name, "running", &[ARTIFACT], key);
        evidence::append(&git, PRODUCER, &forged).unwrap();
    }
    let forged = completion(&test_box, &repo, &id);
    assert_eq!(forged["running"], "unconfirmed", "{forged}");
    assert_eq!(
        forged["runtime"]["running"],
        serde_json::json!([]),
        "{forged}"
    );

    // An outcome check before anything is running confirms nothing.
    let outcome = runtime_record(
        &template,
        "outcome",
        "effective",
        &[ARTIFACT],
        &keys.producer,
    );
    evidence::append(&git, PRODUCER, &outcome).unwrap();
    let early = completion(&test_box, &repo, &id);
    assert_eq!(early["effective"], "unconfirmed", "{early}");

    // Something else is running: an outcome check on the artifact nobody runs is still not
    // confirmed.
    let other = runtime_record(
        &template,
        "other-deployed",
        "running",
        &[OTHER_ARTIFACT],
        &keys.producer,
    );
    evidence::append(&git, PRODUCER, &other).unwrap();
    let unrelated = completion(&test_box, &repo, &id);
    assert_eq!(unrelated["running"], "confirmed", "{unrelated}");
    assert_eq!(unrelated["effective"], "unconfirmed", "{unrelated}");

    let running = runtime_record(
        &template,
        "deployed",
        "running",
        &[ARTIFACT],
        &keys.producer,
    );
    evidence::append(&git, PRODUCER, &running).unwrap();
    let after = completion(&test_box, &repo, &id);
    assert_eq!(after["running"], "confirmed", "{after}");
    assert_eq!(
        after["runtime"]["running"],
        serde_json::json!([other.id, running.id]),
        "{after}"
    );
    assert_eq!(after["effective"], "confirmed", "{after}");
    assert_eq!(
        after["runtime"]["effective"],
        serde_json::json!([outcome.id])
    );
}

#[test]
fn a_running_record_without_an_artifact_or_for_another_commit_confirms_nothing() {
    let (test_box, repo, keys, id, _, template) = landed("", "");
    let git = Git::new(&repo);
    let bare = runtime_record(&template, "no-artifact", "running", &[], &keys.producer);
    evidence::append(&git, PRODUCER, &bare).unwrap();
    let mut elsewhere = template.clone();
    elsewhere.subject.commit = "0".repeat(template.subject.commit.len());
    let elsewhere = runtime_record(
        &elsewhere,
        "elsewhere",
        "running",
        &[ARTIFACT],
        &keys.producer,
    );
    evidence::append(&git, PRODUCER, &elsewhere).unwrap();
    // A name is not a digest: "x" says nothing about what was built.
    for (name, artifacts) in [
        ("not-a-digest", &["x"][..]),
        ("short", &["sha256:0d1c"][..]),
        ("one-bad", &[ARTIFACT, "x"][..]),
    ] {
        let named = runtime_record(&template, name, "running", artifacts, &keys.producer);
        evidence::append(&git, PRODUCER, &named).unwrap();
    }
    let out = completion(&test_box, &repo, &id);
    assert_eq!(out["running"], "unconfirmed", "{out}");
}

/// Without a registry no key can sign as a producer, so no runtime record counts, however it
/// is signed, and the runtime adapter is not asked: nothing it prints could count.
#[test]
fn without_a_registry_no_runtime_record_counts_and_the_adapter_is_not_asked() {
    let config = format!("{SELECT_DEPLOY}[plugin.runtime]\ncommand = \"scripts/bay-runtime\"\n");
    let (test_box, repo, keys, id, _, template) = landed_as(DEPLOY_PROFILE, &config, false);
    let marker = test_box.path().join("runtime-asked");
    support::write_executable(
        &repo.join("scripts/bay-runtime"),
        &format!("#!/bin/sh\ntouch '{}'\necho '[]'\n", marker.display()),
    );
    let git = Git::new(&repo);
    let mut unsigned = runtime_record(&template, "signed", "running", &[ARTIFACT], &keys.producer);
    evidence::append(&git, "local", &unsigned).unwrap();
    unsigned.signature = None;
    unsigned.id = "local/bare".to_owned();
    evidence::append(&git, "local", &unsigned).unwrap();
    let out = completion(&test_box, &repo, &id);
    assert_eq!(out["running"], "unconfirmed", "{out}");
    assert_eq!(out["profile"]["verdict"], "incomplete", "{out}");
    assert_eq!(out["runtime"]["adapter"], "not asked", "{out}");
    assert!(!marker.exists(), "the adapter ran without a registry");
}

/// The adapter is asked only for what a verdict needs: a profile that requires a runtime
/// dimension, for one named change. The default profile and a listing of every change never
/// run it, so `bay status` stays bounded however many changes have landed.
#[test]
fn the_runtime_adapter_is_asked_only_for_a_named_change_whose_profile_needs_it() {
    let marker_script = |test_box: &TestBox, repo: &Path| {
        let marker = test_box.path().join("runtime-asked");
        support::write_executable(
            &repo.join("scripts/bay-runtime"),
            &format!("#!/bin/sh\ntouch '{}'\necho '[]'\n", marker.display()),
        );
        marker
    };
    let plain = "[plugin.runtime]\ncommand = \"scripts/bay-runtime\"\n";
    let (test_box, repo, _, id, _, _) = landed("", plain);
    let marker = marker_script(&test_box, &repo);
    let out = completion(&test_box, &repo, &id);
    assert_eq!(out["runtime"]["adapter"], "not asked", "{out}");
    assert!(!marker.exists(), "the default profile needs no runtime");

    let config = format!("{SELECT_DEPLOY}{plain}");
    let (test_box, repo, _, id, _, _) = landed(DEPLOY_PROFILE, &config);
    let marker = marker_script(&test_box, &repo);
    let all = test_box.bay_ok(&repo, &["status"]);
    let listed = &all["detail"]["changes"][0]["completion"];
    assert_eq!(all["detail"]["changes"][0]["change"], id.as_str(), "{all}");
    assert_eq!(listed["runtime"]["adapter"], "not asked", "{all}");
    assert_eq!(listed["profile"]["verdict"], "incomplete", "{all}");
    assert!(
        !marker.exists(),
        "a listing of every change ran the adapter"
    );
    let named = completion(&test_box, &repo, &id);
    assert_eq!(named["runtime"]["adapter"], "ran", "{named}");
    assert!(marker.exists());
}

/// Ruling R12: a profile is part of the policy, so a change to it is a policy change, but it
/// says nothing about what a check computes: `manifest_digest` stays the digest of the sorted
/// manifest (`INTERFACES` § 7), and a check's evidence is never made stale by a profile.
#[test]
fn a_profile_never_changes_the_manifest_digest() {
    let (_, _, _, _, _, plain) = landed("", "");
    let (_, _, _, _, _, with_profile) = landed(DEPLOY_PROFILE, SELECT_DEPLOY);
    assert_eq!(with_profile.manifest_digest, plain.manifest_digest);
}

/// The profile is selected, as it is defined, at the landing's accepted base: a work tree
/// whose `.bay/config.toml` selects a weaker profile never relaxes a landed change's verdict.
#[test]
fn the_selected_profile_is_read_at_the_accepted_base() {
    let (test_box, repo, _, id, _, _) = landed(DEPLOY_PROFILE, SELECT_DEPLOY);
    std::fs::write(
        repo.join(".bay/config.toml"),
        "[policy]\nprofile = \"default\"\n",
    )
    .unwrap();
    let out = completion(&test_box, &repo, &id);
    assert_eq!(out["profile"]["name"], "deploy", "{out}");
    assert_eq!(out["profile"]["verdict"], "incomplete", "{out}");
}

const DEPLOY_PROFILE: &str =
    "\n[profile.deploy]\nrequire = [\"merged\", \"checked\", \"running\"]\n";
const SELECT_DEPLOY: &str = "[policy]\nprofile = \"deploy\"\n";

#[test]
fn a_profile_that_requires_running_reports_the_gap_until_a_producer_confirms_it() {
    let (test_box, repo, keys, id, _, template) = landed(DEPLOY_PROFILE, SELECT_DEPLOY);
    let gap = completion(&test_box, &repo, &id);
    let profile = &gap["profile"];
    assert_eq!(profile["name"], "deploy", "{gap}");
    assert_eq!(profile["verdict"], "incomplete", "{gap}");
    assert_eq!(profile["missing"], serde_json::json!(["running"]), "{gap}");
    assert_eq!(
        profile["summary"], "merged and checked; runtime confirmation missing",
        "{gap}"
    );

    let git = Git::new(&repo);
    let forged = runtime_record(&template, "by-agent", "running", &[ARTIFACT], &keys.agent);
    evidence::append(&git, PRODUCER, &forged).unwrap();
    assert_eq!(
        completion(&test_box, &repo, &id)["profile"]["verdict"],
        "incomplete"
    );

    let running = runtime_record(
        &template,
        "deployed",
        "running",
        &[ARTIFACT],
        &keys.producer,
    );
    evidence::append(&git, PRODUCER, &running).unwrap();
    let met = completion(&test_box, &repo, &id);
    assert_eq!(met["profile"]["verdict"], "complete", "{met}");
    assert_eq!(met["profile"]["missing"], serde_json::json!([]), "{met}");
    assert_eq!(met["profile"]["summary"], "merged, checked and running");
}

#[test]
fn the_default_profile_keeps_published_and_checked() {
    let (test_box, repo, _, id, _, _) = landed("", "");
    let out = completion(&test_box, &repo, &id);
    assert_eq!(out["merged"], true, "{out}");
    assert_eq!(out["checked"], true, "{out}");
    assert_eq!(out["running"], "unconfirmed", "{out}");
    assert_eq!(out["profile"]["name"], "default", "{out}");
    assert_eq!(out["profile"]["verdict"], "complete", "{out}");
    assert_eq!(out["profile"]["summary"], "merged and checked", "{out}");
}

#[test]
fn a_selected_profile_the_policy_does_not_declare_is_unknown() {
    let (test_box, repo, _, id, _, _) = landed("", SELECT_DEPLOY);
    let out = completion(&test_box, &repo, &id);
    assert_eq!(out["profile"]["verdict"], "unknown", "{out}");
}

#[test]
fn a_profile_with_an_unknown_dimension_makes_the_policy_invalid() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str("\n[profile.deploy]\nrequire = [\"merged\", \"deployed\"]\n");
        }
    }
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, blocked) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_ne!(code, 0, "{blocked}");
    assert_eq!(blocked["reason"], "POLICY_INVALID", "{blocked}");
}

/// The runtime adapter is asked about the landed commit; what it prints that a producer signed
/// counts for that read, and the rest is refused, each with why. `bay status` is a read: it
/// writes nothing to a producer's notes ref, which only that producer writes.
#[test]
fn the_runtime_adapter_counts_only_what_a_producer_signed_and_writes_nothing() {
    let config = format!("{SELECT_DEPLOY}[plugin.runtime]\ncommand = \"scripts/bay-runtime\"\n");
    let (test_box, repo, keys, id, commit, template) = landed(DEPLOY_PROFILE, &config);
    let answer = test_box.path().join("runtime-answer.json");
    let request = test_box.path().join("runtime-request.json");
    support::write_executable(
        &repo.join("scripts/bay-runtime"),
        &format!(
            "#!/bin/sh\ncat > '{}'\ncat '{}'\n",
            request.display(),
            answer.display()
        ),
    );
    let forged = runtime_record(&template, "by-agent", "running", &[ARTIFACT], &keys.agent);
    std::fs::write(&answer, serde_json::to_string(&[&forged]).unwrap()).unwrap();
    let gap = completion(&test_box, &repo, &id);
    assert_eq!(gap["runtime"]["adapter"], "ran", "{gap}");
    assert_eq!(gap["running"], "unconfirmed", "{gap}");
    assert_eq!(gap["runtime"]["refused"][0]["id"], forged.id, "{gap}");
    let asked: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&request).unwrap()).unwrap();
    assert_eq!(asked["commit"], commit.as_str(), "{asked}");
    assert_eq!(asked["change"], id.as_str(), "{asked}");

    let running = runtime_record(
        &template,
        "deployed",
        "running",
        &[ARTIFACT],
        &keys.producer,
    );
    // Producer-signed, but about another tree than the one that landed.
    let mut moved = template.clone();
    moved.subject.tree = "0".repeat(template.subject.tree.len());
    let moved = runtime_record(&moved, "moved", "running", &[ARTIFACT], &keys.producer);
    std::fs::write(
        &answer,
        serde_json::to_string(&[&forged, &moved, &running]).unwrap(),
    )
    .unwrap();
    let tree = test_box.git_ok(&repo, &["rev-parse", &format!("{commit}^{{tree}}")]);
    let notes = || {
        test_box.git_ok(
            &repo,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/notes/",
            ],
        )
    };
    let before = notes();
    let met = completion(&test_box, &repo, &id);
    assert_eq!(met["running"], "confirmed", "{met}");
    assert_eq!(
        met["runtime"]["running"],
        serde_json::json!([running.id]),
        "{met}"
    );
    assert_eq!(met["profile"]["verdict"], "complete", "{met}");
    let refused: Vec<&str> = met["runtime"]["refused"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|refusal| refusal["id"].as_str())
        .collect();
    assert_eq!(refused, [forged.id.as_str(), moved.id.as_str()], "{met}");
    assert_eq!(notes(), before, "status wrote to a notes ref");
    let held = evidence::producer_records(&Git::new(&repo), PRODUCER, &tree).unwrap();
    assert!(!held.iter().any(|record| record.id == running.id));

    // A failing adapter confirms nothing and is named, never read as a pass.
    support::write_executable(&repo.join("scripts/bay-runtime"), "#!/bin/sh\nexit 3\n");
    let failed = completion(&test_box, &repo, &id);
    assert_eq!(failed["runtime"]["adapter"], "failed", "{failed}");
    assert!(failed["runtime"]["error"].is_string(), "{failed}");
    assert_eq!(failed["running"], "unconfirmed", "{failed}");
    assert_eq!(failed["profile"]["verdict"], "incomplete", "{failed}");

    // An adapter that prints more records than one read judges is refused whole.
    let many = vec![&running; 65];
    std::fs::write(&answer, serde_json::to_string(&many).unwrap()).unwrap();
    support::write_executable(
        &repo.join("scripts/bay-runtime"),
        &format!("#!/bin/sh\ncat > /dev/null\ncat '{}'\n", answer.display()),
    );
    let flood = completion(&test_box, &repo, &id);
    assert_eq!(flood["runtime"]["adapter"], "failed", "{flood}");
    assert!(
        flood["runtime"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("at most 64")),
        "{flood}"
    );
    assert_eq!(flood["running"], "unconfirmed", "{flood}");
}
