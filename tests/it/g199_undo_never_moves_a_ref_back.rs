//! G199. Undo never moves a ref back.
//!
//! `bay undo` is a compensating change (ruling R10): trunk only moves forward, by a revert
//! commit on top of the trunk it finds, and no ref bay touches goes to an ancestor of the value
//! it held. Here trunk moves on the remote while the undo's candidate waits just before its
//! publishing push: the push is never forced, so the moved trunk is kept, the undo lands on top
//! of it, the undone change's record is unchanged, and every remote ref is where it was or
//! moved on, never back. A local chain of the undone change that is ahead of the remote's is
//! kept: the undo reads the remote's chain beside it, never over it.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use support::{TestBox, check_files, passing_result, result_json};

/// Every ref of the repository the git option `git_dir` names: name → object.
fn all_refs(test_box: &TestBox, git_dir: &str) -> BTreeMap<String, String> {
    test_box
        .git_ok(
            test_box.path(),
            &[git_dir, "for-each-ref", "--format=%(refname) %(objectname)"],
        )
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(name, oid)| (name.to_owned(), oid.to_owned()))
        .collect()
}

/// Whether `ancestor` is an ancestor of (or is) `descendant`; false for an object that is no
/// commit.
fn is_ancestor(test_box: &TestBox, git_dir: &str, ancestor: &str, descendant: &str) -> bool {
    test_box
        .git(test_box.path())
        .args([git_dir, "merge-base", "--is-ancestor", ancestor, descendant])
        .output()
        .unwrap()
        .status
        .success()
}

