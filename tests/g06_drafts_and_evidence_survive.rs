//! G6. Drafts and evidence survive.
//!
//! A fresh clone can resume a change and read its evidence. `git gc --prune=now`
//! keeps every live draft. `bay share` pushes a change with its evidence to a remote, or
//! writes a bundle, and another actor opens the exact snapshot and its evidence from it.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

mod support;

use std::path::Path;

use bay::change::{Actor, Chain, ChangeRecord, ChangeState, Intent, RECORD_SCHEMA, freeze_draft};
use bay::git::Git;
use bay::ids::new_change_id;
use bay::inputs::DeclaredInput;
use support::TestBox;

fn record(id: &str, base: &str) -> ChangeRecord {
    ChangeRecord {
        schema: RECORD_SCHEMA.to_owned(),
        id: id.to_owned(),
        intent: Intent {
            reference: "CARD-1".to_owned(),
            revision: "1".to_owned(),
        },
        actor: Actor {
            name: "agent-a".to_owned(),
            session: "s1".to_owned(),
        },
        base_first: base.to_owned(),
        base: base.to_owned(),
        observed_remote: base.to_owned(),
        declared_inputs: vec![DeclaredInput::Path("src/".to_owned())],
        observed_inputs: Vec::new(),
        observed_versions: std::collections::BTreeMap::new(),
        validated_candidate: None,
        state: ChangeState::Open,
        superseded_by: None,
        claim: None,
        supersedes: None,
        reverts: None,
        adopted_from: Vec::new(),
        operations: Vec::new(),
        retention_cut: false,
        receipts: Vec::new(),
    }
}

fn edit(repo: &Path, name: &str, text: &str) {
    std::fs::write(repo.join(name), text).unwrap();
}

/// Three drafts, each referenced only by the chain, survive `reflog expire` + `gc --prune=now`.
fn drafts_survive_gc(object_format: &str, ref_format: &str) {
    let test_box = TestBox::new();
    let repo = test_box.repo("repo", object_format, ref_format);
    edit(&repo, "README", "trunk\n");
    test_box.git_ok(&repo, &["add", "README"]);
    test_box.git_ok(&repo, &["commit", "-q", "-m", "trunk"]);
    let base = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);

    let git = Git::new(&repo);
    let chain = Chain::new(&git);
    let id = new_change_id().unwrap();
    let mut record = record(&id, &base);

    edit(&repo, "a.txt", "draft one\n");
    let draft1 = freeze_draft(&git, &base, "draft 1").unwrap();
    let mut head = chain.create(&record, &draft1, "start").unwrap();

    edit(&repo, "a.txt", "draft two\n");
    let draft2 = freeze_draft(&git, &base, "draft 2").unwrap();
    head = chain.append(&head, &record, &draft2, "checkpoint").unwrap();

    edit(&repo, "b.txt", "draft three\n");
    let draft3 = freeze_draft(&git, &base, "draft 3").unwrap();
    record.state = ChangeState::Ready;
    chain.append(&head, &record, &draft3, "freeze").unwrap();

    // Nothing but the chain names the drafts: no branch, no reflog, no index entry.
    test_box.git_ok(&repo, &["reset", "-q", "--hard", &base]);
    test_box.git_ok(&repo, &["reflog", "expire", "--expire=now", "--all"]);
    test_box.git_ok(&repo, &["gc", "-q", "--prune=now"]);

    assert_eq!(
        chain.drafts(&id).unwrap(),
        vec![draft3.clone(), draft2.clone(), draft1.clone()]
    );
    for draft in [&draft1, &draft2, &draft3] {
        test_box.git_ok(&repo, &["cat-file", "-e", &format!("{draft}^{{commit}}")]);
        test_box.git_ok(&repo, &["cat-file", "-e", &format!("{draft}^{{tree}}")]);
    }
    assert_eq!(
        test_box.git_ok(&repo, &["show", &format!("{draft2}:a.txt")]),
        "draft two"
    );
    let (_, read_back) = chain.read(&id).unwrap();
    assert_eq!(read_back, record);
    test_box.git_ok(&repo, &["fsck", "--no-dangling", "--no-progress"]);
}

