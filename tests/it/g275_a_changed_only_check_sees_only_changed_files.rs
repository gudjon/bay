//! G275. A changed-only check sees only the files its candidate changed.
//!
//! A `[[check]]` with `materialize = "changed"` gets a candidate tree that holds only the
//! paths the candidate's diff against its base changes (the request's `changed_paths`), with
//! the entry's `paths` when it lists them: a check that reads `.bay/` and the changed files
//! gets both, and a large tree is never written whole for it. A deleted path is simply absent,
//! so a deletion-only change materializes nothing and the check still runs. The request says
//! `"materialized": "changed"`, so the adapter knows its tree is partial. Without the key the
//! check gets the whole tree, as before. A changed-only record never answers another changed
//! set, so the policy refuses it beside `footprint = true` and on an approval check.
//!
//! Guards to mutate, each of which turns a case red:
//! - `src/check.rs` `prepare`: `let kept = entry.materialized(candidate.changed_paths);` back to
//!   `entry.paths.clone()` (the changed-only check sees the whole tree).
//! - `src/policy.rs` `CheckEntry::materialized`: drop `.chain(self.paths.iter().flatten())`
//!   (the `.bay/` extras go missing).
//! - `src/check.rs` `request`: drop `request["materialized"] = json!("changed");`.
//! - `src/policy.rs` `validate_footprint`: drop the `entry.footprint && entry.changed_only()`
//!   refusal; `validate_kind`: drop the `("materialize", ...)` row.

// Each case runs a check adapter (a shell script), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// The files of the seeded trunk besides the adapter and `.bay/`.
const FILES: &[(&str, &str)] = &[
    ("src/one.txt", "one\n"),
    ("src/two.txt", "two\n"),
    ("src/three.txt", "three\n"),
    ("docs/guide.md", "guide\n"),
    ("docs/faq.md", "faq\n"),
    ("assets/big/a.bin", "a\n"),
    ("assets/big/b.bin", "b\n"),
    ("tools/build.sh", "build\n"),
];

/// What one run of the check saw: whether the request said its tree is partial, the files in
/// its tree (sorted), and the request.
struct Seen {
    part: String,
    files: Vec<String>,
    request: Value,
}

/// A trunk with [`FILES`] whose one check `tests` (its `[[check]]` table ends with `extra`)
/// logs what it saw on each run, through the relay, and passes.
fn trunk(test_box: &TestBox, extra: &str) -> (PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    let log = test_box.path().join("seen.log");
    let relay = test_box.relay(&format!("printf '%s\\n' \"$req\" >> '{}'", log.display()));
    let before = String::new()
        + "seen=$(find . -type f | sed 's|^\\./||' | LC_ALL=C sort | tr '\\n' ' ')\n"
        + "case \"$req\" in *'\"materialized\":\"changed\"'*) part=changed ;; *) part=whole ;; esac\n"
        + "req=\"$part|$seen|$(printf '%s' \"$req\" | tr -d '\\n')\"\n"
        + &relay;
    let files: Vec<(String, String)> = check_files(&before, &passing_result(1), 1, 60)
        .into_iter()
        .map(|(path, text)| {
            if path == ".bay/policy.toml" {
                (path, format!("{text}{extra}"))
            } else {
                (path, text)
            }
        })
        .collect();
    test_box.seed_with_check(&repo, FILES, &files);
    (repo, log)
}

/// Every run the check logged.
fn seen(log: &Path) -> Vec<Seen> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|line| {
            let mut parts = line.splitn(3, '|');
            let part = parts.next().unwrap().to_owned();
            let files = parts
                .next()
                .unwrap()
                .split_whitespace()
                .map(str::to_owned)
                .collect();
            let request = serde_json::from_str(parts.next().unwrap())
                .unwrap_or_else(|error| panic!("{error}: {line}"));
            Seen {
                part,
                files,
                request,
            }
        })
        .collect()
}

/// Starts a change on `src/` and returns its view.
fn start(test_box: &TestBox, repo: &Path, intent: &str) -> PathBuf {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", "src/"]);
    PathBuf::from(started["detail"]["view"].as_str().unwrap())
}

