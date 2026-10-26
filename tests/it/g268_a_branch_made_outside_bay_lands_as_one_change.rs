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
//!
//! 0.5.3 (fleet findings, 2026-10-25): a local branch of the name that holds commits the
//! remote's lacks (a local rebase) is never dropped silently: the start fails, naming both
//! (guard: the `merge-base --is-ancestor` test in `resolve_from`). `bay start <CHANGE> --from
//! <ref>` replaces that change's draft with the branch's net change on the newest trunk, so a
//! rebase never needs a new branch name (guard: the `if let Some(reference) = from` block of
//! `resume`). With no `--paths`, the change declares exactly the paths the branch changes
//! (guard: the `derived` request in `open`). Main's commit keeps the branch's author and its
//! messages, and an amend in the view that changes only the message is a new draft (guards:
//! `src/publish.rs` `commit_candidate`'s author; `src/change.rs` `same_draft`'s message).

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
    assert_eq!(refused["retry_when"], "after_fix", "{refused}");
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

#[test]
fn a_local_branch_with_commits_the_remotes_lacks_is_named_never_dropped() {
    let (test_box, repo, _remote) = trunk();
    let tip = outside_branch(&test_box, &repo, &[&[("src/x.rs", "ONE\ntwo\n")]]);
    // Rebased here: the same edit, made again on trunk, so neither tip holds the other.
    test_box.git_ok(&repo, &["switch", "-q", "-c", "outside"]);
    std::fs::write(repo.join("src/x.rs"), "ONE\ntwo\nthree\n").unwrap();
    test_box.git_ok(&repo, &["commit", "-q", "-am", "rebased"]);
    let local = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);
    test_box.git_ok(&repo, &["switch", "-q", "main"]);

    let (code, failed) = result_json(test_box.bay(&repo).args([
        "start",
        "CARD-rebased",
        "--paths",
        "src/",
        "--from",
        "outside",
    ]));
    assert_eq!(code, 10, "{failed}");
    assert_eq!(failed["reason"], "BRANCH_DIVERGED", "{failed}");
    assert_eq!(failed["detail"]["remote"], tip.as_str(), "{failed}");
    assert_eq!(failed["detail"]["local"], local.as_str(), "{failed}");
}

#[test]
fn a_rebased_branch_replaces_its_changes_draft_under_the_same_change() {
    let (test_box, repo, remote) = trunk();
    outside_branch(&test_box, &repo, &[&[("src/x.rs", "ONE\ntwo\n")]]);
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
    let change = started["change"].as_str().unwrap().to_owned();
    // Trunk moves; the branch is rebased onto it here and never pushed under a new name.
    land_through_bay(&test_box, &repo, "CARD-other", "src/a.rs", "fn a2() {}\n");
    test_box.git_ok(&repo, &["fetch", "-q", "origin", "main"]);
    test_box.git_ok(&repo, &["switch", "-q", "-c", "rebased", "origin/main"]);
    std::fs::write(repo.join("src/x.rs"), "ONE\nTWO\n").unwrap();
    test_box.git_ok(&repo, &["commit", "-q", "-am", "rebased onto main"]);
    let rebased = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);
    test_box.git_ok(&repo, &["switch", "-q", "main"]);

    let resumed = test_box.bay_ok(&repo, &["start", &change, "--from", &rebased]);
    assert_eq!(resumed["change"], change.as_str(), "{resumed}");
    let view = PathBuf::from(resumed["detail"]["view"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(view.join("src/x.rs")).unwrap(),
        "ONE\nTWO\n"
    );
    assert_eq!(
        std::fs::read_to_string(view.join("src/a.rs")).unwrap(),
        "fn a2() {}\n"
    );
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:src/x.rs"]),
        "ONE\nTWO",
        "{landed}"
    );
    let heads = refs(&test_box, &remote, "refs/heads/");
    assert_eq!(heads.len(), 2, "no new branch name: {heads:?}");
}

#[test]
fn with_no_paths_an_outside_branch_declares_what_it_changes() {
    let (test_box, repo, _remote) = trunk();
    outside_branch(&test_box, &repo, &[&[("src/x.rs", "ONE\ntwo\n")]]);
    let started = test_box.bay_ok(&repo, &["start", "CARD-outside", "--from", "outside"]);
    let change = started["change"].as_str().unwrap();
    let record: serde_json::Value = serde_json::from_str(&test_box.git_ok(
        &repo,
        &["show", &format!("refs/bay/changes/{change}:change.json")],
    ))
    .unwrap();
    assert_eq!(
        record["declared_inputs"],
        serde_json::json!(["src/x.rs"]),
        "{record}"
    );
}

