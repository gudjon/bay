//! G268. A branch made outside bay lands as one change.
//!
//! `bay start <intent> --paths <paths> --from <ref>` takes in a branch bay did not make (a
//! dependency bot's branch, a person's branch): a branch name or a full ref. When the remote has
//! that branch, its tip is fetched and taken, even over a stale local branch of the same name;
//! only a ref the remote lacks is read here (N-5). The change's view starts at its base, and its
//! first draft is the branch's net change since its merge base with trunk, merged onto the base
//! by the merge a candidate is built with. The record keeps `from: {ref, commit, source}`
//! (`source` is `remote` or `local`), which `bay start` and `bay status` show. `bay merge`
//! publishes it as one trunk commit with bay's trailers, so `bay status` lists no unattributed
//! landing. A branch that conflicts with trunk is `MERGE_CONFLICT`, and one that changes a path
//! `--paths` does not declare is the blocked `PATHS_UNDECLARED`, naming the uncovered paths; in
//! both cases nothing is created.
//!
//! The guards to mutate, in `src/start.rs`: the draft the new change starts from in `create`,
//! `let draft = taken_in.map_or(base, |taken_in| taken_in.draft.as_str());` (make it
//! `let draft = base;` and the first case fails: the view and the landing lack the branch's
//! edits); the `Merge::Conflict` arm of `take_in` (the second case); its
//! `if !uncovered.is_empty()` refusal (the third case); and in `resolve_from`, the
//! `if on_remote {` branch that fetches the remote's tip (make it `if false {` and the fourth
//! case fails: the stale local branch is taken in, with `source` `local`).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, result_json, unchecked_files};

/// A trunk with `src/x.rs` and no checks. Returns (box, repo, remote).
fn trunk() -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("src/x.rs", "one\ntwo\n")],
        &unchecked_files(),
    );
    (test_box, repo, remote)
}

/// Makes the branch `outside` by plain git, one commit per `(path, contents)` list, pushes it
/// to the remote and deletes it here, so bay must fetch it. Returns its tip.
fn outside_branch(test_box: &TestBox, repo: &Path, commits: &[&[(&str, &str)]]) -> String {
    test_box.git_ok(repo, &["switch", "-q", "-c", "outside"]);
    for (n, files) in commits.iter().enumerate() {
        for (path, text) in *files {
            let full = repo.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, text).unwrap();
        }
        test_box.git_ok(repo, &["add", "--all"]);
        test_box.git_ok(repo, &["commit", "-q", "-m", &format!("outside {n}")]);
    }
    let tip = test_box.git_ok(repo, &["rev-parse", "HEAD"]);
    test_box.git_ok(repo, &["push", "-q", "origin", "outside"]);
    test_box.git_ok(repo, &["switch", "-q", "main"]);
    test_box.git_ok(repo, &["branch", "-q", "-D", "outside"]);
    tip
}

/// Lands one change through bay that writes `path` in its view, so trunk moves past the point
/// the outside branch left it.
fn land_through_bay(test_box: &TestBox, repo: &Path, intent: &str, path: &str, text: &str) {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(path), text).unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
}

fn refs(test_box: &TestBox, remote: &Path, prefix: &str) -> Vec<String> {
    test_box
        .git_bare(
            remote,
            &["for-each-ref", "--format=%(objectname) %(refname)", prefix],
        )
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_two_commit_outside_branch_lands_as_one_attributed_trunk_commit() {
    let (test_box, repo, remote) = trunk();
    let tip = outside_branch(
        &test_box,
        &repo,
        &[&[("src/x.rs", "ONE\ntwo\n")], &[("src/x.rs", "ONE\nTWO\n")]],
    );
    // Trunk moves on through bay after the branch was made: the take-in is a real merge.
    land_through_bay(
        &test_box,
        &repo,
        "CARD-before",
        "src/new.rs",
        "fn new() {}\n",
    );
    let before = test_box.git_bare(&remote, &["rev-parse", "main"]);

    let started = test_box.bay_ok(
        &repo,
        &[
            "start",
            "CARD-outside",
            "--paths",
            "src/",
            "--from",
            "outside",
        ],
    );
    let id = started["change"].as_str().unwrap().to_owned();
    assert_eq!(started["detail"]["from"]["ref"], "outside", "{started}");
    assert_eq!(
        started["detail"]["from"]["commit"],
        tip.as_str(),
        "{started}"
    );
    assert_eq!(started["detail"]["from"]["source"], "remote", "{started}");
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    // The view holds the branch's net change on the base, beside what trunk landed since.
    assert_eq!(
        std::fs::read_to_string(view.join("src/x.rs")).unwrap(),
        "ONE\nTWO\n"
    );
    assert!(view.join("src/new.rs").exists(), "the view is of the base");

    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");

    // Exactly one new trunk commit, on the old tip, holding both edits; the branch's own
    // commits never reach trunk.
    assert_eq!(
        test_box.git_bare(
            &remote,
            &["rev-list", "--count", &format!("{before}..main")]
        ),
        "1"
    );
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", "main^"]),
        before,
        "one parent, the old tip"
    );
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:src/x.rs"]),
        "ONE\nTWO"
    );
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:src/new.rs"]),
        "fn new() {}"
    );
    assert_eq!(test_box.trunk_change_ids(&remote)[0], id);
    let merged = test_box
        .git(test_box.path())
        .arg(format!("--git-dir={}", remote.display()))
        .args(["merge-base", "--is-ancestor", &tip, "main"])
        .status()
        .unwrap();
    assert!(!merged.success(), "the outside commits are not on trunk");

    // `bay status` names where the change came from, and no landing is unattributed.
    let status = test_box.bay_ok(&repo, &["status"]);
    let unattributed = &status["detail"]["unattributed"];
    assert_eq!(unattributed["count"], 0, "{status}");
    let row = test_box.bay_ok(&repo, &["status", &id]);
    let from = &row["detail"]["changes"][0]["from"];
    assert_eq!(from["ref"], "outside", "{row}");
    assert_eq!(from["commit"], tip.as_str(), "{row}");
}

