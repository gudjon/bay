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