#[test]
fn main_keeps_the_branchs_author_and_words_and_an_amend_is_read() {
    let (test_box, repo, remote) = trunk();
    test_box.git_ok(&repo, &["switch", "-q", "-c", "outside"]);
    std::fs::write(repo.join("src/x.rs"), "ONE\ntwo\n").unwrap();
    test_box.git_ok(&repo, &["add", "--all"]);
    test_box.git_ok(
        &repo,
        &[
            "commit",
            "-q",
            "--author=Ada Writer <ada@example.org>",
            "-m",
            "Say one loudly\n\nBecause it matters.\n\nReviewed-by: Lin",
        ],
    );
    test_box.git_ok(&repo, &["push", "-q", "origin", "outside"]);
    test_box.git_ok(&repo, &["switch", "-q", "main"]);
    let started = test_box.bay_ok(&repo, &["start", "CARD-outside", "--from", "outside"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let author = test_box.git_bare(&remote, &["log", "-1", "--format=%an <%ae>", "main"]);
    assert_eq!(author, "Ada Writer <ada@example.org>");
    let message = test_box.git_bare(&remote, &["log", "-1", "--format=%B", "main"]);
    assert!(
        message.contains("Say one loudly") && message.contains("Because it matters."),
        "{message}"
    );
    assert!(message.contains("Reviewed-by: Lin"), "{message}");
    assert!(!message.contains("start --from"), "{message}");

    // A change whose agent amends only the message: main says the amended words.
    let started = test_box.bay_ok(&repo, &["start", "CARD-amend", "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/a.rs"), "fn a2() {}\n").unwrap();
    test_box.git_ok(&view, &["commit", "-q", "-am", "first words"]);
    let id = started["change"].as_str().unwrap();
    test_box.bay_ok(&repo, &["start", id]);
    test_box.git_ok(&view, &["commit", "-q", "--amend", "-m", "better words"]);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let message = test_box.git_bare(&remote, &["log", "-1", "--format=%B", "main"]);
    assert!(message.contains("better words"), "{message}");
}

/// `bay merge --from <branch>` (0.5.8): a branch made outside bay (a dependency bot's) is taken in
/// and merged in one step, under the branch's name as its intent and its own paths, with no
/// change id to carry from `bay start` to `bay merge`.
#[test]
fn merge_from_takes_a_branch_in_and_merges_it_in_one_step() {
    let (test_box, repo, remote) = trunk();
    outside_branch(&test_box, &repo, &[&[("src/x.rs", "ONE\ntwo\n")]]);
    let (code, merged) = result_json(
        test_box
            .bay(&repo)
            .args(["merge", "--from", "outside", "--wait"]),
    );
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:src/x.rs"]),
        "ONE\ntwo"
    );
    // Its intent is the branch, so the same branch merged again adopts that change.
    let id = merged["change"].as_str().unwrap();
    let record = test_box.git_bare(
        &remote,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    assert!(record.contains("\"outside\""), "{record}");
}

/// A fleet finding (2026-10-26): a refresh onto a moved main (`bay start <change>`) keeps the
/// draft's own message and author; it used to write "resume onto trunk" over them.
#[test]
fn a_refresh_onto_a_moved_main_keeps_the_drafts_message_and_author() {
    let (test_box, repo, _remote) = trunk();
    test_box.git_ok(&repo, &["switch", "-q", "-c", "outside"]);
    std::fs::write(repo.join("src/x.rs"), "ONE\ntwo\n").unwrap();
    test_box.git_ok(&repo, &["add", "--all"]);
    test_box.git_ok(
        &repo,
        &[
            "commit",
            "-q",
            "--author",
            "Ada Outside <ada@example.org>",
            "-m",
            "the outside work",
        ],
    );
    test_box.git_ok(&repo, &["push", "-q", "origin", "outside"]);
    test_box.git_ok(&repo, &["switch", "-q", "main"]);
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
    land_through_bay(
        &test_box,
        &repo,
        "CARD-before",
        "src/new.rs",
        "fn new() {}\n",
    );
    let (code, resumed) = result_json(test_box.bay(&repo).args(["start", &id]));
    assert_eq!(code, 0, "{resumed}");
    let view = PathBuf::from(resumed["detail"]["view"].as_str().unwrap());
    let head = test_box.git_ok(&view, &["log", "-1", "--format=%s|%an"]);
    assert_eq!(
        head, "the outside work|Ada Outside",
        "the refresh kept the draft's own message"
    );
}
