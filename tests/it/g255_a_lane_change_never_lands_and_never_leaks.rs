//! G255 (NEW-4). A sandbox lane's work never reaches trunk.
//!
//! A change started with `bay start --lane research` runs its checks against the current trunk
//! when it asks to land, holds no queue place, and is refused `LANE_NEVER_LANDS` with the
//! verdicts; trunk does not move. A change of no lane whose view merged a commit of a research
//! branch is refused `LANE_LEAK`, naming the lane and the commit, while a plain change lands. On
//! a forge, a merge of a research branch made outside bay is named under `unattributed` with
//! the lane it leaked (`lane_leak`), in `bay status`, the fleet map and `bay watch`.

// Each case runs a check adapter (a shell script), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::{TestBox, check_files, passing_result, result_json};

/// A config with one lane, `research`, over `refs/heads/research/*`.
const LANES: &str = "trunk = \"main\"\n\n[[lane]]\nname = \"research\"\nrefs = [\"refs/heads/research/*\"]\nwindow_s = 3600\n";

/// A repository whose trunk declares the `research` lane and one required check that passes.
fn seeded(test_box: &TestBox) -> PathBuf {
    let repo = test_box.repo_any("repo");
    let mut check = check_files("", &passing_result(1), 1, 60);
    check.push((".bay/config.toml".to_owned(), LANES.to_owned()));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &check);
    repo
}

/// The remote's tip of `name`, or `None`.
fn remote_tip(test_box: &TestBox, repo: &Path, name: &str) -> Option<String> {
    let listed = test_box.git_ok(repo, &["ls-remote", "origin", name]);
    listed.split_whitespace().next().map(str::to_owned)
}

/// A research branch `research/x` on the remote: one commit on `main` that adds
/// `src/research.rs`. `repo` is left on `main`.
fn research_branch(test_box: &TestBox, repo: &Path) -> String {
    test_box.git_ok(repo, &["switch", "-q", "-c", "research/x", "main"]);
    std::fs::write(repo.join("src/research.rs"), "fn research() {}\n").unwrap();
    test_box.git_ok(repo, &["add", "src/research.rs"]);
    test_box.git_ok(repo, &["commit", "-q", "-m", "research"]);
    let research = test_box.git_ok(repo, &["rev-parse", "HEAD"]);
    test_box.git_ok(repo, &["push", "-q", "origin", "research/x"]);
    test_box.git_ok(repo, &["switch", "-q", "main"]);
    research
}

