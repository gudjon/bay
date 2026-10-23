//! G264 (N-11). An actor holds at most `max_live_changes` live changes.
//!
//! A policy at the base with `[agent] max_live_changes = N` makes `bay start` refuse
//! `WIP_LIMIT` (blocked, exit 10) while the actor already holds N live changes: not landed,
//! abandoned or superseded, a lane change among them. The detail names each live change, its
//! id and intent, and nothing is created. Another actor still starts; the actor's own live
//! change is still adopted; a change given up frees its place. With no key there is no limit.
//!
//! Guards to mutate: `crate::start::wip_limit`'s comparison
//! `if u64::try_from(live.len()).unwrap_or(u64::MAX) < limit {` (make it always return
//! `Ok(None)`: the refusal tests fail), and the `live(held.record.state)` term of
//! `crate::overlap::counts_live`, which `held_by` filters by (drop it: the abandoned change still counts and the
//! freed-place test fails).

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, UNCHECKED_POLICY, result_json};

/// A config with one lane, `research`, over `refs/heads/research/*`.
const LANES: &str = "main = \"main\"\n\n[[lane]]\nname = \"research\"\nrefs = [\"refs/heads/research/*\"]\nwindow_s = 3600\n";

/// A repository whose trunk declares the `research` lane and a policy with `agent`'s table.
fn seeded(test_box: &TestBox, agent: &str) -> PathBuf {
    let repo = test_box.repo_any("repo");
    let policy = format!("{UNCHECKED_POLICY}\n{agent}");
    test_box.seed_trunk(
        &repo,
        &[
            ("src/a.rs", "fn a() {}\n"),
            (".bay/config.toml", LANES),
            (".bay/policy.toml", &policy),
        ],
    );
    repo
}

/// `bay start <intent> --paths src/` as `actor`, with `extra` arguments: exit code and result.
fn start(
    test_box: &TestBox,
    repo: &Path,
    actor: &str,
    intent: &str,
    extra: &[&str],
) -> (i32, Value) {
    let mut args = vec!["start", intent, "--paths", "src/"];
    args.extend(extra);
    result_json(test_box.bay(repo).env("BAY_ACTOR", actor).args(&args))
}

/// [`start`], asserted to succeed: its result.
fn started(test_box: &TestBox, repo: &Path, actor: &str, intent: &str, extra: &[&str]) -> Value {
    let (code, result) = start(test_box, repo, actor, intent, extra);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["decision"], "ok", "{result}");
    result
}

/// How many change chains the repository holds.
fn chains(test_box: &TestBox, repo: &Path) -> usize {
    test_box
        .git_ok(
            repo,
            &["for-each-ref", "--format=%(refname)", "refs/bay/changes/"],
        )
        .lines()
        .count()
}

/// Asserts `refused` is `WIP_LIMIT` at `limit`, naming exactly `live` (change id, intent).
fn assert_wip_limit(refused: &Value, limit: u64, live: &[(&str, &str)]) {
    assert_eq!(refused["decision"], "blocked", "{refused}");
    assert_eq!(refused["reason"], "WIP_LIMIT", "{refused}");
    assert_eq!(refused["allowed"], serde_json::json!(["read"]), "{refused}");
    assert_eq!(
        refused["retry_when"], "a_live_change_lands_or_is_abandoned",
        "{refused}"
    );
    let detail = &refused["detail"];
    assert_eq!(detail["max_live_changes"], limit, "{refused}");
    let named = detail["live_changes"].as_array().unwrap();
    assert_eq!(named.len(), live.len(), "{refused}");
    for (id, intent) in live {
        assert!(
            named
                .iter()
                .any(|entry| entry["change"] == *id && entry["intent"] == *intent),
            "{id} {intent} not named: {refused}"
        );
    }
}

#[test]
fn an_actor_at_its_limit_starts_nothing_and_a_lane_change_counts() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box, "[agent]\nmax_live_changes = 2\n");

    let first = started(&test_box, &repo, "alice", "CARD-1", &[]);
    let first = first["change"].as_str().unwrap().to_owned();
    let lane = started(&test_box, &repo, "alice", "CARD-2", &["--lane", "research"]);
    assert_eq!(lane["detail"]["lane"], "research", "{lane}");
    let lane = lane["change"].as_str().unwrap().to_owned();
    assert_eq!(chains(&test_box, &repo), 2);

    let (code, refused) = start(&test_box, &repo, "alice", "CARD-3", &[]);
    assert_eq!(code, 10, "{refused}");
    assert_wip_limit(&refused, 2, &[(&first, "CARD-1"), (&lane, "CARD-2")]);
    // Nothing was created: no chain, no claim on the intent.
    assert_eq!(chains(&test_box, &repo), 2);

    // The limit is the actor's own: another actor starts.
    let theirs = started(&test_box, &repo, "bob", "CARD-3", &[]);
    assert!(theirs["change"].is_string(), "{theirs}");

    // The actor's own live change is adopted again at the limit: it holds no more than before.
    let again = started(&test_box, &repo, "alice", "CARD-1", &[]);
    assert_eq!(again["change"], first.as_str(), "{again}");
}

#[test]
fn a_change_given_up_frees_its_place() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box, "[agent]\nmax_live_changes = 1\n");

    let first = started(&test_box, &repo, "alice", "CARD-1", &[]);
    let id = first["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(first["detail"]["view"].as_str().unwrap());

    let (code, refused) = start(&test_box, &repo, "alice", "CARD-2", &[]);
    assert_eq!(code, 10, "{refused}");
    assert_wip_limit(&refused, 1, &[(&id, "CARD-1")]);

    let (code, abandoned) = result_json(
        test_box
            .bay(&view)
            .env("BAY_ACTOR", "alice")
            .args(["abandon"]),
    );
    assert_eq!(code, 0, "{abandoned}");

    let second = started(&test_box, &repo, "alice", "CARD-2", &[]);
    assert_ne!(second["change"], id.as_str(), "{second}");
}

#[test]
fn without_the_key_there_is_no_limit() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box, "");
    for intent in ["CARD-1", "CARD-2", "CARD-3", "CARD-4"] {
        started(&test_box, &repo, "alice", intent, &[]);
    }
    assert_eq!(chains(&test_box, &repo), 4);
}
