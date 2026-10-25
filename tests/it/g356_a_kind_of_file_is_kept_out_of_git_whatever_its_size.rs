//! G356 (owner ruling 2026-10-24, BACKLOG 62): a kind of file is kept out of Git whatever its
//! size.
//!
//! Some documents must be erasable on request, and a blob in Git cannot be erased without
//! rewriting history that bay's signed evidence binds to. Size alone does not say which files
//! those are, so `*.pdf bay-offload` in main's `.gitattributes` (BACKLOG 65, the one home of a
//! path's class) sends every new file of that kind to the store, however small, and `.bay-assets` names its bytes; the store
//! can purge them later and history keeps their address. Other files stay in Git.
//!
//! Guard to mutate: `src/assets.rs` `Rule::offloads`: drop the `by_kind` arm (the document enters
//! Git).

// The store is a shell script, so this runs on Unix only.
#![cfg(unix)]

use crate::g340_a_large_file_saved_never_enters_git::{STORE_PLUGIN, sha256_hex, store};
use crate::support;

use std::os::unix::fs::PermissionsExt as _;

use support::{TestBox, UNCHECKED_POLICY, result_json};

#[test]
fn a_small_document_a_glob_names_goes_to_the_store_and_a_note_stays_in_git() {
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
            (
                ".gitattributes".to_owned(),
                "*.pdf bay-offload\n".to_owned(),
            ),
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

    let document = b"%PDF-1.4 a client's contract\n".to_vec();
    std::fs::create_dir_all(repo.join("docs")).unwrap();
    std::fs::write(repo.join("docs/contract.pdf"), &document).unwrap();
    std::fs::write(repo.join("docs/notes.txt"), "notes\n").unwrap();
    let (code, saved) = result_json(test_box.bay(&repo).args(["save", "a contract and notes"]));
    assert_eq!(code, 0, "{saved}");
    assert_eq!(saved["decision"], "merged", "{saved}");

    let tree = test_box.git_bare(&remote, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(tree.lines().any(|path| path == "docs/notes.txt"), "{tree}");
    assert!(
        !tree.lines().any(|path| path == "docs/contract.pdf"),
        "the document entered Git: {tree}"
    );
    let hex = sha256_hex(&document);
    let manifest = test_box.git_bare(&remote, &["show", "main:.bay-assets"]);
    assert!(
        manifest.contains(&format!(
            "sha256:{hex} {} docs/contract.pdf",
            document.len()
        )),
        "{manifest}"
    );
    assert_eq!(
        std::fs::read(store(&test_box).join(&hex)).unwrap(),
        document
    );
}
