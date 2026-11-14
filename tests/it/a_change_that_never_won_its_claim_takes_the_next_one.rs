//! BACKLOG 235 (a fleet finding, 2026-11-14, bay 0.6.70: h3u6cttl and rqhaya4p, one actor, both
//! read generation 6 of one intent; rqhaya4p claimed it and landed; the next day `bay start
//! h3u6cttl` pushed its chain as it was, its checks passed, and its publish was refused
//! `CHANGE_ALREADY_MERGED` while its work was not on main, which then counted it as ended).
//! A start that ends after its chain is written and before its claim is pushed leaves a change
//! the remote never gave a claim. Picked up again, that change claims its intent's next free
//! generation, names the change that holds the first, and merges. While the intent's newest
//! generation is another live change, it claims nothing and names that change; once that
//! change merges, its own merge is refused `CHANGE_SUPERSEDED` naming the merge and the start
//! that recovers it, and that start claims.
//!
//! Guards to mutate: `src/start.rs` `claim_unwon` taking any claim the change's record names
//! as won (the resumed change keeps its generation and is never merged); `src/publish.rs`
//! `after_refused_push` reading no merge (the refusal says `CHANGE_ALREADY_MERGED`).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::json;
use support::{TestBox, check_files, passing_result, result_json};

/// A trunk with one passing check, and a start of `card` that ends after its chain is written
/// and before its claim is pushed: the change it left.
fn orphaned_start(card: &str) -> (TestBox, PathBuf, PathBuf, String) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let killed = test_box
        .bay(&repo)
        .env("BAY_TEST_KILL_AT", "start-before-claim-push")
        .args(["start", card, "--paths", "src/"])
        .output()
        .unwrap();
    assert_eq!(killed.status.code(), Some(137), "{killed:?}");
    let local = test_box.git_ok(
        &repo,
        &["for-each-ref", "--format=%(refname)", "refs/bay/changes/"],
    );
    let orphan = local
        .trim()
        .strip_prefix("refs/bay/changes/")
        .unwrap()
        .to_owned();
    let claims = test_box.git_bare(&remote, &["for-each-ref", "refs/bay/intent/"]);
    assert_eq!(claims.trim(), "", "the remote holds no claim yet");
    (test_box, repo, remote, orphan)
}

/// Starts `card` and writes `file` in its view: the change and its view.
fn start_with(test_box: &TestBox, repo: &Path, card: &str, file: &str) -> (String, PathBuf) {
    let started = test_box.bay_ok(repo, &["start", card, "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(file), format!("// {card}\n")).unwrap();
    (id, view)
}

fn merged(test_box: &TestBox, view: &Path) {
    let (code, landed) = result_json(test_box.bay(view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
}

#[test]
fn a_change_whose_claim_another_change_landed_takes_the_next_generation_and_merges() {
    let (test_box, repo, remote, orphan) = orphaned_start("CARD-235");
    // GIVEN a second start of the intent that claimed the generation and landed
    let (holder, holder_view) = start_with(&test_box, &repo, "CARD-235", "src/holder.rs");
    assert_ne!(holder, orphan);
    merged(&test_box, &holder_view);

    // WHEN the first change is picked up again
    let resumed = test_box.bay_ok(&repo, &["start", &orphan]);

    // THEN it claims the next generation and names the change that holds its first
    assert_eq!(
        resumed["detail"]["claim"],
        json!({"generation": 2, "unwon": 1, "held_by": holder}),
        "{resumed}"
    );
    // AND it merges, beside the change that landed first.
    let view = PathBuf::from(resumed["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/orphan.rs"), "// first start\n").unwrap();
    merged(&test_box, &view);
    assert_eq!(test_box.trunk_change_ids(&remote), vec![orphan, holder]);
}

#[test]
fn a_change_whose_intent_has_a_live_change_waits_on_it_then_claims_after_its_merge() {
    let (test_box, repo, remote, orphan) = orphaned_start("CARD-235-LIVE");
    // GIVEN a second start of the intent that claimed the generation and is still live
    let (holder, holder_view) = start_with(&test_box, &repo, "CARD-235-LIVE", "src/holder.rs");

    // WHEN the first change is picked up again
    let resumed = test_box.bay_ok(&repo, &["start", &orphan]);

    // THEN it claims nothing, and names the live change it waits on
    let claim = &resumed["detail"]["claim"];
    assert_eq!(claim["generation"], json!(null), "{resumed}");
    assert_eq!(claim["held_by"], json!(holder), "{resumed}");
    assert_eq!(claim["waits_on"], json!(holder), "{resumed}");
    let claims = test_box.git_bare(
        &remote,
        &["for-each-ref", "--format=%(refname)", "refs/bay/intent/"],
    );
    assert_eq!(claims.lines().count(), 1, "{claims}");

    // WHEN the holder merges, and then the first change's merge is tried
    merged(&test_box, &holder_view);
    let view = PathBuf::from(resumed["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/orphan.rs"), "// first start\n").unwrap();
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));

    // THEN it is refused as superseded by the merge, never as already merged
    assert_ne!(code, 0, "{refused}");
    assert_eq!(refused["reason"], "CHANGE_SUPERSEDED", "{refused}");
    assert_eq!(
        refused["detail"]["superseded_by"],
        json!(holder),
        "{refused}"
    );
    assert_eq!(
        refused["detail"]["recovery"],
        json!(format!(
            "bay start {orphan} claims the next generation of its intent, then bay merge"
        )),
        "{refused}"
    );
    // AND the start it names claims the next generation, and the change merges.
    let again = test_box.bay_ok(&repo, &["start", &orphan]);
    assert_eq!(
        again["detail"]["claim"],
        json!({"generation": 2, "unwon": 1, "held_by": holder}),
        "{again}"
    );
    merged(&test_box, &view);
    assert_eq!(test_box.trunk_change_ids(&remote), vec![orphan, holder]);
}

#[test]
fn a_change_whose_generation_another_change_merged_first_takes_the_next_one() {
    let (test_box, repo, remote, orphan) = orphaned_start("CARD-235-RACE");
    let (holder, holder_view) = start_with(&test_box, &repo, "CARD-235-RACE", "src/holder.rs");
    // GIVEN the first change, picked up while the holder is live, merges the generation first,
    // as two changes may hold one generation and the one that merges names it (§ 6)
    let resumed = test_box.bay_ok(&repo, &["start", &orphan]);
    let view = PathBuf::from(resumed["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/orphan.rs"), "// first start\n").unwrap();
    merged(&test_box, &view);

    // WHEN the holder's merge is tried
    let (code, refused) = result_json(test_box.bay(&holder_view).args(["merge", "--wait"]));

    // THEN it is refused as superseded by that merge, though its claim is the remote's
    assert_ne!(code, 0, "{refused}");
    assert_eq!(refused["reason"], "CHANGE_SUPERSEDED", "{refused}");
    assert_eq!(
        refused["detail"]["superseded_by"],
        json!(orphan),
        "{refused}"
    );
    // AND its start claims the next generation, naming the merge of its first, and it merges.
    let again = test_box.bay_ok(&repo, &["start", &holder]);
    assert_eq!(
        again["detail"]["claim"],
        json!({"generation": 2, "unwon": 1, "held_by": orphan}),
        "{again}"
    );
    merged(&test_box, &holder_view);
    assert_eq!(test_box.trunk_change_ids(&remote), vec![holder, orphan]);
}
