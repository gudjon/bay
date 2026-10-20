//! G104b. Metadata reads never fetch lazily (`GIT-FEATURES` § 2 row 6).
//!
//! On a blob-less cache, Git never fetches an object on its own inside a call `bay status` or
//! `bay watch` runs: every such call runs with lazy fetch off. A file of bay's own that trunk
//! moved (here its config) is read by name, and the cache gets exactly that blob in one
//! explicit fetch by id. When the object cannot be had, the read is an error bay reports,
//! never a pass on a guess and never a fetch Git starts by itself.

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, result_json, unchecked_files};

/// The fetches Git started on its own inside another Git call (`child_start` of a
/// `fetch --stdin`, the form of a lazy fetch), from a trace2 event log. A fetch bay itself
/// runs has no Git parent, so it is never one of them.
fn lazy_fetches(trace: &Path) -> Vec<String> {
    std::fs::read_to_string(trace)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(r#""event":"child_start""#))
        .filter(|line| line.contains(r#""fetch""#) && line.contains(r#""--stdin""#))
        .map(str::to_owned)
        .collect()
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

/// Appends a comment to `path` in the seed, commits and pushes it; returns the new blob.
fn move_trunk(test_box: &TestBox, seed: &Path, path: &str, line: &str) -> String {
    let full = seed.join(path);
    let mut text = std::fs::read_to_string(&full).unwrap();
    text.push_str("\n# ");
    text.push_str(line);
    text.push('\n');
    std::fs::write(&full, text).unwrap();
    std::fs::write(seed.join("docs/guide.md"), format!("guide, {line}\n")).unwrap();
    test_box.git_ok(seed, &["commit", "-q", "-am", line]);
    test_box.git_ok(seed, &["push", "-q", "origin", "main"]);
    test_box.git_ok(seed, &["rev-parse", &format!("HEAD:{path}")])
}

/// A remote, its seed clone, a blob-less cache of it, and a view of `src/` in the cache.
fn cache_and_view() -> (TestBox, PathBuf, PathBuf, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let seed = test_box.repo_any("seed");
    let remote = test_box.path().join("seed-remote.git");
    test_box.git_bare(&remote, &["config", "uploadpack.allowFilter", "true"]);
    test_box.seed_with_check(
        &seed,
        &[("src/a.rs", "fn a() {}\n"), ("docs/guide.md", "guide\n")],
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
    let started = test_box.bay_ok(&cache, &["start", "CARD-1", "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    (test_box, remote, seed, cache, view)
}

#[test]
fn status_and_watch_read_bays_files_by_name_and_never_lazily() {
    let (test_box, _remote, seed, cache, view) = cache_and_view();
    let path = ".bay/config.toml";
    for command in ["status", "watch"] {
        let blob = move_trunk(&test_box, &seed, path, &format!("before {command}"));
        let docs = test_box.git_ok(&seed, &["rev-parse", "HEAD:docs/guide.md"]);
        let trace = test_box.path().join(format!("{command}-trace2.json"));
        let (code, result) = result_json(
            test_box
                .bay(&view)
                .env("GIT_TRACE2_EVENT", &trace)
                .arg(command),
        );
        assert_eq!(code, 0, "{result}");
        let lazy = lazy_fetches(&trace);
        assert!(lazy.is_empty(), "bay {command} fetched lazily: {lazy:#?}");
        assert!(
            holds(&test_box, &cache, &blob),
            "bay {command} read trunk's {path}, fetched by id"
        );
        assert!(
            !holds(&test_box, &cache, &docs),
            "bay {command} fetched an undeclared file"
        );
    }
}

#[test]
fn a_metadata_object_the_cache_cannot_have_is_an_error() {
    let (test_box, remote, seed, cache, view) = cache_and_view();
    let blob = move_trunk(&test_box, &seed, ".bay/config.toml", "offline");
    // The cache learns trunk's commit and trees, not its blobs, then the remote goes away.
    let fetched = test_box
        .git(&cache)
        .env("GIT_NO_LAZY_FETCH", "1")
        .args(["fetch", "-q", "origin"])
        .output()
        .unwrap();
    assert!(fetched.status.success(), "{fetched:?}");
    assert!(!holds(&test_box, &cache, &blob));
    std::fs::rename(&remote, remote.with_extension("away")).unwrap();

    for command in ["status", "watch"] {
        let trace = test_box
            .path()
            .join(format!("offline-{command}-trace2.json"));
        let (code, result) = result_json(
            test_box
                .bay(&view)
                .env("GIT_TRACE2_EVENT", &trace)
                .arg(command),
        );
        assert_ne!(result["decision"], "ok", "{result}");
        assert_ne!(code, 0, "{result}");
        assert!(
            result.to_string().contains(".bay/config.toml"),
            "the error names the file bay could not read: {result}"
        );
        let lazy = lazy_fetches(&trace);
        assert!(lazy.is_empty(), "bay {command} fetched lazily: {lazy:#?}");
    }
}

/// The cache learns trunk's commit and trees, not its blobs; then its promisor remote is one
/// that is gone, while trunk's remote still answers ref fetches and pushes. A blob the cache
/// lacks can no longer be had by id: unreadable, though the tree names it.
fn lose_promisor(test_box: &TestBox, cache: &Path) {
    let fetched = test_box
        .git(cache)
        .env("GIT_NO_LAZY_FETCH", "1")
        .args(["fetch", "-q", "origin"])
        .output()
        .unwrap();
    assert!(fetched.status.success(), "{fetched:?}");
    let gone = test_box.path().join("promisor-gone.git");
    for (key, value) in [
        ("remote.origin.promisor", "false"),
        ("remote.lost.url", gone.to_str().unwrap()),
        ("remote.lost.promisor", "true"),
        ("extensions.partialClone", "lost"),
    ] {
        test_box.git_ok(cache, &["config", key, value]);
    }
}

#[test]
fn a_policy_the_cache_cannot_read_is_an_error_naming_it_never_no_policy() {
    let path = ".bay/policy.toml";
    let (test_box, _remote, seed, cache, view) = cache_and_view();
    let blob = move_trunk(&test_box, &seed, path, "unreadable");
    lose_promisor(&test_box, &cache);
    assert!(!holds(&test_box, &cache, &blob));

    // Status reads trunk's red judgement as unread, naming the file: never an empty list.
    let (code, status) = result_json(test_box.bay(&view).arg("status"));
    assert_eq!(code, 0, "{status}");
    let red = &status["detail"]["red"];
    assert!(
        red["unread"].as_str().is_some_and(|why| why.contains(path)),
        "an unreadable policy is no judgement of red: {status}"
    );

    // Watch cannot judge red either, and says which file it could not read.
    let (code, watched) = result_json(test_box.bay(&view).arg("watch"));
    assert_ne!(code, 0, "{watched}");
    assert_ne!(watched["decision"], "ok", "{watched}");
    assert!(watched.to_string().contains(path), "{watched}");

    // Land fails naming the file it could not read: not POLICY_MISSING, and nothing lands.
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_ne!(code, 0, "{landed}");
    assert_ne!(landed["reason"], "POLICY_MISSING", "{landed}");
    assert!(!landed.to_string().contains("POLICY_MISSING"), "{landed}");
    assert!(landed.to_string().contains(path), "{landed}");
    assert!(
        landed
            .to_string()
            .contains("absent from this partial clone"),
        "the error says the file is unreadable, not absent: {landed}"
    );
    assert!(!holds(&test_box, &cache, &blob));
}

#[test]
fn a_trunk_with_no_policy_file_is_still_policy_missing_on_a_blob_less_cache() {
    let (test_box, _remote, seed, cache, view) = cache_and_view();
    test_box.git_ok(&seed, &["rm", "-q", ".bay/policy.toml"]);
    test_box.git_ok(&seed, &["commit", "-q", "-m", "no policy"]);
    test_box.git_ok(&seed, &["push", "-q", "origin", "main"]);
    let trunk = test_box.git_ok(&seed, &["rev-parse", "HEAD"]);
    lose_promisor(&test_box, &cache);

    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{landed}");
    assert_eq!(landed["reason"], "POLICY_MISSING", "{landed}");
    assert_eq!(landed["detail"]["base"], trunk.as_str(), "{landed}");
    assert_eq!(landed["detail"]["path"], ".bay/policy.toml", "{landed}");
}
