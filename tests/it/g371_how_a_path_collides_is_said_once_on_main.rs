//! G371 (BACKLOG 65, owner ruling 2026-10-25: "approved as recommended"). How a path collides is
//! said once, on main.
//!
//! `.gitattributes` on main is the one home of a path's class: Git LFS's `lockable` attribute is
//! bay's lock list, and `bay-offload` sends a kind of file to the store whatever its size. Both
//! are read at main with `git check-attr --source`, so:
//! - a change that unlocks its own file in its `.gitattributes` still needs the file's lock;
//! - a change that marks its own file `bay-offload` keeps it in Git;
//! - `.gitattributes` is one of the policy's paths, so an edit of it outside bay is named, as a
//!   policy edit is (`policy_provenance: "outside_bay"`).
//!
//! `[locks]` in the policy and `offload` in `[plugin.assets]` are gone with no alias: each is
//! refused, naming where it went.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/locks.rs` `not_held`: read the attributes at the candidate's tree, not main's (the
//!   change unlocks its own file).
//! - `src/policy.rs` `Policy::paths`: leave out `.gitattributes` (the outside edit is not named).
//!
//! The offload case is a test of what holds by construction: the rule is handed only main's
//! commit (`assets::configured`, from both the save and the view), and mutations that read the
//! work tree or `HEAD` still read main's state there, so none breaks it.

// The store is a shell script, so this runs on Unix only.
#![cfg(unix)]

use crate::g340_a_large_file_saved_never_enters_git::{STORE_PLUGIN, store};
use crate::support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use support::{TestBox, UNCHECKED_POLICY, check_files, passing_result, result_json};

const LOCKABLE: &str = "art/** lockable\n";

#[test]
fn a_change_cannot_unlock_its_own_file() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("art/logo.bin", "logo 1\n"), ("src/a.rs", "fn a() {}\n")],
        &[
            (".bay/policy.toml".to_owned(), UNCHECKED_POLICY.to_owned()),
            (".gitattributes".to_owned(), LOCKABLE.to_owned()),
        ],
    );
    let started = test_box.bay_ok(
        &repo,
        &[
            "start",
            "an unlocked logo",
            "--paths",
            "art/",
            ".gitattributes",
        ],
    );
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(".gitattributes"), "").unwrap();
    std::fs::write(view.join("art/logo.bin"), "logo 2\n").unwrap();
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "a change unlocked its own file: {refused}");
    assert_eq!(refused["reason"], "LOCK_NOT_HELD", "{refused}");
}

#[test]
fn a_change_cannot_offload_its_own_bytes() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[
            (".bay/policy.toml".to_owned(), UNCHECKED_POLICY.to_owned()),
            (
                ".bay/config.toml".to_owned(),
                format!(
                    "[plugin.assets]\ncommand = \"scripts/assets {}\"\n",
                    store(&test_box).display()
                ),
            ),
            ("scripts/assets".to_owned(), STORE_PLUGIN.to_owned()),
        ],
    );
    std::fs::set_permissions(
        repo.join("scripts/assets"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    test_box.git_ok(&repo, &["add", "scripts/assets"]);
    test_box.git_ok(&repo, &["commit", "-q", "-m", "the store runs"]);
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);

    // The change says its own document is of a kind that leaves Git, in a commit of its own
    // first; main says nothing of it.
    let started = test_box.bay_ok(
        &repo,
        &["start", "a brief", "--paths", "brief.pdf", ".gitattributes"],
    );
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(".gitattributes"), "*.pdf bay-offload\n").unwrap();
    test_box.git_ok(&view, &["add", ".gitattributes"]);
    test_box.git_ok(&view, &["commit", "-q", "-m", "the brief's kind"]);
    std::fs::write(view.join("brief.pdf"), "a short brief\n").unwrap();
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:brief.pdf"]),
        "a short brief",
        "the change's own attribute offloaded its bytes"
    );
}

/// The `policy_provenance` of each evidence record on the remote main's tree.
fn provenance_at_main(test_box: &TestBox, repo: &Path, remote: &Path) -> Vec<String> {
    let tree = test_box.git_bare(remote, &["rev-parse", "main^{tree}"]);
    let notes = test_box.git_ok(
        repo,
        &[
            "notes",
            "--ref=refs/notes/bay/evidence/local",
            "show",
            &tree,
        ],
    );
    let records: Vec<serde_json::Value> = serde_json::from_str(&notes).unwrap();
    records
        .iter()
        .map(|record| {
            record["policy_provenance"]
                .as_str()
                .unwrap_or("absent")
                .to_owned()
        })
        .collect()
}

#[test]
fn an_edit_of_the_attributes_outside_bay_is_named() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((".gitattributes".to_owned(), LOCKABLE.to_owned()));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    // Pushed straight to main, around bay: the lock list is lifted.
    std::fs::write(repo.join(".gitattributes"), "").unwrap();
    test_box.git_ok(&repo, &["commit", "-q", "-am", "unlock the art"]);
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);

    let (_, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    test_box.git_ok(
        &repo,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/notes/bay/*:refs/notes/bay/*",
        ],
    );
    let provenance = provenance_at_main(&test_box, &repo, &remote);
    assert!(
        !provenance.is_empty() && provenance.iter().all(|p| p == "outside_bay"),
        "the outside edit of .gitattributes is not named: {provenance:?}"
    );
}

#[test]
fn the_removed_keys_name_where_they_went() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[(
            ".bay/policy.toml".to_owned(),
            format!("{UNCHECKED_POLICY}\n[locks]\npaths = [\"art/\"]\n"),
        )],
    );
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "POLICY_INVALID", "{refused}");
    assert!(
        refused.to_string().contains("lockable` in .gitattributes"),
        "{refused}"
    );

    let other = TestBox::new();
    let repo = other.repo_any("repo");
    other.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[
            (".bay/policy.toml".to_owned(), UNCHECKED_POLICY.to_owned()),
            (
                ".bay/config.toml".to_owned(),
                "[plugin.assets]\nover_limit = \"offload\"\noffload = [\"**/*.pdf\"]\n".to_owned(),
            ),
        ],
    );
    let (code, refused) = result_json(other.bay(&repo).arg("status"));
    assert_ne!(code, 0, "{refused}");
    assert!(
        refused
            .to_string()
            .contains("bay-offload` in .gitattributes"),
        "{refused}"
    );
}
