//! G295. Every refusal says its next step.
//!
//! What an agent does on a reason has one home: `Reason::next` in `src/result.rs`. Every refusal
//! carries it as `next_step`, right after `retry_when`, and `bay help --json` lists it with each
//! reason code, so the skill and any table of reasons are generated from the binary, never
//! copied. Each next step is one sentence of at most 25 words, and every command it names, with
//! its flags, is one `bay help --json` lists. Driven here through a failed check (also as `bay
//! status` shows its operation), a conflict, a superseded start and an approval still owed.
//!
//! Guards to mutate:
//! - `src/result.rs`, `impl Serialize for BayResult`: `next_step: self.reason.map(Reason::next),`
//!   (make it `next_step: None,`): no refusal carries `next_step`, and every driven case fails.
//! - `src/contract.rs` `reasons`: `"next_step": reason.next(),` (drop the line): the listing
//!   carries no next step, and the first case fails.
//! - `src/result.rs` `Reason::next`, the `Self::MergeConflict` arm: name a command bay does not
//!   have (`` `bay rebase` ``): the first case fails on the unknown command.
//! - `src/status.rs` `describe`: `"next_step": reason.map(Reason::next),` (drop the line): the
//!   failed check's operation in `bay status` carries no next step.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bay::result::Reason;
use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// An approval check for `billing/`, answered by records, never run.
const APPROVAL: &str = "\n[[check]]\nid = \"billing-approval\"\nenvironment = \"people\"\nkind = \"approval\"\nwhen_paths = [\"billing/\"]\n";

/// `bay help --json`: its detail.
fn contract(test_box: &TestBox) -> Value {
    let (code, result) = result_json(test_box.bay(test_box.path()).arg("help"));
    assert_eq!(code, 0, "{result}");
    result["detail"].clone()
}

/// Each reason code with the next step `bay help --json` lists for it.
fn listed_next_steps(contract: &Value) -> HashMap<String, String> {
    contract["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            let code = row["code"].as_str().unwrap().to_owned();
            let next = row["next_step"]
                .as_str()
                .unwrap_or_else(|| panic!("{code} lists no next_step: {row}"))
                .to_owned();
            (code, next)
        })
        .collect()
}

/// The `` `bay ...` `` spans of `sentence`, without the backticks.
fn commands(sentence: &str) -> Vec<&str> {
    sentence
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|span| span.starts_with("bay "))
        .collect()
}

/// `span` (`bay <command> [<sub>] [--flag ...]`) names a command and flags `contract` lists.
fn assert_known(contract: &Value, span: &str, code: &str) {
    let listed = contract["commands"].as_array().unwrap();
    let global: Vec<&str> = contract["global"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|arg| arg["long"].as_str())
        .collect();
    let find = |path: &str| listed.iter().find(|command| command["command"] == path);
    let mut words = span.split_whitespace().skip(1).peekable();
    // The longest listed command path the span starts with.
    let mut path = String::new();
    while let Some(word) = words.peek() {
        let longer = if path.is_empty() {
            (*word).to_owned()
        } else {
            format!("{path} {word}")
        };
        if find(longer.as_str()).is_none() {
            break;
        }
        path = longer;
        words.next();
    }
    let command =
        find(path.as_str()).unwrap_or_else(|| panic!("{code}: `{span}` names no bay command"));
    let flags: Vec<&str> = command["args"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|arg| arg["long"].as_str())
        .collect();
    for word in words.filter(|word| word.starts_with("--")) {
        let flag = word.split(['=', '[']).next().unwrap();
        assert!(
            flags.contains(&flag) || global.contains(&flag),
            "{code}: `bay {path}` has no {flag}: `{span}`"
        );
    }
}

/// `refusal` refuses for `code` and carries the next step the contract lists for it, right
/// after `retry_when`.
fn assert_next_step(refusal: &Value, code: &str, listed: &HashMap<String, String>) {
    assert_eq!(refusal["reason"], code, "{refusal}");
    assert_eq!(
        refusal["next_step"].as_str(),
        Some(listed[code].as_str()),
        "{refusal}"
    );
    let keys: Vec<&String> = refusal.as_object().unwrap().keys().collect();
    let at = keys.iter().position(|key| *key == "retry_when").unwrap();
    assert_eq!(keys.get(at + 1).map(|key| key.as_str()), Some("next_step"));
}

