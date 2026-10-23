//! A landing needs a policy at its base (`DESIGN` § 7: unknown is never pass).
//!
//! With no `.bay/policy.toml` at the accepted base the criteria are unknown, so land and the
//! executor's publish refuse with `POLICY_MISSING`, naming the base commit and the path, and
//! the host hook of a bay-hosted trunk refuses such a publication. A repository that wants no
//! checks says so at the base, where a change cannot edit it: `unchecked = true` and no
//! `[[check]]`. Such a landing says "unchecked by policy" in its result and in its
//! executor-signed terminal record, and status never shows it as checked. `unchecked = true`
//! beside a `[[check]]` is `POLICY_INVALID`, and a candidate that adds `unchecked = true` for
//! itself is judged by the policy at its base. Under an unchecked base a candidate lands
//! unchecked by that base: a policy or adapter it adds takes effect only after it lands, never
//! for its own landing, unless it is a registry-signed repair.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::json;
use support::{
    EXECUTOR, TestBox, TestKeys, UNCHECKED_POLICY, check_files, passing_result, result_json,
    unchecked_files,
};

const POLICY: &str = ".bay/policy.toml";

/// A local repository whose trunk holds `src/a.rs` and `files`; returns (box, repo, remote,
/// trunk).
fn seeded(files: &[(String, String)]) -> (TestBox, PathBuf, PathBuf, String) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let trunk = test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], files);
    (test_box, repo, remote, trunk)
}

fn assert_policy_missing(result: &serde_json::Value, base: &str) {
    assert_eq!(result["decision"], "blocked", "{result}");
    assert_eq!(result["reason"], "POLICY_MISSING", "{result}");
    assert_eq!(result["allowed"], json!(["read"]), "{result}");
    assert_eq!(result["retry_when"], "policy_added", "{result}");
    assert_eq!(result["detail"]["base"], base, "{result}");
    assert_eq!(result["detail"]["path"], POLICY, "{result}");
}

#[test]
fn land_without_a_policy_at_the_base_is_policy_missing() {
    let (test_box, repo, remote, trunk) = seeded(&[]);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, result) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{result}");
    assert_policy_missing(&result, &trunk);
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), trunk);
}

#[test]
fn the_executors_publish_without_a_policy_at_the_base_is_policy_missing() {
    let (test_box, repo, remote, trunk) = seeded(&[]);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let executor = test_box.clone_of(&remote, "executor");
    let ran = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    // The executor's summary of the run carries the reason and its detail; the operation in
    // the chain carries what is allowed.
    let one = &ran["detail"]["ran"][0];
    assert_eq!(one["decision"], "blocked", "{ran}");
    assert_eq!(one["reason"], "POLICY_MISSING", "{ran}");
    assert_eq!(one["detail"]["base"], trunk.as_str(), "{ran}");
    assert_eq!(one["detail"]["path"], POLICY, "{ran}");
    assert_eq!(test_box.trunk_change_ids(&remote), Vec::<String>::new());
    let status = test_box.bay_ok(&executor, &["status", &id]);
    let op = &status["detail"]["changes"][0]["operation"];
    assert_eq!(op["outcome"], "POLICY_MISSING", "{status}");
    assert_eq!(op["allowed"], json!(["read"]), "{status}");
    assert_eq!(op["retry_when"], "policy_added", "{status}");
}

/// Judged at the base: a candidate that brings its own `unchecked = true` to a base without
/// a policy declares nothing for itself.
#[test]
fn a_candidate_cannot_declare_itself_unchecked_at_a_base_without_a_policy() {
    let (test_box, repo, remote, trunk) = seeded(&[]);
    let (_, view) = test_box.start_and_edit(&repo);
    std::fs::create_dir_all(view.join(".bay")).unwrap();
    std::fs::write(view.join(POLICY), UNCHECKED_POLICY).unwrap();
    let (code, result) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{result}");
    assert_policy_missing(&result, &trunk);
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), trunk);
}

/// Judged at the base: a candidate that turns a checked base unchecked is still checked by
/// the base's checks, and a failing one refuses it.
#[test]
fn a_candidate_cannot_declare_itself_unchecked_under_a_checked_base() {
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let (test_box, repo, remote, trunk) = seeded(&check_files("true", &failing, 1, 60));
    let (_, view) = test_box.start_and_edit(&repo);
    std::fs::create_dir_all(view.join(".bay")).unwrap();
    std::fs::write(view.join(POLICY), "revision = 2\nunchecked = true\n").unwrap();
    let (code, result) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "CHECK_FAILED", "{result}");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), trunk);
}

#[test]
fn unchecked_beside_a_check_is_policy_invalid() {
    let mut files = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == POLICY {
            *text = text.replacen("revision = 1\n", "revision = 1\nunchecked = true\n", 1);
        }
    }
    let (test_box, repo, remote, trunk) = seeded(&files);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, result) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "POLICY_INVALID", "{result}");
    assert!(result.to_string().contains("unchecked"), "{result}");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), trunk);
}

