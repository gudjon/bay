//! G593 (a fleet finding, 2026-10-30: a runner's judgement of main's red took 682 s after its
//! update, its thread on a CPU for 12 s of them). In a blob-less clone an evidence read asks for
//! each note at every depth of the notes tree, and the names the tree lacks are answered
//! `missing` with the note blobs the store lacks. Each such name cost a `rev-parse` of its own
//! to tell the two apart, about 27,000 git processes for 109 landings. Now the names of one
//! notes ref are resolved by one tree listing, and the read still fetches the note it needs.
//!
//! Guard to mutate: `src/git.rs` `tree_path` answering `None`, so each name resolves alone.

#![cfg(unix)]

use crate::support;

use std::path::Path;

use support::queued::{Check, OTHER_PRODUCER, host};
use support::result_json;

const CONFIG: &str = "[runner]\nlease_s = 30\nheartbeat_s = 1\n";

/// The git processes of a trace2 event log that run `word` and name a path in an evidence
/// notes ref (`refs/notes/bay/evidence/<producer>:<path>`), or the ref alone with `path` false.
fn evidence_calls(trace: &Path, word: &str, path: bool) -> Vec<String> {
    let names_path = |line: &str| {
        line.split("refs/notes/bay/evidence/").skip(1).any(|rest| {
            rest.split('"')
                .next()
                .is_some_and(|name| name.contains(':'))
        })
    };
    std::fs::read_to_string(trace)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(r#""event":"start""#))
        .filter(|line| line.contains(&format!(r#""{word}""#)))
        .filter(|line| line.contains("refs/notes/bay/evidence/"))
        .filter(|line| !path || names_path(line))
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_blob_less_clone_resolves_absent_note_names_a_listing_at_a_time() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let test_box = &held.test_box;
    test_box.git_bare(&held.remote, &["config", "uploadpack.allowFilter", "true"]);
    let id = held.queue("one", "src");
    // Another producer's runner lands it: the remote holds evidence this clone did not write.
    let runner = held.executor("runner", OTHER_PRODUCER);
    let merged = held.pass(&runner);
    assert_eq!(
        test_box.trunk_change_ids(&held.remote),
        vec![id.clone()],
        "{merged}"
    );
    let cache = test_box.path().join("cache");
    test_box.git_ok(
        test_box.path(),
        &[
            "clone",
            "-q",
            "--filter=blob:none",
            "--no-checkout",
            &format!("file://{}", held.remote.display()),
            cache.to_str().unwrap(),
        ],
    );
    let trace = test_box.path().join("trace.json");
    let (code, status) = result_json(
        test_box
            .bay(&cache)
            .env("GIT_TRACE2_EVENT", &trace)
            .args(["status", &id]),
    );
    assert_eq!(code, 0, "{status}");
    let alone = evidence_calls(&trace, "rev-parse", true);
    assert!(
        alone.is_empty(),
        "{} note names resolved one at a time: {:#?}",
        alone.len(),
        alone.iter().take(3).collect::<Vec<_>>()
    );
    let listings = evidence_calls(&trace, "ls-tree", false);
    assert!(!listings.is_empty(), "the absent note names were listed");
    let change = &status["detail"]["changes"][0];
    assert_eq!(change["change"], id.as_str(), "{status}");
    assert!(
        !change["evidence"].is_null(),
        "the note's records were read: {change}"
    );
}