#[test]
fn gc_prune_now_keeps_every_live_draft_sha1_files() {
    drafts_survive_gc("sha1", "files");
}

#[test]
fn gc_prune_now_keeps_every_live_draft_sha256_reftable() {
    drafts_survive_gc("sha256", "reftable");
}

#[test]
fn a_stale_head_appends_nothing() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    edit(&repo, "README", "trunk\n");
    test_box.git_ok(&repo, &["add", "README"]);
    test_box.git_ok(&repo, &["commit", "-q", "-m", "trunk"]);
    let base = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);
    let git = Git::new(&repo);
    let chain = Chain::new(&git);
    let id = new_change_id().unwrap();
    let record = record(&id, &base);
    let first = chain.create(&record, &base, "start").unwrap();
    assert!(matches!(
        chain.create(&record, &base, "start again"),
        Err(bay::change::ChainError::Exists)
    ));
    let second = chain.append(&first, &record, &base, "one").unwrap();
    let stale = chain.append(&first, &record, &base, "two");
    assert!(
        matches!(stale, Err(bay::change::ChainError::Moved { .. })),
        "{stale:?}"
    );
    assert_eq!(chain.head(&id).unwrap().as_deref(), Some(second.as_str()));
}

/// Agent A starts a change, edits, checkpoints and records evidence on the draft's tree.
/// Returns the repository, the change id, and the draft tree.
fn a_change_with_evidence(test_box: &TestBox) -> (std::path::PathBuf, String, String) {
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n"), ("docs/b.md", "b\n")]);
    let started = test_box.bay_ok(&repo, &["start", "CARD-7", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = Path::new(started["detail"]["view"].as_str().unwrap()).to_path_buf();
    assert!(view.join("src/a.rs").is_file());
    assert!(!view.join("docs/b.md").exists(), "the view is sparse");

    edit(&view, "src/x.rs", "fn x() {}\n");
    let resumed = test_box.bay_ok(&repo, &["start", "--resume", &id]);
    assert_eq!(resumed["detail"]["checkpointed"], true);

    let git = Git::new(&repo);
    let draft = Chain::new(&git).drafts(&id).unwrap()[0].clone();
    let tree = test_box.git_ok(&repo, &["rev-parse", &format!("{draft}^{{tree}}")]);
    test_box.git_ok(
        &repo,
        &[
            "notes",
            "--ref=refs/notes/bay/evidence/ci",
            "add",
            "-m",
            "[{\"schema\":\"bay.evidence/1\",\"id\":\"ci/ev1\",\"verdict\":\"pass\"}]",
            &tree,
        ],
    );
    (repo, id, tree)
}

/// Another actor opens the exact snapshot and its evidence in `other`.
fn opens_exact_snapshot(test_box: &TestBox, other: &Path, id: &str, tree: &str) {
    let opened = test_box.bay_ok(other, &["start", "--resume", id]);
    let view = Path::new(opened["detail"]["view"].as_str().unwrap()).to_path_buf();
    assert_eq!(
        std::fs::read_to_string(view.join("src/x.rs")).unwrap(),
        "fn x() {}\n"
    );
    assert!(!view.join("docs/b.md").exists());
    let git = Git::new(other);
    let draft = Chain::new(&git).drafts(id).unwrap()[0].clone();
    assert_eq!(
        test_box.git_ok(other, &["rev-parse", &format!("{draft}^{{tree}}")]),
        tree
    );
    let note = test_box.git_ok(
        other,
        &["notes", "--ref=refs/notes/bay/evidence/ci", "show", tree],
    );
    assert!(note.contains("ci/ev1"), "{note}");
    test_box.git_ok(other, &["fsck", "--no-dangling", "--no-progress"]);
}

fn fresh_clone(test_box: &TestBox, from: &Path, name: &str) -> std::path::PathBuf {
    let dir = test_box.path().join(name);
    test_box.git_ok(
        test_box.path(),
        &["clone", "-q", from.to_str().unwrap(), dir.to_str().unwrap()],
    );
    dir
}

#[test]
fn share_writes_a_bundle_another_actor_opens() {
    let test_box = TestBox::new();
    let (repo, id, tree) = a_change_with_evidence(&test_box);
    let bundle = test_box.path().join("change.bundle");
    let shared = test_box.bay_ok(&repo, &["share", &id, "--bundle", bundle.to_str().unwrap()]);
    assert!(
        shared["detail"]["refs"]
            .to_string()
            .contains("refs/notes/bay/evidence/ci")
    );

    let other = fresh_clone(&test_box, &test_box.path().join("repo-remote.git"), "other");
    test_box.git_ok(
        &other,
        &[
            "fetch",
            "-q",
            bundle.to_str().unwrap(),
            "refs/bay/changes/*:refs/bay/changes/*",
            "refs/notes/bay/evidence/*:refs/notes/bay/evidence/*",
        ],
    );
    opens_exact_snapshot(&test_box, &other, &id, &tree);
}

#[test]
fn share_pushes_to_a_remote_another_actor_opens() {
    let test_box = TestBox::new();
    let (repo, id, tree) = a_change_with_evidence(&test_box);
    let peer = test_box.bare_any("peer.git");
    test_box.git_ok(&repo, &["push", "-q", peer.to_str().unwrap(), "main"]);
    test_box.git_ok(&repo, &["remote", "add", "peer", peer.to_str().unwrap()]);
    test_box.bay_ok(&repo, &["share", &id, "--remote", "peer"]);

    let other = fresh_clone(&test_box, &peer, "other");
    test_box.git_ok(
        &other,
        &[
            "fetch",
            "-q",
            "origin",
            "refs/notes/bay/evidence/*:refs/notes/bay/evidence/*",
        ],
    );
    opens_exact_snapshot(&test_box, &other, &id, &tree);
}

#[test]
fn a_fresh_clone_that_fetches_the_declared_refspecs_resumes_the_change() {
    let test_box = TestBox::new();
    let (repo, id, tree) = a_change_with_evidence(&test_box);
    test_box.git_ok(
        &repo,
        &[
            "push",
            "-q",
            "origin",
            "refs/notes/bay/evidence/ci:refs/notes/bay/evidence/ci",
        ],
    );
    let other = fresh_clone(&test_box, &test_box.path().join("repo-remote.git"), "other");
    test_box.git_ok(
        &other,
        &[
            "fetch",
            "-q",
            "origin",
            "refs/notes/bay/evidence/*:refs/notes/bay/evidence/*",
        ],
    );
    // The change ref itself comes from the remote: start pushed it, and resume fetches it.
    opens_exact_snapshot(&test_box, &other, &id, &tree);
}

#[test]
fn resume_brings_in_a_newer_trunk_and_reports_what_moved() {
    let test_box = TestBox::new();
    let (repo, id, _) = a_change_with_evidence(&test_box);
    let other = fresh_clone(&test_box, &test_box.path().join("repo-remote.git"), "other");
    edit(&other, "src/a.rs", "fn a() { 1 }\n");
    test_box.git_ok(&other, &["commit", "-q", "-am", "trunk moves"]);
    test_box.git_ok(&other, &["push", "-q", "origin", "main"]);

    let resumed = test_box.bay_ok(&repo, &["start", "--resume", &id]);
    assert_eq!(resumed["detail"]["trunk_moved"], true);
    assert_eq!(
        resumed["detail"]["moved_paths"],
        serde_json::json!(["src/a.rs"])
    );
    let view = Path::new(resumed["detail"]["view"].as_str().unwrap()).to_path_buf();
    assert_eq!(
        std::fs::read_to_string(view.join("src/a.rs")).unwrap(),
        "fn a() { 1 }\n"
    );
    assert_eq!(
        std::fs::read_to_string(view.join("src/x.rs")).unwrap(),
        "fn x() {}\n"
    );
}
