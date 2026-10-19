//! G1. What lands is exactly what was checked.
//!
//! Trunk advances while a check runs. The old candidate is refused at publication,
//! rebuilt on the new trunk, and rechecked. The check request names exactly the paths the
//! candidate changes against the trunk it was merged onto.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

mod support;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn trunk_advances_while_a_check_runs() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let other = test_box.path().join("other");
    let marker = test_box.path().join("moved-once");
    // The first check moves trunk from another clone before it answers.
    let before = format!(
        "if [ ! -f '{marker}' ]; then touch '{marker}'; ( cd '{other}' && echo moved > docs/moved.md && git add docs/moved.md && git -c user.name=t -c user.email=t@t commit -q -m 'trunk moves' && git push -q origin main ); fi",
        marker = marker.display(),
        other = other.display()
    );
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("docs/b.md", "b\n")],
        &check_files(&before, &passing_result(2), 2, 60),
    );
    test_box.clone_of(&remote, "other");
    let (id, view) = test_box.start_and_edit(&repo);

    let (code, result) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["decision"], "landed");
    let published = result["detail"]["commit"].as_str().unwrap().to_owned();

    // What landed is the second candidate, built on the moved trunk.
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);
    assert_eq!(tip, published);
    let parent_subject = test_box.git_bare(&remote, &["log", "-1", "--format=%s", "main^"]);
    assert_eq!(parent_subject, "trunk moves");
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:docs/moved.md"]),
        "moved"
    );
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:src/new.rs"]),
        "fn new() {}"
    );
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id.clone()]);

    // Two attempts; the first candidate never reached trunk; the second was checked itself.
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let change = &status["detail"]["changes"][0];
    assert_eq!(change["state"], "landed");
    assert_eq!(change["operation"]["attempts"], 2);
    assert_eq!(change["operation"]["candidate"], format!("{id}.1.2"));
    let tree = test_box.git_bare(&remote, &["rev-parse", "main^{tree}"]);
    let note = test_box.git_bare(
        &remote,
        &[
            "notes",
            "--ref=refs/notes/bay/evidence/local",
            "show",
            &tree,
        ],
    );
    assert!(
        note.contains(&published),
        "the evidence names the published commit: {note}"
    );
    assert!(note.contains("\"verdict\": \"pass\""), "{note}");
}

#[test]
fn the_check_request_names_exactly_the_paths_the_candidate_changes() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let seen = test_box.path().join("request.json");
    // The adapter keeps the request it was handed.
    let before = format!("printf '%s' \"$req\" > '{}'", seen.display());
    test_box.seed_with_check(
        &repo,
        &[
            ("src/a.rs", "fn a() {}\n"),
            ("src/gone.rs", "fn gone() {}\n"),
            ("src/same.rs", "fn same() {}\n"),
            ("docs/b.md", "b\n"),
        ],
        &check_files(&before, &passing_result(1), 1, 60),
    );
    let (_, view) = test_box.start_and_edit(&repo);
    std::fs::write(view.join("src/a.rs"), "fn a() { 1; }\n").unwrap();
    std::fs::remove_file(view.join("src/gone.rs")).unwrap();
    // Git quotes this name unless it is asked for NUL-separated output.
    std::fs::write(view.join("src/na\u{ef}ve.rs"), "fn naive() {}\n").unwrap();

    let (code, result) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["decision"], "landed", "{result}");

    let request: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&seen).unwrap()).unwrap();
    // Added, modified and deleted paths, sorted; unchanged ones are absent.
    assert_eq!(
        request["changed_paths"],
        serde_json::json!(["src/a.rs", "src/gone.rs", "src/na\u{ef}ve.rs", "src/new.rs"]),
        "{request}"
    );
    // The request carries nothing beyond its documented fields.
    let mut keys: Vec<&str> = request
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "candidate",
            "changed_paths",
            "inputs",
            "plan",
            "requires",
            "schema",
            "timeout_s"
        ],
        "{request}"
    );
}