/// A repository whose trunk holds `src/a.rs` and `billing/rates.txt`, with one required check
/// that prints `result`, and the policy's text extended by `policy`.
fn seeded(test_box: &TestBox, result: &str, policy: &str) -> PathBuf {
    let repo = test_box.repo_any("repo");
    let mut check = check_files("true", result, 1, 60);
    for (path, text) in &mut check {
        if path == ".bay/policy.toml" {
            text.push_str(policy);
        }
    }
    test_box.seed_with_check(
        &repo,
        &[
            ("src/a.rs", "fn a() {}\n"),
            ("billing/rates.txt", "rate 1\n"),
        ],
        &check,
    );
    repo
}

/// `bay start <intent> --paths <paths>`: the change and its view.
fn start(test_box: &TestBox, repo: &Path, intent: &str, paths: &str) -> (String, PathBuf) {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", paths]);
    let change = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    (change, view)
}

#[test]
fn every_reason_lists_one_short_next_step_naming_real_commands() {
    let test_box = TestBox::new();
    let contract = contract(&test_box);
    let listed = listed_next_steps(&contract);
    assert_eq!(listed.len(), Reason::ALL.len(), "{listed:?}");
    for &reason in Reason::ALL {
        let code = reason.code();
        let next = &listed[code];
        assert_eq!(next, reason.next(), "{code}");
        assert!(!next.trim().is_empty(), "{code}: an empty next step");
        let count = next.split_whitespace().count();
        assert!(count <= 25, "{code}: {count} words: {next}");
        for span in commands(next) {
            assert_known(&contract, span, code);
        }
    }
}

#[test]
fn a_failed_check_and_its_operation_say_the_listed_next_step() {
    let test_box = TestBox::new();
    let listed = listed_next_steps(&contract(&test_box));
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let repo = seeded(&test_box, &failing, "");
    let (change, view) = start(&test_box, &repo, "CARD-1", "src/");
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();

    let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_next_step(&refused, "CHECK_FAILED", &listed);

    // `bay status` shows the operation's refusal with the same next step.
    let status = test_box.bay_ok(&repo, &["status", &change]);
    let operation = &status["detail"]["changes"][0]["operation"];
    assert_eq!(operation["outcome"], "CHECK_FAILED", "{status}");
    assert_eq!(
        operation["next_step"].as_str(),
        Some(listed["CHECK_FAILED"].as_str()),
        "{status}"
    );
}

#[test]
fn a_conflict_says_the_listed_next_step() {
    let test_box = TestBox::new();
    let listed = listed_next_steps(&contract(&test_box));
    let repo = seeded(&test_box, &passing_result(1), "");
    let remote = test_box.path().join("repo-remote.git");
    let (_, view) = start(&test_box, &repo, "CARD-1", "src/");
    std::fs::write(view.join("src/a.rs"), "fn a() { mine }\n").unwrap();
    let other = test_box.clone_of(&remote, "other");
    std::fs::write(other.join("src/a.rs"), "fn a() { theirs }\n").unwrap();
    test_box.git_ok(&other, &["commit", "-q", "-am", "theirs"]);
    test_box.git_ok(&other, &["push", "-q", "origin", "main"]);

    let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_next_step(&refused, "MERGE_CONFLICT", &listed);
}

#[test]
fn a_superseded_start_says_the_listed_next_step() {
    let test_box = TestBox::new();
    let listed = listed_next_steps(&contract(&test_box));
    let repo = seeded(&test_box, &passing_result(1), "");
    start(&test_box, &repo, "CARD-9", "src/");

    // A next generation of the intent before the first one landed is refused.
    let (code, refused) = result_json(
        test_box
            .bay(&repo)
            .args(["start", "CARD-9", "--new", "--paths", "src/"]),
    );
    assert_eq!(code, 10, "{refused}");
    assert_next_step(&refused, "CHANGE_SUPERSEDED", &listed);
}

#[test]
fn an_approval_still_owed_says_the_listed_next_step() {
    let test_box = TestBox::new();
    let listed = listed_next_steps(&contract(&test_box));
    let repo = seeded(&test_box, &passing_result(1), APPROVAL);
    let (_, view) = start(&test_box, &repo, "CARD-billing", "billing/");
    std::fs::write(view.join("billing/discount.txt"), "discount 1\n").unwrap();

    let (code, waiting) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{waiting}");
    assert_next_step(&waiting, "APPROVAL_PENDING", &listed);
}
