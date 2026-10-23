//! G325 (the 0.3.0 vocabulary table, owner ruling 2026-10-22): one name per idea, and no
//! removed name is still accepted.
//!
//! bay has no users yet, so 0.3.0 removes each old name instead of keeping it as a hidden
//! alias: a removed command or flag is a usage error (exit 2), and the contract, `bay help
//! --json`, lists no removed reason code, decision or result field. A second name for one idea
//! is a second thing to learn (`AGENTS.md`, one place, one truth).
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/main.rs` `Command`: give `Merge` the alias `#[command(alias = "land")]` (`bay land`
//!   parses).
//! - `src/main.rs` `Command::Status`: give `branches` the alias `#[arg(alias = "stranded")]`
//!   (`status --stranded` parses).
//! - `src/result.rs` `Decision::word`: `"merged"` back to `"landed"` (the contract lists it).
//! - `src/contract.rs` `schema`: put back the `published` property.

use crate::support;

use serde_json::Value;
use support::TestBox;

/// Each removed command line, with what replaced it.
const REMOVED: &[(&[&str], &str)] = &[
    (&["land"], "bay merge"),
    (&["watch"], "bay status --since"),
    (&["executor", "run", "--once"], "bay runner run"),
    (&["adapter", "test", "check"], "bay plugin test"),
    (&["start", "--resume", "abcd2345"], "bay start <change>"),
    (&["start", "CARD-1", "--intent-revision", "2"], "nothing"),
    (&["merge", "--abandon"], "bay abandon"),
    (&["merge", "--observe", "5"], "bay merge --wait=5"),
    (&["merge", "--change", "abcd2345"], "bay merge <change>"),
    (&["approve", "abcd2345", "--reject"], "bay reject <change>"),
    (&["status", "--stranded"], "bay status --branches"),
    (&["status", "--heads", "--prune"], "bay prune"),
    // 0.4.0 (BACKLOG 53, 51 R7).
    (&["status", "--heads"], "bay status --branches"),
    (&["status", "--fleet"], "bay status --all"),
    (&["enrol", "executor", "r1", "k.pub"], "bay enrol runner"),
    (
        &["doctor", "--role", "executor"],
        "bay doctor --role runner",
    ),
    (&["host", "init", "h.git"], "bay host h.git"),
    (
        &["status", "--since", "--landed"],
        "bay status --since --merged",
    ),
    (&["install", "--trunk", "main"], "bay install --main"),
    (
        &["host", "init", "h.git", "--trunk", "main"],
        "bay host --main",
    ),
    (&["update", "--channel", "stable"], "nothing"),
];

/// Reason codes 0.3.0 renamed or folded.
const REMOVED_CODES: &[&str] = &[
    "TRUNK_MOVED",
    "TRUNK_RED",
    "TRUNK_HELD",
    "CHANGE_NOT_LANDED",
    "CHANGE_ALREADY_LANDED",
    "PUBLISH_RETRY_EXHAUSTED",
    "PUBLISH_OUTCOME_UNKNOWN",
    "QUEUE_HELD",
    "PUBLICATION_QUEUED",
    "GIT_BELOW_FLOOR",
    "CLAUDE_BELOW_FLOOR",
    "AGENT_BELOW_FLOOR",
    "BAY_BELOW_FLOOR",
    "CLAUDE_ABSENT",
    "AGENT_ABSENT",
    "TALK_INVALID",
    "TALK_BODY_CONFLICT",
    "TALK_CLOSED",
    "TALK_SUPERSEDE_REFUSED",
    "TALK_ANSWER_CONFLICT",
    "TALK_SENDER_NOT_ACTOR",
    "TALK_BACKLOG",
    "LANE_NEVER_LANDS",
    "UNATTRIBUTED_LANDING",
    "EXECUTOR_KEY_MISSING",
    "WORKSPACE_INCOMPLETE",
];

#[test]
fn every_removed_command_and_flag_is_a_usage_error() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    for (args, instead) in REMOVED {
        let out = test_box.bay(&repo).args(*args).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "bay {} is still accepted; it is {instead} now: {out:?}",
            args.join(" ")
        );
    }
}

#[test]
fn the_contract_lists_no_removed_code_decision_or_field() {
    let test_box = TestBox::new();
    // `bay_ok` adds `--json`: this is `bay help --json`.
    let contract = test_box.bay_ok(test_box.path(), &["help"]);
    let codes: Vec<&str> = contract["detail"]["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|reason| reason["code"].as_str())
        .collect();
    for code in REMOVED_CODES {
        assert!(!codes.contains(code), "the contract still lists {code}");
    }
    for code in [
        "MAIN_MOVED",
        "QUEUED",
        "TOOL_BELOW_FLOOR",
        "TALK_REFUSED",
        "OUTSIDE_MERGE",
    ] {
        assert!(codes.contains(&code), "the contract lists no {code}");
    }
    let text = contract.to_string();
    assert!(text.contains("\"merged\""), "no decision merged");
    let schema: &Value = &contract["detail"]["schema"];
    let properties = schema["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("no schema properties: {schema}"));
    for field in ["published", "next", "receipts"] {
        assert!(
            !properties.contains_key(field),
            "bay.result/1 still has {field}"
        );
    }
    assert!(properties.contains_key("evidence"), "{schema}");
    assert_eq!(
        schema["properties"]["decision"]["enum"],
        serde_json::json!(["ok", "merged", "queued", "blocked", "failed"]),
        "{schema}"
    );
}

/// BACKLOG 55 (c), owner ruling 2026-10-22: the config names the branch bay serves `main`, and
/// the old key `trunk` is a key bay does not know.
///
/// Guard to mutate: `src/config.rs` `Config`: `#[serde(rename = "main")]` to
/// `#[serde(alias = "trunk", rename = "main")]` (the old key loads).
#[test]
fn the_config_key_trunk_is_unknown() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    test_box.commit_config(&repo, "trunk = \"main\"\n");
    let (code, result) = support::result_json(test_box.bay(&repo).arg("status"));
    assert_ne!(code, 0, "the old key loaded: {result}");
    assert!(result.to_string().contains("trunk"), "{result}");
    test_box.commit_config(&repo, "main = \"main\"\n");
    let (code, result) = support::result_json(test_box.bay(&repo).arg("status"));
    assert_eq!(code, 0, "{result}");
}