/// The landing, its terminal record and status all say "unchecked by policy"; status never
/// reports the change as checked, and no evidence is written for it.
#[test]
fn an_unchecked_policy_lands_and_status_says_unchecked() {
    let (test_box, repo, remote, _) = seeded(&unchecked_files());
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert_eq!(
        landed["detail"]["unchecked"], "unchecked by policy",
        "{landed}"
    );
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id.clone()]);

    let status = test_box.bay_ok(&repo, &["status", &id]);
    let change = &status["detail"]["changes"][0];
    assert_eq!(change["state"], "merged", "{status}");
    assert_eq!(change["completion"]["merged"], true, "{status}");
    assert_eq!(change["completion"]["checked"], false, "{status}");
    assert_eq!(
        change["completion"]["unchecked"], "unchecked by policy",
        "{status}"
    );
    assert_ne!(
        change["completion"]["profile"]["summary"], "merged and checked",
        "{status}"
    );
    let terminal = change["terminal"].as_array().unwrap();
    assert!(
        terminal
            .iter()
            .any(|record| record["outcome"] == "merged"
                && record["unchecked"] == "unchecked by policy"),
        "{status}"
    );
    let notes = test_box.git_bare(&remote, &["for-each-ref", "refs/notes/bay/evidence/"]);
    assert_eq!(notes, "", "an unchecked landing writes no evidence");
}

/// A bay host whose trunk holds `files`, seeded before its registry; returns (box, host, repo,
/// keys).
fn hosted(files: &[(String, String)]) -> (TestBox, PathBuf, PathBuf, TestKeys) {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], files);
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    (test_box, host, repo, keys)
}

fn signed_commit(
    test_box: &TestBox,
    repo: &Path,
    key: &Path,
    tree: &str,
    parent: &str,
    message: &str,
) -> String {
    let signing = format!("user.signingkey={}", key.display());
    let user = format!("user.name={EXECUTOR}");
    let out = test_box
        .git(repo)
        .args([
            "-c",
            "gpg.format=ssh",
            "-c",
            signing.as_str(),
            "-c",
            user.as_str(),
            "-c",
            "user.email=x@bay.invalid",
            "commit-tree",
            "-S",
            tree,
            "-p",
            parent,
            "-m",
            message,
        ])
        .env("GIT_AUTHOR_NAME", EXECUTOR)
        .env("GIT_COMMITTER_NAME", EXECUTOR)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// The host hook applies the same rule: an executor-signed publication of a verified, current
/// request is refused when its base has no policy.
#[test]
fn the_host_hook_refuses_a_publication_on_a_base_without_a_policy() {
    let (test_box, host, repo, keys) = hosted(&[]);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, result) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{result}");
    let trunk = test_box.git_bare(&host, &["rev-parse", "main"]);
    assert_policy_missing(&result, &trunk);

    // The agent-signed request is on the host; a publication of it, executor-signed, with its
    // landed marker in the same push, is everything the hook asks for but the policy.
    let request = test_box.git_bare(&host, &["rev-parse", &format!("refs/bay/requests/{id}")]);
    let request_json: serde_json::Value = serde_json::from_str(&test_box.git_bare(
        &host,
        &["cat-file", "blob", &format!("{request}:request.json")],
    ))
    .unwrap();
    let revision = request_json["revision"].as_str().unwrap().to_owned();
    let registry = test_box.git_ok(&repo, &["rev-parse", "refs/bay/registry"]);
    test_box.git_ok(
        &repo,
        &["fetch", "-q", "origin", &format!("refs/bay/requests/{id}")],
    );
    let tree = test_box.git_ok(&repo, &["rev-parse", &format!("{revision}^{{tree}}")]);
    let publication = signed_commit(
        &test_box,
        &repo,
        &keys.executor,
        &tree,
        &trunk,
        &format!(
            "publish\n\nChange-Id: {id}\nRevision-Id: {revision}\nRequest-Id: {request}\nRegistry-Id: {registry}\n"
        ),
    );
    let out = test_box
        .git(&repo)
        .args([
            "push",
            "--atomic",
            "origin",
            &format!("{publication}:refs/heads/main"),
            &format!("{publication}:refs/bay/landed/{id}"),
        ])
        .output()
        .unwrap();
    let why = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "the publication was accepted: {why}");
    assert!(why.contains("POLICY_MISSING"), "{why}");
    assert_eq!(test_box.git_bare(&host, &["rev-parse", "main"]), trunk);
}

/// On a bay-hosted trunk an unchecked base lands, executor-signed, with no evidence.
#[test]
fn an_unchecked_policy_lands_on_a_bay_hosted_trunk() {
    let (test_box, host, repo, keys) = hosted(&unchecked_files());
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(
        landed["detail"]["unchecked"], "unchecked by policy",
        "{landed}"
    );
    assert_eq!(test_box.trunk_change_ids(&host), vec![id]);
    let tip = test_box.git_bare(&host, &["rev-parse", "main"]);
    assert!(test_box.executor_signed(&host, &keys, &tip));
}

