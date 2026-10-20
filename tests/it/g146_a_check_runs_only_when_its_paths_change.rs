//! G146. A check runs only when its paths change.
//!
//! A `[[check]]` with `when_paths` is required only when the candidate's own diff against its
//! base touches a path one of its globs matches: never the paths the change declared, since a
//! write outside those is recorded, not refused. The globs are judged in the policy at the base
//! and enter the definition digest, so a candidate cannot narrow them for itself (G26), and an
//! edit to them is a policy change checked twice. A check not required because no changed path
//! matched is recorded as `not_required` with the reason, in the land result and in the
//! landing's terminal record: never silently skipped.

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

const STUDIO: &str = "when_paths = [\"studio/**\", \"shared/**\"]\n";
const WORLD: &str = "when_paths = [\"world/\", \"shared/*.txt\"]\n";

/// One `[[check]]` entry on the shared adapter, with `extra` lines.
fn entry(id: &str, extra: &str) -> String {
    format!(
        "\n[[check]]\nid = \"{id}\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check {id}\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n{extra}"
    )
}

fn policy(revision: u32, world: &str) -> String {
    format!(
        "revision = {revision}\n{}{}",
        entry("studio", STUDIO),
        entry("world", world)
    )
}

/// A trunk with a studio app, a world app and shared code, whose `world` check fails any
/// candidate that holds `world/broken`.
fn trunk() -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let script = check_files(
        &format!("[ \"$1\" = world ] && [ -e world/broken ] && {{ {failing}; exit 0; }}"),
        &passing_result(1),
        1,
        60,
    )
    .into_iter()
    .find(|(path, _)| path == "scripts/bay-check")
    .unwrap()
    .1;
    test_box.seed_with_check(
        &repo,
        &[
            ("studio/app.txt", "studio\n"),
            ("world/map.txt", "world\n"),
            ("shared/lib.txt", "shared\n"),
        ],
        &[
            ("scripts/bay-check".to_owned(), script),
            (".bay/policy.toml".to_owned(), policy(1, WORLD)),
        ],
    );
    (test_box, repo, remote)
}

/// Starts `intent` on `paths` and returns its view.
fn start(test_box: &TestBox, repo: &Path, intent: &str, paths: &[&str]) -> PathBuf {
    let mut args = vec!["start", intent, "--paths"];
    args.extend(paths);
    let started = test_box.bay_ok(repo, &args);
    PathBuf::from(started["detail"]["view"].as_str().unwrap())
}

fn write(view: &Path, path: &str, text: &str) {
    let full = view.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, text).unwrap();
}