/// Lands the change in `view` and asserts it landed.
fn land(test_box: &TestBox, view: &Path) {
    let (code, landed) = result_json(test_box.bay(view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
}

/// The one run the check made, or every run when it made several: each saw the same.
fn only_run(log: &Path) -> Seen {
    let mut runs = seen(log);
    assert!(!runs.is_empty(), "the check never ran");
    let first = runs.remove(0);
    for run in &runs {
        assert_eq!(run.files, first.files);
        assert_eq!(run.part, first.part);
    }
    first
}

#[test]
fn a_changed_only_check_sees_only_the_file_the_change_touched() {
    let test_box = TestBox::new();
    let (repo, log) = trunk(&test_box, "materialize = \"changed\"\n");
    let view = start(&test_box, &repo, "ONE-1");
    std::fs::write(view.join("src/one.txt"), "one, changed\n").unwrap();
    land(&test_box, &view);
    let run = only_run(&log);
    assert_eq!(run.files, ["src/one.txt"], "{}", run.request);
    assert_eq!(run.part, "changed", "{}", run.request);
    assert_eq!(run.request["materialized"], "changed", "{}", run.request);
    assert_eq!(
        run.request["changed_paths"],
        serde_json::json!(["src/one.txt"]),
        "{}",
        run.request
    );
}

#[test]
fn a_changed_only_check_also_sees_the_paths_it_lists() {
    let test_box = TestBox::new();
    let (repo, log) = trunk(
        &test_box,
        "materialize = \"changed\"\npaths = [\".bay/\"]\n",
    );
    let view = start(&test_box, &repo, "ONE-2");
    std::fs::write(view.join("src/one.txt"), "one, changed\n").unwrap();
    land(&test_box, &view);
    let run = only_run(&log);
    assert_eq!(
        run.files,
        [".bay/config.toml", ".bay/policy.toml", "src/one.txt"],
        "{}",
        run.request
    );
    assert_eq!(run.request["materialized"], "changed", "{}", run.request);
}

#[test]
fn without_the_key_the_check_sees_the_whole_tree() {
    let test_box = TestBox::new();
    let (repo, log) = trunk(&test_box, "");
    let view = start(&test_box, &repo, "ONE-3");
    std::fs::write(view.join("src/one.txt"), "one, changed\n").unwrap();
    land(&test_box, &view);
    let run = only_run(&log);
    for (path, _) in FILES {
        assert!(
            run.files.iter().any(|file| file == path),
            "{path}: {:?}",
            run.files
        );
    }
    for path in [".bay/policy.toml", "scripts/bay-check"] {
        assert!(
            run.files.iter().any(|file| file == path),
            "{path}: {:?}",
            run.files
        );
    }
    assert_eq!(run.part, "whole");
    assert!(run.request.get("materialized").is_none(), "{}", run.request);
}

#[test]
fn a_deletion_only_change_materializes_nothing_and_the_check_still_runs() {
    let test_box = TestBox::new();
    let (repo, log) = trunk(&test_box, "materialize = \"changed\"\n");
    let view = start(&test_box, &repo, "GONE-1");
    std::fs::remove_file(view.join("src/two.txt")).unwrap();
    land(&test_box, &view);
    let run = only_run(&log);
    assert!(run.files.is_empty(), "{:?}", run.files);
    assert_eq!(run.request["materialized"], "changed", "{}", run.request);
    assert_eq!(
        run.request["changed_paths"],
        serde_json::json!(["src/two.txt"]),
        "{}",
        run.request
    );
}

#[test]
fn a_changed_only_check_with_a_footprint_or_on_an_approval_makes_the_policy_invalid() {
    for extra in [
        "materialize = \"changed\"\nwhen_paths = [\"src/**\"]\nfootprint = true\n",
        "\n[[check]]\nid = \"sign-off\"\nenvironment = \"people\"\nkind = \"approval\"\nwhen_paths = [\"**\"]\nmaterialize = \"changed\"\n",
    ] {
        let test_box = TestBox::new();
        let (repo, _) = trunk(&test_box, extra);
        let view = start(&test_box, &repo, "BAD-1");
        std::fs::write(view.join("src/one.txt"), "one, changed\n").unwrap();
        let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
        assert_eq!(code, 10, "{extra}: {refused}");
        assert_eq!(refused["reason"], "POLICY_INVALID", "{extra}: {refused}");
        let why = refused["detail"]["why"].as_str().unwrap_or_default();
        assert!(why.contains("materialize"), "{extra}: {refused}");
    }
}