/// Starts a change claiming `paths` and returns (id, view).
fn start_on(test_box: &TestBox, repo: &Path, intent: &str, paths: &[&str]) -> (String, PathBuf) {
    let mut args = vec!["start", intent, "--paths"];
    args.extend(paths);
    let started = test_box.bay_ok(repo, &args);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    (id, view)
}

/// Writes `files` into `view`; an adapter under `scripts/` is executable.
fn write_files(view: &Path, files: &[(String, String)]) {
    use std::os::unix::fs::PermissionsExt;
    for (path, text) in files {
        let at = view.join(path);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(&at, text).unwrap();
        if path.starts_with("scripts/") {
            std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
}

/// `files` with the policy's revision raised to `revision`.
fn at_revision(mut files: Vec<(String, String)>, revision: u32) -> Vec<(String, String)> {
    for (path, text) in &mut files {
        if path == POLICY {
            *text = text.replacen("revision = 1\n", &format!("revision = {revision}\n"), 1);
        }
    }
    files
}

/// Judged at the base: under an unchecked base, a candidate that adds its own checked policy
/// and adapter lands unchecked by the base's policy. Its policy takes effect only after it
/// lands, never for its own landing, so a check it wrote never makes its landing "checked".
#[test]
fn a_candidate_that_adds_checks_under_an_unchecked_base_lands_unchecked() {
    let (test_box, repo, remote, _) = seeded(&unchecked_files());
    let (id, view) = start_on(&test_box, &repo, "CARD-ADD", &["src/", ".bay/", "scripts/"]);
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    write_files(
        &view,
        &at_revision(check_files("true", &passing_result(1), 1, 60), 2),
    );
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert_eq!(
        landed["detail"]["unchecked"], "unchecked by policy",
        "{landed}"
    );
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let change = &status["detail"]["changes"][0];
    assert_eq!(change["completion"]["checked"], false, "{status}");
    assert_eq!(
        change["completion"]["unchecked"], "unchecked by policy",
        "{status}"
    );
    let notes = test_box.git_bare(&remote, &["for-each-ref", "refs/notes/bay/evidence/"]);
    assert_eq!(
        notes, "",
        "the candidate's own check never ran for its landing"
    );

    // After it lands, its policy is the base: the next change is checked by it. Its edit is its
    // own: one already on trunk would end `no_effect` (G213).
    let (next, view) = test_box.start_and_edit(&repo);
    std::fs::write(view.join("src/new.rs"), "fn next() {}\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert!(landed["detail"].get("unchecked").is_none(), "{landed}");
    let status = test_box.bay_ok(&repo, &["status", &next]);
    assert_eq!(
        status["detail"]["changes"][0]["completion"]["checked"], true,
        "{status}"
    );
}

/// Checked twice (§ 7): a candidate that turns a checked base unchecked is still checked by
/// the base's checks, and when they pass it lands as checked.
#[test]
fn a_candidate_that_turns_a_checked_base_unchecked_lands_checked_by_the_base() {
    let (test_box, repo, _remote, _) = seeded(&check_files("true", &passing_result(1), 1, 60));
    let (id, view) = test_box.start_and_edit(&repo);
    std::fs::create_dir_all(view.join(".bay")).unwrap();
    std::fs::write(view.join(POLICY), "revision = 2\nunchecked = true\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert!(landed["detail"].get("unchecked").is_none(), "{landed}");
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let change = &status["detail"]["changes"][0];
    assert_eq!(change["completion"]["checked"], true, "{status}");
    assert_eq!(change["completion"]["unchecked"], json!(null), "{status}");
}

/// The existing repair path: a registry-signed repair may give a base without a policy its
/// first policy, judged by that new policy alone, and is checked by it. An agent-signed one
/// is `POLICY_MISSING`.
#[test]
fn a_registry_signed_repair_adds_the_first_policy_to_a_base_without_one() {
    let (test_box, repo, remote, trunk) = seeded(&[]);
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    let repair = |key: &Path, intent: &str| {
        let (id, view) = start_on(&test_box, &repo, intent, &[".bay/", "scripts/"]);
        write_files(&view, &check_files("true", &passing_result(1), 1, 60));
        test_box.git_ok(&repo, &["config", "bay.agentKey", key.to_str().unwrap()]);
        let landed = result_json(test_box.bay(&view).args(["merge", "--wait", &id]));
        (id, landed)
    };
    let (_, (code, refused)) = repair(&keys.agent, "REPAIR-1");
    assert_eq!(code, 10, "{refused}");
    assert_policy_missing(&refused, &trunk);

    let (id, (code, repaired)) = repair(&keys.registry, "REPAIR-2");
    assert_eq!(code, 0, "{repaired}");
    assert_eq!(repaired["decision"], "merged", "{repaired}");
    assert!(repaired["detail"].get("unchecked").is_none(), "{repaired}");
    assert!(
        test_box
            .git_bare(&remote, &["show", &format!("main:{POLICY}")])
            .contains("[[check]]")
    );
    let status = test_box.bay_ok(&repo, &["status", &id]);
    assert_eq!(
        status["detail"]["changes"][0]["completion"]["checked"], true,
        "{status}"
    );
}