#[test]
fn an_undo_racing_a_trunk_move_lands_on_top_and_rewinds_nothing() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    // This store's copy of the undone chain is one state commit ahead of the remote's, as
    // work not yet pushed is: the undo reads the remote's and keeps this one.
    let git = bay::git::Git::new(&repo);
    let chain = bay::change::Chain::new(&git);
    let (head, record) = chain.read(&id).unwrap();
    let draft = chain.drafts(&id).unwrap()[0].clone();
    let ahead = chain.append(&head, &record, &draft, "kept here").unwrap();

    let remote_dir = format!("--git-dir={}", remote.display());
    let local_dir = format!("--git-dir={}", repo.join(".git").display());
    let remote_before = all_refs(&test_box, &remote_dir);
    let local_before = all_refs(&test_box, &local_dir);
    let undone = format!("refs/bay/changes/{id}");
    let trunk_before = remote_before["refs/heads/main"].clone();

    // The undo holds its checked candidate just before the publishing push.
    let release = test_box.path().join("release-undo");
    let undo = test_box
        .bay_process(&repo)
        .env("BAY_TEST_PAUSE_AT", "before-publish")
        .env("BAY_TEST_PAUSE_FILE", &release)
        .args(["--json", "undo", &id, "--wait"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let paused = test_box.path().join("release-undo.paused");
    let deadline = Instant::now() + Duration::from_secs(120);
    while !paused.exists() {
        assert!(
            Instant::now() < deadline,
            "the undo never reached its publishing push"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Meanwhile trunk moves forward on the remote.
    let other = test_box.clone_of(&remote, "other");
    std::fs::write(other.join("src/b.rs"), "fn b() {}\n").unwrap();
    test_box.git_ok(&other, &["add", "--all"]);
    test_box.git_ok(&other, &["commit", "-q", "-m", "elsewhere"]);
    test_box.git_ok(&other, &["push", "-q", "origin", "main"]);
    let moved = test_box.git_ok(&other, &["rev-parse", "HEAD"]);
    std::fs::write(&release, "").unwrap();

    let out = undo.wait_with_output().unwrap();
    let result: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "the undo printed no result: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    assert_eq!(out.status.code(), Some(0), "{result}");
    assert_eq!(result["decision"], "merged", "{result}");
    let undo_id = result["change"].as_str().unwrap();

    trunk_moved_forward_only(&test_box, &remote, &trunk_before, &moved, undo_id);

    // The undone change's record is never rewound: on the remote it stays, or moves on to the
    // state this store had not pushed yet (bay's sync pushes local work the remote lacks).
    let remote_after = all_refs(&test_box, &remote_dir);
    let local_after = all_refs(&test_box, &local_dir);
    let remote_chain = &remote_after[&undone];
    assert!(
        *remote_chain == remote_before[&undone] || *remote_chain == ahead,
        "the remote chain of the undone change went to {remote_chain}"
    );
    assert_eq!(
        local_after.get(&undone),
        Some(&ahead),
        "the local chain was rewound"
    );
    // Nor did this store's chains, claims or trunk copy go back.
    for (name, before) in local_before.iter().filter(|(name, _)| {
        ["refs/bay/changes/", "refs/bay/intent/", "refs/remotes/"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
    }) {
        let after = local_after
            .get(name)
            .unwrap_or_else(|| panic!("{name} was deleted"));
        assert!(
            is_ancestor(&test_box, &local_dir, before, after),
            "{name} moved from {before} to {after}, not forward"
        );
    }

    no_remote_ref_went_back(&test_box, &remote_dir, &remote_before, &remote_after);
}

/// No remote ref went back: each one bay moved went forward, or to an unrelated value (a
/// heartbeat), never to an ancestor of what it held; claims and markers only gained new refs.
fn no_remote_ref_went_back(
    test_box: &TestBox,
    remote_dir: &str,
    remote_before: &BTreeMap<String, String>,
    remote_after: &BTreeMap<String, String>,
) {
    // No remote ref went back: each one bay moved went forward, or to an unrelated value
    // (a heartbeat), never to an ancestor of what it held.
    for (name, before) in remote_before {
        let Some(after) = remote_after.get(name) else {
            panic!("{name} was deleted by the undo");
        };
        if after == before {
            continue;
        }
        assert!(
            !is_ancestor(test_box, remote_dir, after, before),
            "{name} moved back from {before} to {after}"
        );
        if name.starts_with("refs/heads/") || name.starts_with("refs/bay/changes/") {
            assert!(
                is_ancestor(test_box, remote_dir, before, after),
                "{name} moved from {before} to {after}, not forward"
            );
        }
    }
    // Claims and markers are create-only: the undo only adds its own.
    for prefix in [
        "refs/bay/intent/",
        "refs/bay/landed/",
        "refs/bay/landed-intent/",
    ] {
        for (name, before) in remote_before
            .iter()
            .filter(|(name, _)| name.starts_with(prefix))
        {
            assert_eq!(remote_after.get(name), Some(before), "{name} moved");
        }
    }
}

/// Trunk moved forward only: the undo is a revert on top of the moved trunk, the move is kept
/// and the undone change's effect is gone.
fn trunk_moved_forward_only(
    test_box: &TestBox,
    remote: &std::path::Path,
    trunk_before: &str,
    moved: &str,
    undo_id: &str,
) {
    let remote_dir = format!("--git-dir={}", remote.display());
    let trunk_after = test_box.git_bare(remote, &["rev-parse", "main"]);
    assert!(is_ancestor(
        test_box,
        &remote_dir,
        trunk_before,
        &trunk_after
    ));
    assert!(is_ancestor(test_box, &remote_dir, moved, &trunk_after));
    assert_eq!(
        test_box.git_bare(remote, &["rev-parse", "main^1"]),
        moved,
        "the undo is not a commit on top of the trunk it found"
    );
    assert_eq!(test_box.trunk_change_ids(remote)[0], undo_id);
    assert!(
        !test_box
            .git_bare(
                remote,
                &["ls-tree", "--name-only", "main", "--", "src/b.rs"]
            )
            .is_empty(),
        "the trunk move was lost"
    );
    assert!(
        test_box
            .git_bare(
                remote,
                &["ls-tree", "--name-only", "main", "--", "src/new.rs"]
            )
            .is_empty(),
        "the undone change's effect is still on trunk"
    );
}
