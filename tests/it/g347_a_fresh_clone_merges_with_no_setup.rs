//! G347 (owner, 2026-10-24: "with git, we only connect git once in the repo to the origin"): a
//! fresh clone merges with no setup.
//!
//! Git needs one fact, the remote, and `git clone` sets it; everything else is derived. bay keeps
//! that: a plain `git clone` of a bay repository saves and merges with no `bay install` and no
//! `bay.*` git config. main is the remote's default branch, so a repository that serves `dev`
//! makes `dev` the remote's default, and every fresh clone follows. A clone made before the
//! default moved still holds the old one; bay asks the remote for its default once, when the
//! committed config names another main, so that clone needs no step either.
//!
//! Guard to mutate: `src/repo.rs` `open_with`: drop the `refresh_default` retry (the older
//! clone is refused, told to run `bay install --main dev`).

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, UNCHECKED_POLICY, result_json};

/// A repository on the remote whose files and `.bay/config.toml` are `config`, with no check.
fn seeded(test_box: &TestBox, config: &str) -> (PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[
            (".bay/policy.toml".to_owned(), UNCHECKED_POLICY.to_owned()),
            (".bay/config.toml".to_owned(), config.to_owned()),
        ],
    );
    (repo, test_box.path().join("repo-remote.git"))
}

/// The `bay.*` keys of `clone`'s own git config.
fn bay_keys(test_box: &TestBox, clone: &Path) -> String {
    let out = test_box
        .git(clone)
        .args(["config", "--local", "--get-regexp", "^bay\\."])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// `bay save` of a new file in `clone`; asserts it merged.
fn saves(test_box: &TestBox, clone: &Path, file: &str) {
    std::fs::write(clone.join(file), "new\n").unwrap();
    let (code, saved) = result_json(test_box.bay(clone).args(["save", "a new file"]));
    assert_eq!(code, 0, "{saved}");
    assert_eq!(saved["decision"], "merged", "{saved}");
}

#[test]
fn a_plain_git_clone_saves_and_merges() {
    let test_box = TestBox::new();
    let (_repo, remote) = seeded(&test_box, "");
    let clone = test_box.clone_of(&remote, "fresh");
    assert_eq!(bay_keys(&test_box, &clone), "", "the clone was set up");
    saves(&test_box, &clone, "b.txt");
    assert_eq!(test_box.git_bare(&remote, &["show", "main:b.txt"]), "new");
}

#[test]
fn a_served_branch_that_is_the_remotes_default_needs_no_flag() {
    let test_box = TestBox::new();
    let (repo, remote) = seeded(&test_box, "main = \"dev\"\n");
    test_box.git_ok(&repo, &["push", "-q", "origin", "main:dev"]);
    // A clone made while the remote's default was still main.
    let older = test_box.clone_of(&remote, "older");
    test_box.git_bare(&remote, &["symbolic-ref", "HEAD", "refs/heads/dev"]);

    let fresh = test_box.clone_of(&remote, "fresh");
    assert_eq!(bay_keys(&test_box, &fresh), "", "the clone was set up");
    saves(&test_box, &fresh, "b.txt");
    assert_eq!(test_box.git_bare(&remote, &["show", "dev:b.txt"]), "new");

    // The older clone asks the remote for its default once, then merges onto dev too.
    test_box.git_ok(&older, &["checkout", "-q", "-B", "dev", "origin/dev"]);
    test_box.git_ok(&older, &["pull", "-q", "--ff-only", "origin", "dev"]);
    saves(&test_box, &older, "c.txt");
    assert_eq!(test_box.git_bare(&remote, &["show", "dev:c.txt"]), "new");
}
