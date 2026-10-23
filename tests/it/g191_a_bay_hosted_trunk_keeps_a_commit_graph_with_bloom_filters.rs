//! G191: a bay-hosted trunk keeps a commit-graph with generation numbers and changed-path Bloom
//! filters after pushes.
//!
//! `bay host` configures the host's graph (`core.commitGraph`, `gc.writeCommitGraph`,
//! `commitGraph.changedPaths`, version 2 filters) and enables maintenance's commit-graph task;
//! bay runs no maintenance itself (G172). After pushes and Git's own maintenance, the graph
//! verifies, and a path-limited walk from the newest tip reads a filter for every commit it
//! visits.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::TestBox;

#[test]
fn a_hosted_trunk_walks_its_history_through_bloom_filters_after_a_push() {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    // The test's own maintenance run below is the only one, so nothing races it.
    test_box.git_bare(&host, &["config", "maintenance.auto", "false"]);
    let git_dir = format!("--git-dir={}", host.display());

    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    for (path, text) in [("a/one", "1\n"), ("b/two", "2\n"), ("c/three", "3\n")] {
        test_box.seed_trunk(&repo, &[(path, text)]);
    }
    test_box.git_bare(&host, &["maintenance", "run", "--task=commit-graph"]);
    test_box.git_bare(&host, &["commit-graph", "verify"]);
    let walk = test_box.assert_walk_reads_filters(&[&git_dir], "main", "a/one");
    assert!(walk["definitely_not"].as_u64() > Some(0), "{walk}");

    // A later push and the next maintenance run: the new tip is in the graph with its filter.
    test_box.seed_trunk(&repo, &[("d/four", "4\n")]);
    test_box.git_bare(&host, &["maintenance", "run", "--task=commit-graph"]);
    test_box.git_bare(&host, &["commit-graph", "verify"]);
    test_box.assert_walk_reads_filters(&[&git_dir], "main", "d/four");

    // The settings that make it so, as `bay host` wrote them.
    for (key, value) in [
        ("core.commitGraph", "true"),
        ("gc.writeCommitGraph", "true"),
        ("commitGraph.changedPaths", "true"),
        ("commitGraph.changedPathsVersion", "2"),
        ("maintenance.commit-graph.enabled", "true"),
    ] {
        assert_eq!(
            test_box.git_bare(&host, &["config", "--get", key]),
            value,
            "{key}"
        );
    }
}