/// The records on `tree` in the remote: (check id, manifest digest), sorted.
fn records_on(test_box: &TestBox, remote: &Path, tree: &str) -> Vec<(String, String)> {
    let notes = test_box.git_bare(
        remote,
        &["notes", "--ref=refs/notes/bay/evidence/local", "show", tree],
    );
    let records: Vec<serde_json::Value> = serde_json::from_str(&notes).unwrap();
    let mut out: Vec<(String, String)> = records
        .iter()
        .map(|record| {
            (
                record["check"]["id"].as_str().unwrap().to_owned(),
                record["manifest_digest"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

fn ids(records: &[(String, String)]) -> Vec<&str> {
    let mut ids: Vec<&str> = records.iter().map(|(id, _)| id.as_str()).collect();
    ids.dedup();
    ids
}

fn landed_tree(test_box: &TestBox, remote: &Path, landed: &serde_json::Value) -> String {
    let commit = landed["detail"]["commit"].as_str().unwrap();
    test_box.git_bare(remote, &["rev-parse", &format!("{commit}^{{tree}}")])
}

#[test]
fn a_studio_change_skips_the_world_check_and_records_why() {
    let (test_box, repo, remote) = trunk();
    // The change declares both apps but changes only the studio: the diff, not the declaration,
    // says what is required.
    let view = start(&test_box, &repo, "STUDIO-1", &["studio/", "world/"]);
    write(&view, "studio/app.txt", "studio two\n");
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let tree = landed_tree(&test_box, &remote, &landed);
    assert_eq!(ids(&records_on(&test_box, &remote, &tree)), ["studio"]);
    // Never silently skipped: the land result names it, with the reason.
    let not_required = landed["detail"]["not_required"].as_array().unwrap();
    assert_eq!(not_required.len(), 1, "{landed}");
    assert_eq!(not_required[0]["check"], "world");
    assert_eq!(not_required[0]["environment"], "local");
    let reason = not_required[0]["reason"].as_str().unwrap();
    assert!(reason.contains("when_paths"), "{reason}");
    // And so does the landing's terminal record.
    let change = landed["change"].as_str().unwrap();
    let status = test_box.bay_ok(&repo, &["status", change]);
    let terminal = status["detail"]["changes"][0]["terminal"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["outcome"] == "published")
        .unwrap_or_else(|| panic!("no published terminal record: {status}"))
        .clone();
    assert_eq!(
        terminal["not_required"], landed["detail"]["not_required"],
        "{status}"
    );
}

#[test]
fn a_shared_change_requires_both_checks() {
    let (test_box, repo, remote) = trunk();
    let view = start(&test_box, &repo, "SHARED-1", &["shared/"]);
    write(&view, "shared/lib.txt", "shared two\n");
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let tree = landed_tree(&test_box, &remote, &landed);
    assert_eq!(
        ids(&records_on(&test_box, &remote, &tree)),
        ["studio", "world"]
    );
    assert!(landed["detail"].get("not_required").is_none(), "{landed}");
}

#[test]
fn a_write_outside_the_declared_paths_still_requires_its_check() {
    let (test_box, repo, remote) = trunk();
    let trunk_tip = test_box.git_bare(&remote, &["rev-parse", "main"]);
    // Declared the studio only, but the change breaks the world: the world check runs and
    // fails.
    let view = start(&test_box, &repo, "STUDIO-2", &["studio/"]);
    write(&view, "studio/app.txt", "studio two\n");
    write(&view, "world/broken", "oops\n");
    let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    assert_eq!(
        refused["detail"]["checks"][0]["check"], "world",
        "{refused}"
    );
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", "main"]),
        trunk_tip
    );
}

#[test]
fn a_candidate_cannot_narrow_when_paths_for_itself() {
    let (test_box, repo, remote) = trunk();
    let trunk_tip = test_box.git_bare(&remote, &["rev-parse", "main"]);
    // The candidate breaks the world and, in its own policy, points the world check elsewhere.
    // It is judged by the policy at its base, which requires the world check.
    let view = start(&test_box, &repo, "NARROW-1", &["world/", ".bay/"]);
    write(&view, "world/broken", "oops\n");
    write(
        &view,
        ".bay/policy.toml",
        &policy(2, "when_paths = [\"nowhere/\"]\n"),
    );
    let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    assert_eq!(
        refused["detail"]["checks"][0]["check"], "world",
        "{refused}"
    );
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", "main"]),
        trunk_tip
    );
}

#[test]
fn an_edit_to_when_paths_is_a_new_definition_checked_twice() {
    let (test_box, repo, remote) = trunk();
    // A policy change that only widens the world check's globs. A policy change runs every
    // check under both policies, and the globs are in the definition digest, so the studio
    // check's evidence under the base's manifest never stands for the candidate's own.
    let view = start(&test_box, &repo, "WIDEN-1", &[".bay/"]);
    write(
        &view,
        ".bay/policy.toml",
        &policy(
            2,
            "when_paths = [\"world/\", \"shared/*.txt\", \"docs/\"]\n",
        ),
    );
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let tree = landed_tree(&test_box, &remote, &landed);
    let records = records_on(&test_box, &remote, &tree);
    let studio: Vec<&String> = records
        .iter()
        .filter(|(id, _)| id == "studio")
        .map(|(_, digest)| digest)
        .collect();
    assert_eq!(studio.len(), 2, "checked under both manifests: {records:?}");
    assert_ne!(studio[0], studio[1], "{records:?}");
}

#[test]
fn a_bad_when_paths_list_makes_the_policy_invalid() {
    for bad in [
        "when_paths = []\n",
        "when_paths = [\"/etc/\"]\n",
        "when_paths = [\"../up/\"]\n",
        "when_paths = [\"\"]\n",
    ] {
        let test_box = TestBox::new();
        let repo = test_box.repo_any("repo");
        test_box.seed_with_check(
            &repo,
            &[("studio/app.txt", "studio\n")],
            &[
                (
                    "scripts/bay-check".to_owned(),
                    check_files("true", &passing_result(1), 1, 60)[0].1.clone(),
                ),
                (".bay/policy.toml".to_owned(), policy(1, bad)),
            ],
        );
        let view = start(&test_box, &repo, "BAD-1", &["studio/"]);
        write(&view, "studio/app.txt", "studio two\n");
        let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
        assert_eq!(code, 10, "{bad}: {refused}");
        assert_eq!(refused["reason"], "POLICY_INVALID", "{bad}: {refused}");
    }
}
