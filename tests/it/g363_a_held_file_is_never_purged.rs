//! G363 (BACKLOG 62, owner ruling 2026-10-24: "a hold that refuses a purge"). A held file is
//! never purged.
//!
//! `[assets] hold = [globs]` in main's policy holds the stored bytes of every file ever saved
//! under a held path. bay never purges; a store asks `bay assets held <id>` before it does:
//! - an object a manifest line names under a held path is held, naming the glob, the path and
//!   the commit; another path's object is not;
//! - a file deleted from main since stays held: any manifest line of main's history counts;
//! - a bare repository (a `bay host`, a gateway's cache) answers from its own main;
//! - a typo beside the hold in `[assets]` never lifts it.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/assets.rs` `held`: walk only main's newest manifest commit (`-1`) (the deleted file is
//!   not held).
//! - `src/assets.rs` `held`: never match a glob (no object is held).
//! - `src/policy.rs` `assets_at`: return the default table when it does not parse (the typo
//!   lifts the hold).

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, UNCHECKED_POLICY, result_json};

const BRIEF: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
const RENDER: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

/// A trunk whose policy holds `matters/acme/` (plus `extra` in `[assets]`), and whose manifest
/// names a brief under it and a render outside it. Returns the box, the clone and the remote.
fn trunk(extra: &str) -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let manifest = format!("{BRIEF} 5 matters/acme/brief.pdf\n{RENDER} 5 renders/cut.png\n");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), (".bay-assets", &manifest)],
        &[(
            ".bay/policy.toml".to_owned(),
            format!("{UNCHECKED_POLICY}\n[assets]\nhold = [\"matters/acme/\"]\n{extra}"),
        )],
    );
    let remote = test_box.path().join("repo-remote.git");
    (test_box, repo, remote)
}

fn held(test_box: &TestBox, dir: &Path, id: &str) -> Value {
    let (code, answer) = result_json(test_box.bay(dir).args(["assets", "held", id]));
    assert_eq!(code, 0, "{answer}");
    answer["detail"].clone()
}

#[test]
fn a_file_saved_under_a_held_path_is_held_and_another_is_not() {
    let (test_box, repo, _) = trunk("");
    let brief = held(&test_box, &repo, BRIEF);
    assert_eq!(brief["held"], true, "{brief}");
    assert_eq!(brief["hold"], "matters/acme/", "{brief}");
    assert_eq!(brief["path"], "matters/acme/brief.pdf", "{brief}");
    assert!(
        brief["commit"].as_str().is_some_and(|c| c.len() >= 40),
        "{brief}"
    );
    assert_eq!(held(&test_box, &repo, RENDER)["held"], false);
}

#[test]
fn a_file_deleted_since_stays_held() {
    let (test_box, repo, _) = trunk("");
    std::fs::write(
        repo.join(".bay-assets"),
        format!("{RENDER} 5 renders/cut.png\n"),
    )
    .unwrap();
    test_box.git_ok(&repo, &["commit", "-q", "-am", "drop the brief"]);
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);
    let brief = held(&test_box, &repo, BRIEF);
    assert_eq!(
        brief["held"], true,
        "deleting the file lifted the hold: {brief}"
    );
}

#[test]
fn a_bare_repository_answers_from_its_own_main() {
    let (test_box, _, remote) = trunk("");
    assert_eq!(held(&test_box, &remote, BRIEF)["held"], true);
    assert_eq!(held(&test_box, &remote, RENDER)["held"], false);
}

#[test]
fn a_typo_beside_the_hold_never_lifts_it() {
    let (test_box, repo, _) = trunk("kep = 3\n");
    let brief = held(&test_box, &repo, BRIEF);
    assert_eq!(brief["held"], true, "a typo lifted the hold: {brief}");
}

#[test]
fn an_identity_that_is_not_one_is_refused() {
    let (test_box, repo, _) = trunk("");
    let (code, answer) = result_json(test_box.bay(&repo).args(["assets", "held", "brief.pdf"]));
    assert_ne!(code, 0, "{answer}");
    assert_eq!(answer["decision"], "failed", "{answer}");
}
