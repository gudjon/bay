//! G563 (a fleet finding, 2026-10-29: `refs/bay/changes/hvvrfutj-r` held a copy of
//! `hvvrfutj`'s chain, made outside bay. The copy said `ready`, so it took one of its actor's live
//! places for four days after `hvvrfutj` was abandoned, and `bay abandon hvvrfutj-r` wrote to
//! `hvvrfutj` and failed with "the change chain moved").
//!
//! bay names each chain by its record's id, so a ref of any other name is a copy: it never counts
//! as a live change, and `bay abandon` of it fails naming the copy and how to remove it.
//!
//! Guards to mutate, in `src/change.rs`: the `record.id != id` refusal in `Chain::read` (the
//! abandon writes to the other change), and the `record.id == id` arm in `Chain::read_all` (the
//! copy takes the live place and the second start is refused `WIP_LIMIT`).

use crate::support;

use support::{TestBox, UNCHECKED_POLICY, result_json};

#[test]
fn a_copy_takes_no_live_place_and_abandoning_it_names_the_copy() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let policy = format!("{UNCHECKED_POLICY}\n[agent]\nmax_live_changes = 1\n");
    test_box.seed_trunk(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), (".bay/policy.toml", &policy)],
    );
    let (code, first) = result_json(
        test_box
            .bay(&repo)
            .env("BAY_ACTOR", "agent")
            .args(["start", "first", "--paths", "src/"]),
    );
    assert_eq!(code, 0, "{first}");
    let id = first["change"].as_str().unwrap().to_owned();
    let copy = format!("{id}-r");
    // The copy is made as the fleet's was: the chain's head under a second name, here and on the
    // remote.
    test_box.git_ok(
        &repo,
        &[
            "update-ref",
            &format!("refs/bay/changes/{copy}"),
            &format!("refs/bay/changes/{id}"),
        ],
    );
    test_box.git_ok(
        &repo,
        &[
            "push",
            "-q",
            "origin",
            &format!("refs/bay/changes/{copy}:refs/bay/changes/{copy}"),
        ],
    );
    let (code, abandoned) = result_json(
        test_box
            .bay(&repo)
            .env("BAY_ACTOR", "agent")
            .args(["abandon", &id]),
    );
    assert_eq!(code, 0, "{abandoned}");

    let (code, second) = result_json(
        test_box
            .bay(&repo)
            .env("BAY_ACTOR", "agent")
            .args(["start", "second", "--paths", "src/"]),
    );
    assert_eq!(
        code, 0,
        "the copy of an abandoned change took the actor's one live place: {second}"
    );

    let (code, refused) = result_json(
        test_box
            .bay(&repo)
            .env("BAY_ACTOR", "agent")
            .args(["abandon", &copy]),
    );
    assert_ne!(code, 0, "{refused}");
    let text = refused.to_string();
    assert!(
        text.contains("a copy bay never wrote")
            && text.contains(&format!(":refs/bay/changes/{copy}")),
        "abandoning the copy names it and how to remove it: {refused}"
    );
}