#[test]
fn a_lane_change_runs_its_checks_and_is_refused_lane_never_lands_with_its_verdicts() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    let before = remote_tip(&test_box, &repo, "refs/heads/main").unwrap();

    // A lane the config does not declare starts nothing.
    let (code, unknown) = result_json(test_box.bay(&repo).args([
        "start",
        "CARD-nolane",
        "--paths",
        "src/",
        "--lane",
        "nope",
    ]));
    assert_eq!(code, 20, "{unknown}");
    assert!(
        unknown["detail"].as_str().unwrap().contains("nope"),
        "{unknown}"
    );

    let started = test_box.bay_ok(
        &repo,
        &[
            "start",
            "CARD-lane",
            "--paths",
            "src/",
            "--lane",
            "research",
        ],
    );
    assert_eq!(started["detail"]["lane"], "research", "{started}");
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();

    let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["decision"], "blocked", "{refused}");
    assert_eq!(refused["reason"], "LANE_NEVER_LANDS", "{refused}");
    assert_eq!(refused["allowed"], json!(["read"]), "{refused}");
    assert_eq!(refused["retry_when"], "never", "{refused}");
    assert_eq!(refused["published"], false, "{refused}");
    let detail = &refused["detail"];
    assert_eq!(detail["lane"], "research", "{refused}");
    assert_eq!(
        detail["trunk"],
        before.as_str(),
        "checked on the current trunk: {refused}"
    );
    assert_eq!(detail["verdict"], "pass", "{refused}");
    let checks: Vec<(&str, &str)> = detail["checks"]
        .as_array()
        .unwrap_or_else(|| panic!("{refused}"))
        .iter()
        .map(|check| {
            (
                check["check"].as_str().unwrap(),
                check["verdict"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(checks, [("tests", "pass")], "the check ran: {refused}");

    // Nothing was published, no landing marker exists, and no queue place was held.
    assert_eq!(
        remote_tip(&test_box, &repo, "refs/heads/main").as_deref(),
        Some(before.as_str())
    );
    assert_eq!(
        remote_tip(&test_box, &repo, &format!("refs/bay/landed/{id}")),
        None
    );
    assert_eq!(
        test_box.git_ok(&repo, &["ls-remote", "origin", "refs/bay/queue/*"]),
        ""
    );

    // The operation is blocked on its outcome: an unchanged retry is refused with no work.
    let (code, again) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{again}");
    assert_eq!(again["reason"], "LANE_NEVER_LANDS", "{again}");
    assert_eq!(again["detail"]["unchanged"], true, "{again}");
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let row = &status["detail"]["changes"][0];
    assert_eq!(row["lane"], "research", "{status}");
    assert_eq!(row["landed"], Value::Null, "{status}");
    assert_eq!(row["operation"]["outcome"], "LANE_NEVER_LANDS", "{status}");
    assert_eq!(row["operation"]["published"], false, "{status}");
}

#[test]
fn a_view_that_merged_a_lane_commit_is_refused_lane_leak_and_a_plain_change_lands() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    let research = research_branch(&test_box, &repo);
    let before = remote_tip(&test_box, &repo, "refs/heads/main").unwrap();

    // A change of no lane whose view merged the research branch.
    let (_, view) = test_box.start_and_edit(&repo);
    test_box.git_ok(
        &view,
        &[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "take research",
            "research/x",
        ],
    );
    let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "LANE_LEAK", "{refused}");
    assert_eq!(refused["detail"]["lane"], "research", "{refused}");
    let leak = &refused["detail"]["leaks"][0];
    assert_eq!(leak["lane"], "research", "{refused}");
    assert_eq!(leak["commits"], json!([research]), "{refused}");
    assert_eq!(leak["count"], 1, "{refused}");
    assert_eq!(
        remote_tip(&test_box, &repo, "refs/heads/main").as_deref(),
        Some(before.as_str()),
        "nothing landed"
    );

    // A plain change lands.
    let (_, plain) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&plain).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "landed", "{landed}");
}

#[test]
fn a_forge_merge_of_a_lane_branch_is_named_as_a_lane_leak() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    let cursor = test_box.bay_ok(&repo, &["watch"])["detail"]["cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let research = research_branch(&test_box, &repo);
    // Someone merges the research branch on the forge, outside bay, and the branch is gone from
    // this clone: bay reads it from the remote.
    test_box.git_ok(
        &repo,
        &[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "merge research",
            "research/x",
        ],
    );
    let merge = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);
    test_box.git_ok(&repo, &["branch", "-q", "-D", "research/x"]);

    let status = test_box.bay_ok(&repo, &["status"]);
    let unattributed = &status["detail"]["unattributed"];
    assert_eq!(unattributed["lane_leaks"], 1, "{status}");
    let landing = &unattributed["landings"][0];
    assert_eq!(landing["commit"], merge.as_str(), "{status}");
    assert_eq!(landing["landed_as"], "merge", "{status}");
    assert_eq!(landing["lane"], "research", "{status}");
    let leak = &landing["lane_leak"][0];
    assert_eq!(leak["lane"], "research", "{status}");
    assert_eq!(leak["ref"], "refs/remotes/origin/research/x", "{status}");
    assert_eq!(leak["commits"], json!([research]), "{status}");

    // The fleet map reads the same from the cache `bay status` fetched.
    let fleet = test_box.bay_ok(&repo, &["status", "--fleet"]);
    assert_eq!(fleet["detail"]["unattributed"], *unattributed, "{fleet}");

    // `bay watch` names the landing outside bay with the lane it leaked.
    let watched = test_box.bay_ok(&repo, &["watch", "--since", &cursor]);
    let events: Vec<&Value> = watched["detail"]["events"]
        .as_array()
        .unwrap_or_else(|| panic!("{watched}"))
        .iter()
        .filter(|event| {
            event["kind"] == "unattributed_landing" && event["commit"] == merge.as_str()
        })
        .collect();
    assert_eq!(events.len(), 1, "{watched}");
    assert_eq!(events[0]["lane"], "research", "{watched}");
    assert_eq!(
        events[0]["lane_leak"][0]["commits"],
        json!([research]),
        "{watched}"
    );
}
