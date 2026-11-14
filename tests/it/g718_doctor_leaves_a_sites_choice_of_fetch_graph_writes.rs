//! G718 (a fleet's doctor, 2026-11-14: a store many sessions fetch into keeps one writer of its
//! commit-graph, and doctor advised every fetch to write it). Doctor advises a fetch that
//! refreshes the commit-graph only where `fetch.writeCommitGraph` is unset: a site that set it to
//! false chose so, and doctor's store row then gives no advice against it.
//!
//! Guard to mutate: `src/doctor.rs`, the store row's fetch note: advise it whenever a fetch does
//! not write the graph (the note returns on a site that set false).

use crate::support;

use support::{TestBox, result_json, row};

fn store_notes(test_box: &TestBox, repo: &std::path::Path) -> String {
    let (_, result) = result_json(test_box.bay(repo).arg("doctor"));
    row(&result, "store")["notes"].to_string()
}

#[test]
fn doctor_advises_fetch_graph_writes_only_where_the_key_is_unset() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");

    // GIVEN the key unset: doctor advises it
    assert!(store_notes(&test_box, &repo).contains("a fetch does not refresh"));

    // WHEN the site sets it false
    test_box.git_ok(&repo, &["config", "fetch.writeCommitGraph", "false"]);

    // THEN doctor gives no advice against that choice
    assert!(!store_notes(&test_box, &repo).contains("a fetch does not refresh"));
}
