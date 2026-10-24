//! G344 (BACKLOG 58, owner ruling 2026-10-23): main's commit keeps the agent's message.
//!
//! A host shows what a change did from main's commit. An agent says it in its own commit in
//! the view, with an `Open:` line for anything left undone or needing a person. `bay merge`
//! keeps that message: main's commit has bay's subject, then the agent's words, then bay's
//! trailers and the agent's own (`git interpret-trailers --parse` reads `Open:` there). A
//! trailer bay writes itself (`Change-Id` and the rest) never comes from the agent. No flag:
//! the message is the view's latest commit's.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/change.rs` `freeze_draft`: always use `message` (the draft says "draft").
//! - `src/publish.rs` `candidate_message`: return `bay_message` alone (main's commit holds
//!   only bay's lines).

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use support::{TestBox, result_json, unchecked_files};

#[test]
fn main_keeps_the_agents_words_and_open_items() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let started = test_box.bay_ok(&repo, &["start", "speed up the parser", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/a.rs"), "fn a() { 1 }\n").unwrap();
    test_box.git_ok(&view, &["add", "-A"]);
    test_box.git_ok(
        &view,
        &[
            "commit",
            "-q",
            "-m",
            "Speed up the parser",
            "-m",
            "The tokenizer no longer copies each line.",
            "-m",
            "Open: benchmark it on the large corpus\nChange-Id: forged",
        ],
    );
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");

    let message = test_box.git_bare(&remote, &["log", "-1", "--format=%B", "main"]);
    assert!(
        message.starts_with("speed up the parser: change "),
        "{message}"
    );
    assert!(
        message.contains("\n\nSpeed up the parser\n\nThe tokenizer no longer copies each line."),
        "{message}"
    );
    let trailers = test_box.git_bare(
        &remote,
        &[
            "-c",
            "trailer.separators=:",
            "log",
            "-1",
            "--format=%(trailers:only)",
            "main",
        ],
    );
    assert!(
        trailers.contains("Open: benchmark it on the large corpus"),
        "{trailers}"
    );
    assert!(trailers.contains(&format!("Change-Id: {id}")), "{trailers}");
    assert!(
        !trailers.contains("forged"),
        "an agent set a bay trailer: {trailers}"
    );
}