#[test]
fn an_outside_branch_that_conflicts_with_trunk_is_merge_conflict_and_creates_nothing() {
    let (test_box, repo, remote) = trunk();
    let tip = outside_branch(&test_box, &repo, &[&[("src/x.rs", "OUTSIDE\ntwo\n")]]);
    land_through_bay(&test_box, &repo, "CARD-trunk", "src/x.rs", "TRUNK\ntwo\n");
    let changes = refs(&test_box, &remote, "refs/bay/changes/");
    let claims = refs(&test_box, &remote, "refs/bay/intent/");

    let (code, refused) = result_json(test_box.bay(&repo).args([
        "start",
        "CARD-outside",
        "--paths",
        "src/",
        "--from",
        "outside",
    ]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "MERGE_CONFLICT", "{refused}");
    assert_eq!(
        refused["detail"]["conflicts"],
        serde_json::json!(["src/x.rs"]),
        "{refused}"
    );
    assert_eq!(
        refused["detail"]["from"]["commit"],
        tip.as_str(),
        "{refused}"
    );
    assert_eq!(refs(&test_box, &remote, "refs/bay/changes/"), changes);
    assert_eq!(refs(&test_box, &remote, "refs/bay/intent/"), claims);
}

#[test]
fn an_outside_branch_that_changes_an_undeclared_path_is_refused_and_creates_nothing() {
    let (test_box, repo, remote) = trunk();
    outside_branch(
        &test_box,
        &repo,
        &[&[("src/x.rs", "ONE\ntwo\n"), ("docs/notes.md", "notes\n")]],
    );
    let changes = refs(&test_box, &remote, "refs/bay/changes/");
    let claims = refs(&test_box, &remote, "refs/bay/intent/");

    let (code, refused) = result_json(test_box.bay(&repo).args([
        "start",
        "CARD-outside",
        "--paths",
        "src/",
        "--from",
        "outside",
    ]));
    // N-5: a blocked refusal with its own reason, never a plain failure.
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["decision"], "blocked", "{refused}");
    assert_eq!(refused["reason"], "PATHS_UNDECLARED", "{refused}");
    assert_eq!(refused["retry_when"], "paths_declared", "{refused}");
    assert_eq!(
        refused["allowed"],
        serde_json::json!(["read", "repair"]),
        "{refused}"
    );
    // Only the uncovered path is named, beside what was declared.
    assert_eq!(
        refused["detail"]["uncovered"],
        serde_json::json!(["docs/notes.md"]),
        "{refused}"
    );
    let declared = refused["detail"]["declared"].as_array().unwrap();
    assert_eq!(declared.len(), 1, "{refused}");
    assert!(
        declared[0].as_str().unwrap().starts_with("src"),
        "{refused}"
    );
    assert_eq!(
        refused["detail"]["recovery"], "start again with --paths covering them",
        "{refused}"
    );
    assert_eq!(refs(&test_box, &remote, "refs/bay/changes/"), changes);
    assert_eq!(refs(&test_box, &remote, "refs/bay/intent/"), claims);

    // Declared, the same branch is taken in, named by its full ref.
    let started = test_box.bay_ok(
        &repo,
        &[
            "start",
            "CARD-outside",
            "--paths",
            "src/",
            "docs/",
            "--from",
            "refs/heads/outside",
        ],
    );
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(view.join("docs/notes.md")).unwrap(),
        "notes\n"
    );
}

#[test]
fn the_remotes_tip_is_taken_over_a_stale_local_branch_of_the_same_name() {
    // N-5.
    let (test_box, repo, _remote) = trunk();
    let tip = outside_branch(
        &test_box,
        &repo,
        &[&[("src/x.rs", "ONE\ntwo\n")], &[("src/x.rs", "ONE\nTWO\n")]],
    );
    // This repository holds `outside` one commit behind the remote's.
    let stale = test_box.git_ok(&repo, &["rev-parse", &format!("{tip}^")]);
    test_box.git_ok(&repo, &["branch", "-q", "outside", &stale]);

    let started = test_box.bay_ok(
        &repo,
        &[
            "start",
            "CARD-outside",
            "--paths",
            "src/",
            "--from",
            "outside",
        ],
    );
    let from = &started["detail"]["from"];
    assert_eq!(from["ref"], "outside", "{started}");
    assert_eq!(from["commit"], tip.as_str(), "the remote's tip: {started}");
    assert_eq!(from["source"], "remote", "{started}");
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(view.join("src/x.rs")).unwrap(),
        "ONE\nTWO\n"
    );

    // A branch only this repository holds is read here, and says so.
    test_box.git_ok(&repo, &["branch", "-q", "only-here", &stale]);
    let started = test_box.bay_ok(
        &repo,
        &[
            "start",
            "CARD-only-here",
            "--paths",
            "src/",
            "--from",
            "only-here",
        ],
    );
    let from = &started["detail"]["from"];
    assert_eq!(from["commit"], stale.as_str(), "{started}");
    assert_eq!(from["source"], "local", "{started}");
}
