//! G921 (plan S4, CL-1: at 50,000 changes the remote held 150,000 live refs, a chain, a request
//! and markers for every change ever made, and each listing paid for all of them). When retention
//! cuts a merged or abandoned change, its runner moves every ref the change leaves into the shard
//! index (`DESIGN` § 10a, ruling R11): its chain and its request each as a record of their target,
//! the record file it holds and its step log, and a merged change's markers as before. The change
//! leaves no ref, and `status` and `start` still answer for it from its shard; a store that held
//! its chain lets it go once it reads the shard.
//!
//! Guards to mutate: in `src/compact.rs`, `names_of` leaving out the chain and the request (they
//! stay on the host); in `src/status.rs`, `held_records` without its fall back to
//! `compact::kept` (a status of the old change cannot read it); `compact::forget` deleting
//! nothing (the agent's store keeps the chain).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// A host whose changes retention cuts at once, with no ref budget, and an agent's clone of it
/// with enrolled keys.
fn host_and_repo(test_box: &TestBox) -> (PathBuf, PathBuf, support::TestKeys) {
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[retention]\nwindow_s = 0\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    (host, repo, keys)
}

/// The refs of `names` that `host` holds.
fn held(test_box: &TestBox, host: &Path, names: &[String]) -> Vec<String> {
    let listed = test_box.git_bare(host, &["for-each-ref", "--format=%(refname)", "refs/bay/"]);
    names
        .iter()
        .filter(|name| listed.lines().any(|line| line == name.as_str()))
        .cloned()
        .collect()
}

/// Runner passes in `executor` until `host` holds none of `names`, at most 12.
fn compact_away(test_box: &TestBox, host: &Path, executor: &Path, names: &[String]) {
    for _ in 0..12 {
        if held(test_box, host, names).is_empty() {
            return;
        }
        let ran = test_box.bay_ok(executor, &["runner", "run", "--once"]);
        for entry in ran["detail"]["ran"].as_array().into_iter().flatten() {
            assert!(entry.get("error").is_none(), "{ran}");
        }
    }
    assert_eq!(held(test_box, host, names), Vec::<String>::new());
}

/// The change `status` in `dir` reads for `id`.
fn status_of(test_box: &TestBox, dir: &Path, id: &str) -> Value {
    let status = test_box.bay_ok(dir, &["status", id]);
    let change = status["detail"]["changes"][0].clone();
    assert_eq!(change["change"], id, "{status}");
    change
}

#[test]
fn a_cut_merged_change_leaves_no_ref_and_still_answers() {
    let test_box = TestBox::new();
    let (host, repo, keys) = host_and_repo(&test_box);
    let started = test_box.bay_ok(&repo, &["start", "LEDGER-1", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let intent = started["detail"]["intent"].as_str().unwrap().to_owned();
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    let landed_intent = intent.replace("refs/bay/intent/", "refs/bay/landed-intent/");
    let names = [
        format!("refs/bay/changes/{id}"),
        format!("refs/bay/requests/{id}"),
        format!("refs/bay/landed/{id}"),
        intent,
        landed_intent,
    ];
    assert_eq!(held(&test_box, &host, &names), names);

    // WHEN retention cuts it and runners pass
    let executor = test_box.clone_of(&host, "executor");
    test_box.use_keys(&executor, &keys);
    compact_away(&test_box, &host, &executor, &names);

    // THEN the host holds no ref of it: chain, request, marker, claim and landed claim moved
    assert_eq!(held(&test_box, &host, &names), Vec::<String>::new());
    // AND a fresh store's status answers from the shard
    let fresh = test_box.clone_of(&host, "fresh");
    let change = status_of(&test_box, &fresh, &id);
    assert_eq!(change["state"], "merged", "{change}");
    assert_eq!(change["retention_cut"], true, "{change}");
    // AND the store that made it answers alike, and lets its copy of the chain go
    let change = status_of(&test_box, &repo, &id);
    assert_eq!(change["state"], "merged", "{change}");
    let chain = &names[0];
    assert_eq!(
        test_box.git_ok(&repo, &["for-each-ref", "--format=%(refname)", chain]),
        "",
        "the agent's store keeps {chain}"
    );
    // AND a listing of the store's changes names it no more
    let listed = test_box.bay_ok(&repo, &["status"]);
    let changes = listed["detail"]["changes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        changes.iter().all(|change| change["change"] != id.as_str()),
        "{listed}"
    );
}

#[test]
fn a_cut_abandoned_change_leaves_no_ref_and_its_intent_opens_the_next_generation() {
    let test_box = TestBox::new();
    let (host, repo, keys) = host_and_repo(&test_box);
    let started = test_box.bay_ok(&repo, &["start", "LEDGER-2", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let intent = started["detail"]["intent"].as_str().unwrap().to_owned();
    std::fs::write(view.join("src/gone.rs"), "fn gone() {}\n").unwrap();
    // submitted, so it has a request, and given up before any runner took it
    let (code, submitted) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");
    let (code, abandoned) = result_json(test_box.bay(&view).arg("abandon"));
    assert_eq!(code, 0, "{abandoned}");
    let names = [
        format!("refs/bay/changes/{id}"),
        format!("refs/bay/requests/{id}"),
    ];
    assert_eq!(held(&test_box, &host, &names), names);

    // WHEN retention cuts it and runners pass
    let executor = test_box.clone_of(&host, "executor");
    test_box.use_keys(&executor, &keys);
    compact_away(&test_box, &host, &executor, &names);

    // THEN the host holds neither its chain nor its request
    assert_eq!(held(&test_box, &host, &names), Vec::<String>::new());
    // AND a fresh store's status answers from the shard
    let fresh = test_box.clone_of(&host, "fresh");
    test_box.use_keys(&fresh, &keys);
    let change = status_of(&test_box, &fresh, &id);
    assert_eq!(change["state"], "abandoned", "{change}");
    assert_eq!(change["retention_cut"], true, "{change}");
    // AND a start of its intent never takes it up again: it claims the next generation
    let (code, next) = result_json(
        test_box
            .bay(&fresh)
            .args(["start", "LEDGER-2", "--paths", "src/"]),
    );
    assert_eq!(code, 0, "{next}");
    assert_ne!(next["change"], id.as_str(), "{next}");
    assert_eq!(
        next["detail"]["intent"].as_str(),
        Some(format!("{}/2", intent.strip_suffix("/1").unwrap()).as_str()),
        "{next}"
    );
}
