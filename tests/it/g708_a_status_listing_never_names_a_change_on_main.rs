//! G708 (a fleet finding, 2026-11-13: `bay status` named five changes in `more_changes` whose own
//! `bay status <change>` said they were on main). Their chains still read `ready`; each had
//! published an operation, or held a landed marker, or was refused as already merged. A listing
//! judges what is live by the one rule the flow and the overlaps use.
//!
//! Guards to mutate: in `src/status.rs` `live_ids`, the `counts_live` filter (back to the chain's
//! state alone); in `src/overlap.rs` `counts_live`, the `ended()` term (a published operation no
//! longer ends a change).

#![cfg(unix)]

use crate::support;

use support::{TestBox, result_json, unchecked_files};

#[test]
fn a_status_listing_never_names_a_change_on_main() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");

    // GIVEN a merged change whose chain reads `ready` again, its published operation kept, with
    // no landed marker
    let handle = bay::repo::Repo::open(&repo).unwrap();
    let chain = bay::change::Chain::new(&handle.git);
    let (head, mut record) = chain.read(&id).unwrap();
    assert!(
        record
            .operations
            .iter()
            .any(|op| op.state == bay::change::OpState::Published),
        "{record:?}"
    );
    record.state = bay::change::ChangeState::Ready;
    let draft = chain.latest_draft(&id).unwrap();
    let state = chain
        .append(&head, &record, &draft, "an older bay reopened it")
        .unwrap();
    let name = format!("refs/bay/changes/{id}");
    test_box.git_ok(&repo, &["update-ref", &name, &state]);
    test_box.git_ok(
        &repo,
        &["push", "-q", "-f", "origin", &format!("{state}:{name}")],
    );
    let marker = format!("refs/bay/landed/{id}");
    let _ = test_box.git_bare(&remote, &["update-ref", "-d", &marker]);
    let _ = std::process::Command::new("git")
        .current_dir(&repo)
        .args(["update-ref", "-d", &marker])
        .status();

    // WHEN the repository's changes are listed
    let (_, status) = result_json(test_box.bay(&repo).args(["status"]));

    // THEN the listing neither describes nor names it, and the flow counts nothing live
    let detail = &status["detail"];
    let rows = detail["changes"].as_array().unwrap();
    assert!(
        rows.iter().all(|row| row["change"] != id.as_str()),
        "{detail}"
    );
    assert!(detail["more_changes"].is_null(), "{detail}");
    assert_eq!(detail["flow"]["live"], 0, "{detail}");
}
