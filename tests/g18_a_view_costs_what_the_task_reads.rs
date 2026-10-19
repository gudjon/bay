//! G18. A view costs what the task reads.
//!
//! `bay start` from a warm machine cache (a blob-less, no-checkout clone) opens a sparse view
//! of the declared paths and runs its first check with no full clone; declared paths stay
//! readable offline after `start`; an undeclared path fetched later is recorded as an observed
//! input; a check cannot write into another change's workspace; a view holds no links into
//! another live checkout.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

/// The objects the cache does not hold (`?` lines of a missing-object walk).
fn missing(test_box: &TestBox, cache: &Path) -> Vec<String> {
    let out = test_box
        .git(cache)
        .env("GIT_NO_LAZY_FETCH", "1")
        .args(["rev-list", "--objects", "--missing=print", "--all"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix('?'))
        .map(str::to_owned)
        .collect()
}

/// A remote whose history holds a large blob that was later deleted, and a machine cache.
fn remote_and_cache() -> (TestBox, PathBuf, PathBuf, String) {
    let test_box = TestBox::new();
    let seed = test_box.repo_any("seed");
    let remote = test_box.path().join("seed-remote.git");
    test_box.git_bare(&remote, &["config", "uploadpack.allowFilter", "true"]);
    let big = "x".repeat(300_000);
    test_box.seed_with_check(
        &seed,
        &[
            ("src/a.rs", "fn a() {}\n"),
            ("docs/guide.md", "guide\n"),
            ("old/huge.bin", &big),
        ],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let huge = test_box.git_ok(&seed, &["rev-parse", "HEAD:old/huge.bin"]);
    test_box.git_ok(&seed, &["rm", "-q", "-r", "old"]);
    test_box.git_ok(&seed, &["commit", "-q", "-m", "drop the old data"]);
    test_box.git_ok(&seed, &["push", "-q", "origin", "main"]);

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
    (test_box, remote, cache, huge)
}

#[test]
fn a_sparse_view_from_the_cache_lands_with_no_full_clone() {
    let (test_box, remote, cache, huge) = remote_and_cache();
    let docs = test_box.git_ok(&cache, &["rev-parse", "origin/main:docs/guide.md"]);
    assert!(
        missing(&test_box, &cache).contains(&docs),
        "the cache starts blob-less"
    );

    let started = test_box.bay_ok(&cache, &["start", "CARD-1", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    assert!(view.join("src/a.rs").is_file());
    assert!(!view.join("docs/guide.md").exists());
    assert!(
        missing(&test_box, &cache).contains(&docs),
        "start fetched only the declared paths"
    );

    // A view links only into the cache, which is not a live checkout, and holds no symlinks.
    let link = std::fs::read_to_string(view.join(".git")).unwrap();
    assert!(
        link.contains(&format!("{}", cache.join(".git").display())),
        "{link}"
    );
    let checked_out: Vec<_> = std::fs::read_dir(&cache)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(
        checked_out,
        vec![std::ffi::OsString::from(".git")],
        "the cache has no work tree files"
    );
    for entry in walk(&view) {
        assert!(
            !entry.symlink_metadata().unwrap().file_type().is_symlink(),
            "{}",
            entry.display()
        );
    }

    // The first check runs from the cache; history it never needed is still not fetched.
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
    assert!(
        missing(&test_box, &cache).contains(&huge),
        "no full clone: old history stays unfetched"
    );
}

#[test]
fn declared_paths_stay_readable_offline_after_start() {
    let (test_box, remote, cache, _) = remote_and_cache();
    let started = test_box.bay_ok(&cache, &["start", "CARD-1", "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::rename(&remote, remote.with_extension("away")).unwrap();
    assert_eq!(
        std::fs::read_to_string(view.join("src/a.rs")).unwrap(),
        "fn a() {}\n"
    );
    let out = test_box
        .git(&view)
        .env("GIT_NO_LAZY_FETCH", "1")
        .args(["diff", "--stat", "HEAD"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn an_undeclared_path_fetched_later_is_an_observed_input() {
    let (test_box, _remote, cache, _) = remote_and_cache();
    let started = test_box.bay_ok(&cache, &["start", "CARD-1", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    test_box.git_ok(&view, &["sparse-checkout", "add", "/docs/"]);
    assert!(view.join("docs/guide.md").is_file());
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    let (code, _) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0);
    let record = test_box.git_ok(
        &cache,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    let record: serde_json::Value = serde_json::from_str(&record).unwrap();
    assert_eq!(record["declared_inputs"], serde_json::json!(["src/"]));
    assert_eq!(record["observed_inputs"], serde_json::json!(["docs/"]));
}

#[test]
fn a_check_cannot_write_into_another_changes_workspace() {
    let Some(sandbox) = support::sandbox() else {
        eprintln!("no check sandbox on this machine; G18's sandbox case cannot run");
        return;
    };
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let views = repo.join(".git/bay/views");
    // The check tries to write into every view on the machine.
    let before = format!(
        "for v in '{}'/*/; do echo pwned > \"$v/src/pwned.rs\"; done",
        views.display()
    );
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files(&before, &passing_result(1), 1, 60),
    );
    let other = test_box.bay_ok(&repo, &["start", "CARD-2", "--paths", "src/"]);
    let other_view = PathBuf::from(other["detail"]["view"].as_str().unwrap());
    let (_, view) = test_box.start_and_edit(&repo);
    let (_, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    // The check ran, inside the sandbox: its evidence record says which one.
    let notes = test_box.git_ok(
        &repo,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/notes/bay/evidence/",
        ],
    );
    let records: String = notes
        .lines()
        .flat_map(|notes_ref| {
            test_box
                .git_ok(
                    &repo,
                    &["ls-tree", "-r", "--format=%(objectname)", notes_ref],
                )
                .lines()
                .map(|blob| test_box.git_ok(&repo, &["cat-file", "blob", blob]))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(
        records.contains(&format!("\"sandbox\": \"{sandbox}\"")),
        "no evidence record from a {sandbox} run: {landed}\n{records}"
    );
    assert!(
        !other_view.join("src/pwned.rs").exists(),
        "the check wrote into another change's view"
    );
    assert!(!view.join("src/pwned.rs").exists());
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|name| name == ".git") {
            continue;
        }
        if path.is_dir() && !path.symlink_metadata().unwrap().file_type().is_symlink() {
            out.extend(walk(&path));
        }
        out.push(path);
    }
    out
}
