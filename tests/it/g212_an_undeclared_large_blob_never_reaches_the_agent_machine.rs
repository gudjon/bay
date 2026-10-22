//! G212. An undeclared large blob never reaches the agent's machine (extends G18).
//!
//! On a machine whose cache is a blob-less, no-checkout clone, the agent's whole path fetches
//! the blobs of its declared paths and bay's own files, never one outside them: `start --paths
//! src/`, edits under `src/`, `status`, `watch`, `start --resume` after trunk moved (a view's
//! refresh) and `land --wait`. A large asset on trunk, and the newer one trunk moves to, stay
//! absent from the cache throughout: the agent pays for what it declared, whatever trunk holds.

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, result_json, unchecked_files};

/// About 2 MiB: large beside the declared files.
fn large(seed: char) -> String {
    format!("{seed}123456789abcdef").repeat(131_072)
}

/// Whether the cache holds `oid`, asked without fetching it.
fn holds(test_box: &TestBox, cache: &Path, oid: &str) -> bool {
    test_box
        .git(cache)
        .env("GIT_NO_LAZY_FETCH", "1")
        .args(["cat-file", "-e", oid])
        .output()
        .unwrap()
        .status
        .success()
}

/// A remote whose trunk holds a large asset outside `src/`, its seed clone, and a blob-less
/// machine cache of it.
fn remote_and_cache() -> (TestBox, PathBuf, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let seed = test_box.repo_any("seed");
    let remote = test_box.path().join("seed-remote.git");
    test_box.git_bare(&remote, &["config", "uploadpack.allowFilter", "true"]);
    test_box.seed_with_check(
        &seed,
        &[
            ("src/a.rs", "fn a() {}\n"),
            ("docs/guide.md", "guide\n"),
            ("assets/big.bin", &large('0')),
        ],
        &unchecked_files(),
    );
    let cache = test_box.path().join("cache");
    test_box.git_ok(
        test_box.path(),
        &[
            "clone",
            "-q",
            "--filter=blob:none",
            "--no-checkout",
            &format!("file://{}", remote.display()),
            cache.to_str().unwrap(),
        ],
    );
    (test_box, remote, seed, cache)
}

#[test]
fn the_agents_whole_path_never_fetches_an_undeclared_blob() {
    let (test_box, remote, seed, cache) = remote_and_cache();
    let big = test_box.git_ok(&seed, &["rev-parse", "HEAD:assets/big.bin"]);
    let docs = test_box.git_ok(&seed, &["rev-parse", "HEAD:docs/guide.md"]);
    assert!(
        !holds(&test_box, &cache, &big),
        "the cache starts blob-less"
    );

    let started = test_box.bay_ok(&cache, &["start", "CARD-1", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    assert!(view.join("src/a.rs").is_file());
    assert!(!view.join("assets/big.bin").exists());
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    test_box.bay_ok(&view, &["status"]);
    test_box.bay_ok(&view, &["status", "--since"]);
    for (oid, what) in [(&big, "the large asset"), (&docs, "an undeclared file")] {
        assert!(
            !holds(&test_box, &cache, oid),
            "start, an edit, status and watch fetched {what}"
        );
    }

    // Trunk moves: a newer large asset and a declared file. Resuming brings trunk into the view.
    std::fs::write(seed.join("assets/big.bin"), large('1')).unwrap();
    std::fs::write(seed.join("src/a.rs"), "fn a() { /* moved */ }\n").unwrap();
    test_box.git_ok(&seed, &["commit", "-q", "-am", "trunk moves"]);
    test_box.git_ok(&seed, &["push", "-q", "origin", "main"]);
    let newer = test_box.git_ok(&seed, &["rev-parse", "HEAD:assets/big.bin"]);
    test_box.bay_ok(&cache, &["start", &id]);
    assert_eq!(
        std::fs::read_to_string(view.join("src/a.rs")).unwrap(),
        "fn a() { /* moved */ }\n",
        "the refreshed view holds trunk's declared file"
    );
    assert!(!view.join("assets/big.bin").exists());

    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
    for (oid, what) in [
        (&big, "the large asset"),
        (&newer, "trunk's newer large asset"),
        (&docs, "an undeclared file"),
    ] {
        assert!(
            !holds(&test_box, &cache, oid),
            "the agent's path fetched {what}"
        );
    }
}
