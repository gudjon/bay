//! G335 (owner ruling 2026-10-23): no key and no state, class or event a result prints says an
//! old word.
//!
//! 0.3.0 renamed the ideas; 0.3.5 renames what results still printed with the old words:
//! `freshness.trunk` is `freshness.main`, `detail.landed` is `detail.merged`, a change's state
//! `landed` is `merged`, `waiting_for: "executor"` is `"runner"`, the events `trunk_red`,
//! `trunk_not_linear` and `unattributed_landing` are `main_red`, `main_not_linear` and
//! `outside_merge`, and so on. Records stored in Git keep their names; only what is printed is
//! held here.
//!
//! Guard to mutate: `src/result.rs` `BayResult::serialize`: `self.detail.as_ref().map(printed)`
//! to `self.detail.clone()` (the detail prints its stored names).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// Parts of a key that are old words.
const OLD_KEY_PARTS: &[&str] = &[
    "trunk",
    "landed",
    "landing",
    "executor",
    "published",
    "publish",
];

/// Values a state, class, row name or event no longer prints.
const OLD_VALUES: &[&str] = &[
    "landed",
    "published",
    "trunk",
    "executor",
    "runner_capacity",
    "main-hook",
    "main_red",
    "main_red_cleared",
    "main_not_linear",
    "outside_merge",
    "merged_on_your_paths",
    "no_live_runner",
];

const VALUE_KEYS: &[&str] = &[
    "recorded_state",
    "receipt",
    "requires",
    "require",
    "missing",
    "state",
    "class",
    "kind",
    "event",
    "type",
    "outcome",
    "waiting_for",
    "warning",
    "warnings",
];

fn old_words(value: &Value, path: &str, under: Option<&str>, found: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            for (key, inner) in object {
                if key.split('_').any(|part| OLD_KEY_PARTS.contains(&part)) {
                    found.push(format!("{path}.{key}"));
                }
                old_words(inner, &format!("{path}.{key}"), Some(key), found);
            }
        }
        Value::Array(items) => {
            for (n, item) in items.iter().enumerate() {
                old_words(item, &format!("{path}[{n}]"), under, found);
            }
        }
        Value::String(word)
            if under.is_some_and(|key| VALUE_KEYS.contains(&key))
                && OLD_VALUES.contains(&word.as_str()) =>
        {
            found.push(format!("{path} = {word:?}"));
        }
        // An id that begins with an old event kind (`trunk_not_linear:<sha>`).
        Value::String(word)
            if under.is_some_and(|key| key == "id" || key == "recurrence_key")
                && OLD_VALUES
                    .iter()
                    .any(|old| word.starts_with(&format!("{old}:"))) =>
        {
            found.push(format!("{path} = {word:?}"));
        }
        _ => {}
    }
}

#[test]
fn every_result_prints_the_new_words() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let runner = test_box.clone_of(&remote, "runner");
    let (id, view) = test_box.start_and_edit(&repo);
    let (_, queued) = result_json(test_box.bay(&view).arg("merge"));
    let ran = test_box.bay_ok(&runner, &["runner", "run", "--once"]);
    let mut results = vec![("merge", queued), ("runner run", ran)];
    for args in [
        &["status"][..],
        &["status", id.as_str()][..],
        &["status", "--since"][..],
        &["status", "--since", "--merged"][..],
        &["status", "--heads"][..],
        &["status", "--fleet"][..],
        &["doctor"][..],
    ] {
        let (_, result) = result_json(test_box.bay(&repo).args(args));
        results.push((args.join(" ").leak(), result));
    }
    let mut found = Vec::new();
    for (name, result) in &results {
        old_words(result, &format!("bay {name}"), None, &mut found);
    }
    assert!(found.is_empty(), "printed old words:\n{}", found.join("\n"));
    let change = &results[3].1["detail"]["changes"][0];
    assert_eq!(change["state"], "merged", "{}", results[3].1);
}
