//! G285. A box read path is readable in a check, and never writable.
//!
//! A box names directories its checks may read with the multi-valued Git config
//! `bay.checkReadPath` (a package store, say): `git config --global --add bay.checkReadPath
//! ~/.store`. Each value is absolute or starts with `~/`, for the executor's `HOME`. The sandbox
//! binds each one read-only: a check adapter reads a file there, and a write there fails.
//! Without the setting the path is outside the allow-list, so the read fails as it always did
//! (G62). A relative value refuses the check, and its record names the value. The paths are box
//! facts: they never enter the definition digest, and the record of a sandboxed run lists them
//! in `sandbox_read`, next to `sandbox`. `bay doctor` lists them in its `check-read-paths` row,
//! and a path that does not exist is degraded.
//!
//! The guards to mutate: in `src/check.rs`, `run_one` adds the box's paths to the run's
//! read-only list with `read_only.extend(place.read.iter()...)` (drop it, and the read fails
//! in the configured case); `src/check/sandbox.rs` `bwrap_args` binds each `read_only`
//! directory with `"--ro-bind"` (make it `"--bind"`, and the write lands); `read_path` refuses
//! a value with `if path.is_absolute()` (make it `if true`, and the relative case is not
//! refused by name); `evidence` inserts `SANDBOX_READ` (drop it, and the record does not list
//! the path); `doctor::read_paths_row` sets `"degraded"` for a missing path. The sandboxed
//! cases need the check sandbox; on a machine without one they are skipped, unless
//! `BAY_REQUIRE_SANDBOX` is set.

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json, row};

/// Every evidence record in `repo`'s notes, as one text.
fn records(test_box: &TestBox, repo: &Path) -> String {
    let notes = test_box.git_ok(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/notes/bay/evidence/",
        ],
    );
    notes
        .lines()
        .flat_map(|notes_ref| {
            test_box
                .git_ok(
                    repo,
                    &["ls-tree", "-r", "--format=%(objectname)", notes_ref],
                )
                .lines()
                .map(|blob| test_box.git_ok(repo, &["cat-file", "blob", blob]))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The store in the executor's `HOME`, with one file the check reads.
fn store(test_box: &TestBox) -> PathBuf {
    let store = test_box.home.join(".store");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("file.txt"), "stored\n").unwrap();
    store
}

/// A trunk whose one check passes only when it reads `<store>/file.txt` and cannot write in
/// `store`; otherwise it reports a product failure. Returns (repo, view).
fn submitted(test_box: &TestBox, store: &Path) -> (PathBuf, PathBuf) {
    let file = store.join("file.txt");
    let written = store.join("written.txt");
    let before = format!(
        "if [ \"$(cat '{}' 2>/dev/null)\" = stored ]; then read=yes; else read=no; fi\n\
         if ( echo x > '{}' ) 2>/dev/null; then wrote=yes; else wrote=no; fi",
        file.display(),
        written.display()
    );
    let pass = passing_result(1);
    let fail = pass.replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let result =
        format!("if [ \"$read\" = yes ] && [ \"$wrote\" = no ]; then {pass}; else {fail}; fi");
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files(&before, &result, 1, 60),
    );
    let (_, view) = test_box.start_and_edit(&repo);
    (repo, view)
}

fn add_read_path(test_box: &TestBox, value: &str) {
    test_box.git_ok(
        test_box.path(),
        &["config", "--global", "--add", "bay.checkReadPath", value],
    );
}

#[test]
fn a_configured_read_path_is_readable_and_never_writable() {
    let Some(sandbox) = support::sandbox() else {
        eprintln!("no check sandbox on this machine; G285 cannot run");
        return;
    };
    let test_box = TestBox::new();
    let store = store(&test_box);
    // `~/` is the executor's HOME.
    add_read_path(&test_box, "~/.store");
    let (repo, view) = submitted(&test_box, &store);
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(
        code, 0,
        "the check could not read the box read path, or wrote it: {landed}"
    );
    assert_eq!(landed["decision"], "landed", "{landed}");
    assert!(
        !store.join("written.txt").exists(),
        "the check wrote the store"
    );
    assert_eq!(
        std::fs::read_to_string(store.join("file.txt")).unwrap(),
        "stored\n"
    );
    let records = records(&test_box, &repo);
    assert!(
        records.contains(&format!("\"sandbox\": \"{sandbox}\"")),
        "{records}"
    );
    let listed = serde_json::to_string(&store.display().to_string()).unwrap();
    assert!(
        records.contains("\"sandbox_read\"") && records.contains(&listed),
        "the record does not list the read path {listed}: {records}"
    );
}

#[test]
fn without_the_setting_the_read_fails_as_before() {
    let Some(_sandbox) = support::sandbox() else {
        eprintln!("no check sandbox on this machine; G285 cannot run");
        return;
    };
    let test_box = TestBox::new();
    let store = store(&test_box);
    let (repo, view) = submitted(&test_box, &store);
    let (code, blocked) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "the check read outside its allow-list: {blocked}");
    assert_eq!(blocked["reason"], "CHECK_FAILED", "{blocked}");
    assert!(!records(&test_box, &repo).contains("\"sandbox_read\""));
}

#[test]
fn a_relative_read_path_refuses_the_check_by_name() {
    let test_box = TestBox::new();
    let store = store(&test_box);
    add_read_path(&test_box, "relative/store");
    let (_repo, view) = submitted(&test_box, &store);
    let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(
        code, 10,
        "a relative read path did not refuse the check: {refused}"
    );
    assert_eq!(refused["reason"], "CHECK_UNKNOWN", "{refused}");
    assert_eq!(
        refused["detail"]["checks"][0]["verdict"], "infra_failure",
        "{refused}"
    );
    let text = refused.to_string();
    assert!(
        text.contains("bay.checkReadPath")
            && text.contains("relative/store")
            && text.contains("is not an absolute path"),
        "the refusal does not name the relative read path: {refused}"
    );
}

#[test]
fn doctor_lists_the_read_paths_and_a_missing_one_is_degraded() {
    let test_box = TestBox::new();
    let store = store(&test_box);
    let repo = test_box.repo_any("repo");
    // No setting: no row.
    let (_, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert!(
        result["detail"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["name"] != "check-read-paths"),
        "{result}"
    );
    add_read_path(&test_box, &store.display().to_string());
    let (_, result) = result_json(test_box.bay(&repo).arg("doctor"));
    let paths = row(&result, "check-read-paths");
    assert_eq!(paths["state"], "green", "{result}");
    assert_eq!(paths["paths"][0]["path"], store.display().to_string());
    assert_eq!(paths["paths"][0]["exists"], true, "{result}");
    let missing = test_box.path().join("absent");
    add_read_path(&test_box, &missing.display().to_string());
    let (_, result) = result_json(test_box.bay(&repo).arg("doctor"));
    let paths = row(&result, "check-read-paths");
    assert_eq!(paths["state"], "degraded", "{result}");
    assert_eq!(
        paths["missing"][0],
        missing.display().to_string(),
        "{result}"
    );
}
