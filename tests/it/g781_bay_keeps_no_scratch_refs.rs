//! G781 (a measurement, 2026-11-18: a live `bay queue` took 95 s, 87 s of it in four fetches
//! that each walked the store's 119,575 local refs for about 20 s first; 6,942 of them were
//! `bay waiting`'s copies of every change chain, kept between runs). `bay waiting` lists the
//! remote's chain heads, fetches only those this store lacks, under its own process's scratch
//! refs, and deletes them before it returns: it never fetches every chain and leaves no copy.
//! A start deletes the scratch refs an ended or older process left behind, and keeps those of a
//! process that still runs. Read from the trace a test build writes (G171).
//!
//! Guards to mutate: in `src/approve.rs` `fetch_missing`, drop the deletion of the scratch refs
//! (the second read finds them left); in `src/scratch.rs` `prune`, treat every ref as in use
//! (`None => false`): the leftovers stay.

#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::approval::trunk;

/// The refs of `dir`'s store under `prefix`.
fn refs(test_box: &support::TestBox, dir: &std::path::Path, prefix: &str) -> Vec<String> {
    test_box
        .git_ok(dir, &["for-each-ref", "--format=%(refname)", prefix])
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn waiting_fetches_only_what_it_lacks_and_keeps_no_copy() {
    let trunk = trunk();
    let (id, _view, _candidate) = trunk.held_change();
    let tb = &trunk.test_box;

    // WHEN the approver lists what waits, with a trace of its Git calls
    let trace = tb.path().join("waiting-trace.jsonl");
    let (code, listed) = support::result_json(
        tb.bay(&trunk.approver_box)
            .arg("waiting")
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{listed}");
    let items = listed["detail"]["waiting"].as_array().unwrap();
    assert!(
        items.iter().any(|item| item["change"] == id.as_str()),
        "the held change is listed: {listed}"
    );

    // THEN it fetched no wholesale copy of every chain
    let text = std::fs::read_to_string(&trace).unwrap_or_default();
    let fetches: Vec<String> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["sub"] == "fetch")
        .map(|line| line["args"].to_string())
        .collect();
    assert!(
        fetches
            .iter()
            .all(|args| !args.contains("refs/bay/changes/*")),
        "a fetch of every chain: {fetches:?}"
    );
    // AND it left no scratch ref behind
    assert_eq!(
        refs(tb, &trunk.approver_box, "refs/bay/fetch-waiting/"),
        Vec::<String>::new()
    );

    // AND a second read, with every head held here, fetches no chain at all
    let again = tb.path().join("waiting-again.jsonl");
    let (code, listed) = support::result_json(
        tb.bay(&trunk.approver_box)
            .arg("waiting")
            .env("BAY_TEST_GIT_TRACE", &again),
    );
    assert_eq!(code, 0, "{listed}");
    let text = std::fs::read_to_string(&again).unwrap_or_default();
    let chain_fetches = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["sub"] == "fetch" && line["args"].to_string().contains("fetch-waiting"))
        .count();
    assert_eq!(chain_fetches, 0, "a held chain was fetched again: {text}");
}

#[test]
fn a_start_deletes_scratch_refs_no_running_process_holds() {
    let trunk = trunk();
    let tb = &trunk.test_box;
    let repo = &trunk.repo;
    let head = tb.git_ok(repo, &["rev-parse", "HEAD"]);
    let head = head.trim();
    // GIVEN scratch refs an older bay kept, one of a process that has ended, and one of a
    // process that still runs
    let live = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let ended = 999_999;
    for name in [
        "refs/bay/fetch-waiting/abcdefgh".to_owned(),
        "refs/bay/fetch/from-123".to_owned(),
        format!("refs/bay/fetch/{ended}/abcdefgh"),
        format!("refs/bay/fetch-evidence/{ended}/someone"),
        format!("refs/bay/fetch/{}/kept", live.id()),
    ] {
        tb.git_ok(repo, &["update-ref", &name, head]);
    }

    // WHEN a change starts
    tb.bay_ok(repo, &["start", "CARD-G781", "--paths", "src/"]);

    // THEN only the running process's scratch ref is left
    let mut left = refs(tb, repo, "refs/bay/fetch-waiting/");
    left.extend(refs(tb, repo, "refs/bay/fetch/"));
    left.extend(refs(tb, repo, "refs/bay/fetch-evidence/"));
    let mut live = live;
    let _ = live.kill();
    let _ = live.wait();
    assert_eq!(left, vec![format!("refs/bay/fetch/{}/kept", live.id())]);
}
