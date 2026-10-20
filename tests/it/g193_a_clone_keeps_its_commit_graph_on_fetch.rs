//! G193: an executor's clone and a shared cache keep a commit-graph with changed-path Bloom
//! filters on fetch.
//!
//! `bay install` in a clone sets `fetch.writeCommitGraph` (and, where Git has them,
//! `commitGraph.changedPaths` and version 2 filters) and writes the first graph with filters,
//! since a fetch's write only keeps filters the graph already has. After a fetch, the graph
//! verifies and a path-limited walk from the fetched tip reads a filter for every commit.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use bay::version::Version;
use support::{TestBox, result_json, row};

#[test]
fn an_installed_clone_reads_bloom_filters_for_what_it_fetched() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    test_box.seed_trunk(&repo, &[("src/b.rs", "fn b() {}\n")]);

    // A blob-less store as a site makes one, then `bay install` in it.
    test_box.git_bare(&remote, &["config", "uploadpack.allowFilter", "true"]);
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
    // Only the fetch writes the graph here: no maintenance runs beside it.
    test_box.git_ok(&cache, &["config", "maintenance.auto", "false"]);
    let installed = test_box.bay_ok(&cache, &["install"]);
    let graph = &installed["detail"]["commit_graph"];
    assert_eq!(graph["state"], "ok", "{installed}");
    assert_eq!(
        test_box.git_ok(&cache, &["config", "--get", "fetch.writeCommitGraph"]),
        "true"
    );
    let git = Version::parse(&test_box.git_ok(test_box.path(), &["--version"]));
    if git >= Version::parse(bay::commit_graph::BLOOM_V2_SINCE) {
        assert_eq!(
            test_box.git_ok(
                &cache,
                &["config", "--get", "commitGraph.changedPathsVersion"]
            ),
            "2"
        );
    }

    // New trunk commits, fetched: the graph covers them, with their filters.
    test_box.seed_trunk(&repo, &[("src/c.rs", "fn c() {}\n")]);
    test_box.seed_trunk(&repo, &[("src/d.rs", "fn d() {}\n")]);
    test_box.git_ok(&cache, &["fetch", "-q", "origin"]);
    test_box.git_ok(&cache, &["commit-graph", "verify"]);
    let walk = test_box.assert_walk_reads_filters(
        &["-C", cache.to_str().unwrap()],
        "refs/remotes/origin/main",
        "src/d.rs",
    );
    assert!(walk["definitely_not"].as_u64() > Some(0), "{walk}");

    // Doctor agrees, and installing again changes nothing the site or the last run set.
    let (_, doctor) = result_json(test_box.bay(&cache).arg("doctor"));
    let store = row(&doctor, "store");
    assert_eq!(store["commit_graph"]["state"], "ok", "{store}");
    assert_eq!(store["fetch_writes_commit_graph"], true, "{store}");
    let again = test_box.bay_ok(&cache, &["install"]);
    assert_eq!(
        again["detail"]["commit_graph"]["set"],
        serde_json::json!([]),
        "{again}"
    );
}
