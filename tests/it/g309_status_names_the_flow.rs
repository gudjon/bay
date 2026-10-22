//! G309. `bay status` names the flow of work.
//!
//! `detail.flow` is derived on read from the change chains this repository holds, never
//! stored: `live`, the changes that have not ended, as the WIP limit counts them, and
//! `by_actor`, each actor's live changes beside the policy's `[agent] max_live_changes`; `landed`,
//! the changes landed in the last 7 days; `cycle_time_s`, the p50 and p90 from start to landing
//! of those landed in the last 30 (null when none); `oldest_live`, the live change that started
//! first and its age; and `waiting_approval`, how many live changes wait for a person's approval.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/flow.rs` `flow`: `.filter(|held| crate::overlap::counts_live(held, &landed))` (drop
//!   the filter: the landed change counts as live, and `live` is 3);
//! - `src/flow.rs` `flow`: `.filter(|(_, at)| at.saturating_add(LANDED_DAYS * DAY_S) >= now)`
//!   (make it `.filter(|_| false)`: `landed` is 0);
//! - `src/flow.rs` `flow`: `.filter(|(_, at)| at.saturating_add(CYCLE_DAYS * DAY_S) >= now)`
//!   (make it `.filter(|_| false)`: `cycle_time_s` is null);
//! - `src/status.rs` `run`: `"flow": flow,` (drop it: there is no `detail.flow`).

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use support::{TestBox, check_files, passing_result, result_json};

/// A repository whose trunk holds `src/a.rs`, one passing check, and a WIP limit of 3.
fn seeded(test_box: &TestBox) -> PathBuf {
    let repo = test_box.repo_any("repo");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str("\n[agent]\nmax_live_changes = 3\n");
        }
    }
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    repo
}

/// `bay start <intent> --paths src/` as `actor`, writing `file` in its view: the change id and
/// its view.
fn start(
    test_box: &TestBox,
    repo: &Path,
    actor: &str,
    intent: &str,
    file: &str,
) -> (String, PathBuf) {
    let (code, started) = result_json(
        test_box
            .bay(repo)
            .env("BAY_ACTOR", actor)
            .args(["start", intent, "--paths", "src/"]),
    );
    assert_eq!(code, 0, "{started}");
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(file), "fn more() {}\n").unwrap();
    (id, view)
}

#[test]
fn status_counts_live_by_actor_landings_cycle_time_and_the_oldest_live_change() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);

    let (_landing, landing_view) = start(&test_box, &repo, "alice", "CARD-1", "src/one.rs");
    let (older, _) = start(&test_box, &repo, "alice", "CARD-2", "src/two.rs");
    // Committer times have one-second resolution: the next change starts a clear second later.
    std::thread::sleep(Duration::from_millis(1100));
    let (_newer, _) = start(&test_box, &repo, "bob", "CARD-3", "src/three.rs");

    let (code, landed) = result_json(
        test_box
            .bay(&landing_view)
            .env("BAY_ACTOR", "alice")
            .args(["land", "--wait"]),
    );
    assert_eq!(code, 0, "{landed}");

    let status = test_box.bay_ok(&repo, &["status"]);
    let flow = &status["detail"]["flow"];
    assert_eq!(flow["live"], 2, "{flow}");
    assert_eq!(
        flow["by_actor"],
        json!([
            {"actor": "alice", "live": 1, "limit": 3},
            {"actor": "bob", "live": 1, "limit": 3},
        ]),
        "{flow}"
    );
    assert_eq!(flow["landed"], 1, "{flow}");

    let cycle = &flow["cycle_time_s"];
    assert_eq!(cycle["landed"], 1, "{flow}");
    let p50 = cycle["p50"].as_u64().unwrap_or_else(|| panic!("{flow}"));
    let p90 = cycle["p90"].as_u64().unwrap_or_else(|| panic!("{flow}"));
    assert!(p50 <= p90, "{flow}");
    // One landing: both percentiles are its cycle time, well under the test's own run.
    assert_eq!(p50, p90, "{flow}");
    assert!(p50 < 600, "{flow}");

    assert_eq!(flow["oldest_live"]["change"], older.as_str(), "{flow}");
    assert!(flow["oldest_live"]["age_s"].as_u64().is_some(), "{flow}");
    assert_eq!(
        flow["waiting_approval"],
        json!({"count": 0, "oldest": Value::Null, "age_s": Value::Null}),
        "{flow}"
    );
}

#[test]
fn with_nothing_landed_there_is_no_cycle_time() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    start(&test_box, &repo, "alice", "CARD-1", "src/one.rs");

    let status = test_box.bay_ok(&repo, &["status"]);
    let flow = &status["detail"]["flow"];
    assert_eq!(flow["live"], 1, "{flow}");
    assert_eq!(flow["landed"], 0, "{flow}");
    assert_eq!(flow["cycle_time_s"], Value::Null, "{flow}");
}
