//! G170. A check materializes with one batched fetch.
//!
//! In a blob-less clone, the check's candidate arrives in one `git fetch --stdin` from the
//! promisor remote (or a second one for the adapter), not in one lazy fetch per file, and the
//! tree the check saw is exactly the candidate's.

// Linux only: the check copies its tree out, so it runs unconfined (ruling R14), and on macOS a
// check always runs under `sandbox-exec`. The whole file is gated, so its helpers are too.
#![cfg(target_os = "linux")]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, formats, passing_result, result_json};

/// The files of trunk besides the policy and the adapter.
const FILES: usize = 50;

/// The fetches a trace2 event log records that ask for objects by id (`fetch --stdin`): a
/// lazy fetch of one blob and a batched fetch of many alike. Git starts a lazy fetch through
/// its own exec path, so a `git` wrapper on `PATH` would never see it; trace2 does.
fn object_fetches(trace: &Path) -> usize {
    std::fs::read_to_string(trace)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(r#""event":"start""#))
        .filter(|line| line.contains(r#""fetch""#) && line.contains(r#""--stdin""#))
        .count()
}

/// The objects the cache does not hold.
fn missing(test_box: &TestBox, cache: &Path) -> usize {
    let out = test_box
        .git(cache)
        .env("GIT_NO_LAZY_FETCH", "1")
        .args(["rev-list", "--objects", "--missing=print", "--all"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|line| line.starts_with('?'))
        .count()
}

/// The tree id of the files under `dir`, as Git would record them in a repository of the
/// suite's object format.
fn tree_of(test_box: &TestBox, dir: &Path) -> String {
    let (object_format, _) = formats();
    let store = test_box.path().join("copied.git");
    test_box.git_ok(
        test_box.path(),
        &[
            "init",
            "-q",
            "--bare",
            &format!("--object-format={object_format}"),
            store.to_str().unwrap(),
        ],
    );
    let git_dir = format!("--git-dir={}", store.display());
    let work_tree = format!("--work-tree={}", dir.display());
    test_box.git_ok(dir, &[&git_dir, &work_tree, "add", "-A", "."]);
    test_box.git_ok(dir, &[&git_dir, &work_tree, "write-tree"])
}

#[test]
fn a_blobless_check_fetches_once_and_sees_the_exact_tree() {
    let test_box = TestBox::new();
    // The check copies what it sees out of its tree, so it runs unconfined (ruling R14).
    test_box.no_sandbox();
    let copied = test_box.path().join("copied");
    let seed = test_box.repo_any("seed");
    let remote = test_box.path().join("seed-remote.git");
    test_box.git_bare(&remote, &["config", "uploadpack.allowFilter", "true"]);
    let files: Vec<(String, String)> = (0..FILES)
        .map(|at| (format!("lib/m{at:02}.txt"), format!("module {at}\n")))
        .chain([("src/a.rs".to_owned(), "fn a() {}\n".to_owned())])
        .collect();
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let copy = format!(
        "rm -rf '{copied}'; mkdir -p '{copied}'; cp -Rp . '{copied}/'",
        copied = copied.display()
    );
    test_box.seed_with_check(
        &seed,
        &files,
        &check_files(&copy, &passing_result(1), 1, 60),
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
    let started = test_box.bay_ok(&cache, &["start", "CARD-1", "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    let absent = missing(&test_box, &cache);
    assert!(
        absent >= FILES,
        "the cache lacks the files the view did not declare: {absent}"
    );

    let trace = test_box.path().join("trace2.json");
    let (code, landed) = result_json(
        test_box
            .bay(&view)
            .env("GIT_TRACE2_EVENT", &trace)
            .args(["land", "--wait"]),
    );
    assert_eq!(code, 0, "{landed}");
    let fetches = object_fetches(&trace);
    assert!(
        fetches <= 2,
        "{fetches} object fetches for a check over {absent} absent blobs"
    );

    // The tree the check saw is the candidate's, which is what landed (G1).
    let landed_tree = test_box.git_bare(&remote, &["rev-parse", "main^{tree}"]);
    assert_eq!(tree_of(&test_box, &copied), landed_tree);
}
