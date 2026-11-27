//! G942 (BACKLOG 354, a fleet finding, 2026-11-27: `bay start <change> --paths /crates/…` wrote
//! each path with its leading `/` into the change's record, and that pick-up and every later one
//! failed at `git diff … -- /crates`, "Invalid path", with nothing changed). A `--paths` value is
//! a path from the repository's root, `/src/` is `src/`: the record declares it as git names it,
//! and a pick-up after main moved brings main in. A path with a `..` part is refused before
//! anything is written.
//!
//! Guards to mutate: `src/main.rs` `start --paths` without its `value_parser` (the record
//! declares `/src/`); `src/inputs.rs` `paths` reading a path as written (with the parser gone
//! too, the pick-up after main moved fails); `declared_path` taking a `..` part (the start is
//! not refused). The read of a record a 0.7.42 start wrote is
//! `inputs::tests::a_record_with_a_leading_slash_reads_from_the_root`.

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::{TestBox, check_files, passing_result, result_json};

/// The paths the record of `id` declares, in `repo`'s copy of it.
fn declared(test_box: &TestBox, repo: &Path, id: &str) -> Value {
    let record = test_box.git_ok(
        repo,
        &[
            "cat-file",
            "blob",
            &format!("refs/bay/changes/{id}:change.json"),
        ],
    );
    serde_json::from_str::<Value>(&record).unwrap()["declared_inputs"].clone()
}

/// A change on `path` with `file` in it, merged; main moves.
fn land(test_box: &TestBox, repo: &Path, card: &str, path: &str, file: (&str, &str)) {
    let started = test_box.bay_ok(repo, &["start", card, "--paths", path]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(file.0), file.1).unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
}

#[test]
fn a_path_with_a_leading_slash_is_declared_from_the_root() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[
            ("src/a.rs", "fn a() {}\n"),
            ("docs/a.md", "a\n"),
            ("lib/a.rs", "fn l() {}\n"),
        ],
        &check_files("true", &passing_result(1), 1, 60),
    );
    // GIVEN a change started on /src/, with work in its view
    let started = test_box.bay_ok(&repo, &["start", "CARD-942", "--paths", "/src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    // THEN its record declares src/, as git names it
    assert_eq!(declared(&test_box, &repo, &id), json!(["src/"]));

    // WHEN main moves, and the change is picked up with /docs/ too
    land(
        &test_box,
        &repo,
        "CARD-942-lib",
        "lib/",
        ("lib/b.rs", "fn b() {}\n"),
    );
    let (code, widened) =
        result_json(
            test_box
                .bay(&repo)
                .args(["start", &id, "--paths", "/src/", "//./docs/"]),
        );
    // THEN the pick-up brings main in and declares docs/
    assert_eq!(code, 0, "{widened}");
    assert_eq!(widened["change"], id.as_str(), "{widened}");
    assert_eq!(
        widened["detail"]["paths_added"],
        json!(["docs/"]),
        "{widened}"
    );
    assert_eq!(declared(&test_box, &repo, &id), json!(["src/", "docs/"]));
    assert!(view.join("docs/a.md").exists(), "{widened}");

    // AND a later pick-up after main moved again, with a path it covers, adds none
    land(
        &test_box,
        &repo,
        "CARD-942-lib-2",
        "lib/",
        ("lib/c.rs", "fn c() {}\n"),
    );
    let (code, again) =
        result_json(
            test_box
                .bay(&repo)
                .args(["start", &id, "--paths", "/docs/a.md"]),
        );
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["detail"]["paths_added"], json!([]), "{again}");

    // AND the change merges, with its work and a file under docs/
    std::fs::write(view.join("docs/b.md"), "b\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert_eq!(test_box.trunk_change_ids(&remote).first(), Some(&id));
    let merged = test_box.git_bare(&remote, &["show", "main:docs/b.md"]);
    assert_eq!(merged.trim(), "b", "{landed}");
}

#[test]
fn a_path_with_a_parent_part_is_refused_before_anything_is_written() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let changes = || test_box.git_bare(&remote, &["for-each-ref", "refs/bay/changes/"]);
    let before = changes();
    // WHEN a start names a path with a `..` part
    let out = test_box
        .bay(&repo)
        .args(["start", "CARD-942-up", "--paths", "src/", "../outside"])
        .output()
        .unwrap();
    // THEN it is a usage error that names the path, and no change is written
    let said = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{said}");
    assert!(said.contains("../outside"), "{said}");
    assert_eq!(changes(), before, "{said}");
}
