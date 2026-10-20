//! G175. A view can warm its history, and says how much.
//!
//! With `[view] warm_history = true`, `bay start` and `start --resume` fetch the history of the
//! declared paths into a blob-less store with `git backfill`, and the result's `history` names
//! the missing-object counts before and after, the objects fetched and their bytes. Nothing of
//! an undeclared path's history is fetched. A declared directory written with its trailing `/`
//! is warmed. Without the switch nothing old is fetched (G18 still holds), and below Git 2.54 the
//! warm-up is `skipped` with its floor.

// The store is served over `file://` and the remote is moved away to prove the view offline.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result};

/// The versions of the declared and the undeclared file, oldest first; the last is the tip.
const SRC: [&str; 3] = ["fn a() { 1 }\n", "fn a() { 22 }\n", "fn a() { 333 }\n"];
const DOCS: [&str; 3] = ["guide one\n", "guide two\n", "guide three\n"];

/// A remote whose trunk changed `src/a.rs` and `docs/guide.md` three times, and a blob-less,
/// no-checkout clone of it. Returns the box, the remote, the clone, and the ids of the two old
/// versions of each file.
fn remote_and_cache(config: &str) -> (TestBox, PathBuf, PathBuf, Vec<String>, Vec<String>) {
    let test_box = TestBox::new();
    let seed = test_box.repo_any("seed");
    let remote = test_box.path().join("seed-remote.git");
    test_box.git_bare(&remote, &["config", "uploadpack.allowFilter", "true"]);
    let mut check = check_files("true", &passing_result(1), 1, 60);
    check.push((".bay/config.toml".to_owned(), config.to_owned()));
    test_box.seed_with_check(
        &seed,
        &[("src/a.rs", SRC[0]), ("docs/guide.md", DOCS[0])],
        &check,
    );
    let (mut old_src, mut old_docs) = (Vec::new(), Vec::new());
    for version in 1..3 {
        old_src.push(test_box.git_ok(&seed, &["rev-parse", "HEAD:src/a.rs"]));
        old_docs.push(test_box.git_ok(&seed, &["rev-parse", "HEAD:docs/guide.md"]));
        std::fs::write(seed.join("src/a.rs"), SRC[version]).unwrap();
        std::fs::write(seed.join("docs/guide.md"), DOCS[version]).unwrap();
        test_box.git_ok(&seed, &["commit", "-q", "-a", "-m", "next version"]);
    }
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
    (test_box, remote, cache, old_src, old_docs)
}

/// Whether the store at `dir` holds object `id`, asked without fetching it.
fn held(test_box: &TestBox, dir: &Path, id: &str) -> bool {
    test_box
        .git(dir)
        .env("GIT_NO_LAZY_FETCH", "1")
        .args(["cat-file", "-e", id])
        .output()
        .unwrap()
        .status
        .success()
}

/// Whether this box's real Git has `backfill` with a pathspec (2.54).
fn backfill_takes_a_pathspec(test_box: &TestBox) -> bool {
    let line = test_box.git_ok(test_box.path(), &["--version"]);
    let version: Vec<u64> = line
        .split_whitespace()
        .nth(2)
        .unwrap_or_default()
        .split('.')
        .map_while(|part| part.parse().ok())
        .collect();
    version.as_slice() >= [2, 54].as_slice()
}

const WARM: &str = "[view]\nwarm_history = true\n";

#[test]
fn start_warms_the_declared_history_and_says_how_much() {
    let (test_box, remote, cache, old_src, old_docs) = remote_and_cache(WARM);
    if !backfill_takes_a_pathspec(&test_box) {
        eprintln!("git below 2.54 on this machine; G175's warm-up case cannot run");
        return;
    }
    assert!(
        old_src.iter().all(|id| !held(&test_box, &cache, id)),
        "the cache starts blob-less"
    );

    // `src/`, with the trailing `/` that `git backfill` alone would match nothing with.
    let started = test_box.bay_ok(&cache, &["start", "CARD-1", "--paths", "src/"]);
    let history = &started["detail"]["history"];
    assert_eq!(history["state"], "warmed", "{started}");
    assert_eq!(history["paths"], serde_json::json!(["src"]), "{history}");
    assert_eq!(history["missing_before"], 2, "{history}");
    assert_eq!(history["missing_after"], 0, "{history}");
    assert_eq!(history["fetched"], 2, "{history}");
    assert_eq!(history["bytes"], SRC[0].len() + SRC[1].len(), "{history}");
    for id in &old_src {
        assert!(
            held(&test_box, &cache, id),
            "old src/a.rs {id} was not fetched"
        );
    }
    for id in &old_docs {
        assert!(
            !held(&test_box, &cache, id),
            "old docs/guide.md {id} was fetched, though undeclared"
        );
    }

    // The declared history reads offline, with no lazy fetch.
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::rename(&remote, remote.with_extension("away")).unwrap();
    let out = test_box
        .git(&view)
        .env("GIT_NO_LAZY_FETCH", "1")
        .args(["log", "-p", "--format=%H", "--", "src/"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = String::from_utf8_lossy(&out.stdout);
    assert!(log.contains("fn a() { 1 }"), "{log}");
}

#[test]
fn without_the_switch_nothing_old_is_fetched_until_resume_asks() {
    let (test_box, _remote, cache, old_src, old_docs) = remote_and_cache("");
    let started = test_box.bay_ok(&cache, &["start", "CARD-1", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    assert_eq!(
        started["detail"]["history"],
        serde_json::Value::Null,
        "{started}"
    );
    for blob in old_src.iter().chain(&old_docs) {
        assert!(!held(&test_box, &cache, blob), "{blob} was fetched unasked");
    }
    if !backfill_takes_a_pathspec(&test_box) {
        eprintln!("git below 2.54 on this machine; G175's resume case cannot run");
        return;
    }

    // The switch turned on at trunk: the next resume warms the change's declared paths.
    test_box.commit_config(&cache, WARM);
    let resumed = test_box.bay_ok(&cache, &["start", "--resume", &id]);
    let history = &resumed["detail"]["history"];
    assert_eq!(history["state"], "warmed", "{resumed}");
    assert_eq!(history["fetched"], 2, "{history}");
    for id in &old_src {
        assert!(
            held(&test_box, &cache, id),
            "old src/a.rs {id} was not fetched"
        );
    }
    for id in &old_docs {
        assert!(
            !held(&test_box, &cache, id),
            "old docs/guide.md {id} was fetched"
        );
    }
}

#[test]
fn below_git_2_54_the_warm_up_is_skipped_with_its_floor() {
    let (test_box, _remote, cache, old_src, _) = remote_and_cache(WARM);
    test_box.fake_git_version("2.53.0");
    let started = test_box.bay_ok(&cache, &["start", "CARD-1", "--paths", "src/"]);
    let history = &started["detail"]["history"];
    assert_eq!(history["state"], "skipped", "{started}");
    assert_eq!(history["floor"], "2.54", "{history}");
    assert_eq!(history["git"], "2.53.0", "{history}");
    for id in &old_src {
        assert!(
            !held(&test_box, &cache, id),
            "{id} was fetched below the floor"
        );
    }
}
