//! G192: `bay doctor` reports the commit-graph of each store (`DESIGN` § 15.2): missing, without
//! changed-path Bloom filters Git reads, or ok, in the `store` row. A graph only speeds walks
//! up, so every finding is advisory and never a refusal.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, result_json, row};

fn store(test_box: &TestBox, dir: &std::path::Path) -> serde_json::Value {
    let (_, doctor) = result_json(test_box.bay(dir).arg("doctor"));
    assert_ne!(doctor["decision"], "blocked", "{doctor}");
    let store = row(&doctor, "store").clone();
    assert_ne!(store["state"], "red", "{doctor}");
    store
}

fn graph_state(test_box: &TestBox, dir: &std::path::Path) -> serde_json::Value {
    let store = store(test_box, dir);
    store["commit_graph"]["state"].clone()
}

#[test]
fn doctor_names_a_missing_a_filterless_and_a_full_commit_graph() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    test_box.seed_trunk(&repo, &[("src/b.rs", "fn b() {}\n")]);

    // No graph: advisory, with the command that writes one.
    let row = store(&test_box, &repo);
    assert_eq!(row["commit_graph"]["state"], "missing", "{row}");
    assert_eq!(row["state"], "advisory", "{row}");
    assert!(
        row["notes"].to_string().contains("no commit-graph"),
        "{row}"
    );
    assert_eq!(row["fetch_writes_commit_graph"], false, "{row}");

    // A graph with no filters.
    test_box.git_ok(
        &repo,
        &["commit-graph", "write", "--reachable", "--no-changed-paths"],
    );
    let row = store(&test_box, &repo);
    assert_eq!(row["commit_graph"]["state"], "no_bloom_filters", "{row}");
    assert!(
        row["notes"]
            .to_string()
            .contains("no changed-path Bloom filters"),
        "{row}"
    );

    // Version 1 filters under `commitGraph.changedPathsVersion=2`: Git does not read them.
    test_box.git_ok(
        &repo,
        &[
            "-c",
            "commitGraph.changedPathsVersion=1",
            "commit-graph",
            "write",
            "--reachable",
            "--changed-paths",
        ],
    );
    assert_eq!(graph_state(&test_box, &repo), "ok");
    test_box.git_ok(&repo, &["config", "commitGraph.changedPathsVersion", "2"]);
    assert_eq!(graph_state(&test_box, &repo), "no_bloom_filters");

    // A full graph, and its version-2 filters, with fetch keeping it fresh: no graph note.
    test_box.git_ok(
        &repo,
        &["commit-graph", "write", "--reachable", "--changed-paths"],
    );
    test_box.git_ok(&repo, &["config", "fetch.writeCommitGraph", "true"]);
    let row = store(&test_box, &repo);
    assert_eq!(row["commit_graph"]["state"], "ok", "{row}");
    assert_eq!(
        row["commit_graph"]["bloom_versions"],
        serde_json::json!([2]),
        "{row}"
    );
    assert_eq!(row["fetch_writes_commit_graph"], true, "{row}");
    assert!(!row["notes"].to_string().contains("commit-graph"), "{row}");

    // A split chain on top of it keeps the filters.
    test_box.seed_trunk(&repo, &[("src/c.rs", "fn c() {}\n")]);
    test_box.git_ok(&repo, &["commit-graph", "write", "--reachable", "--split"]);
    assert_eq!(graph_state(&test_box, &repo), "ok");

    // Switched off, Git reads none of it.
    test_box.git_ok(&repo, &["config", "core.commitGraph", "false"]);
    assert_eq!(graph_state(&test_box, &repo), "disabled");
}

#[test]
fn doctor_on_a_bay_hosted_trunk_reports_its_graph() {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    test_box.git_bare(&host, &["config", "maintenance.auto", "false"]);
    // Born empty, the host has no graph until Git's maintenance writes one.
    let row = store(&test_box, &host);
    assert_eq!(row["commit_graph"]["state"], "missing", "{row}");
    assert_eq!(
        row["fetch_writes_commit_graph"],
        serde_json::Value::Null,
        "{row}"
    );

    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    test_box.git_bare(&host, &["maintenance", "run", "--task=commit-graph"]);
    let row = store(&test_box, &host);
    assert_eq!(row["commit_graph"]["state"], "ok", "{row}");
    assert_eq!(
        row["commit_graph"]["bloom_versions"],
        serde_json::json!([2]),
        "{row}"
    );
}
