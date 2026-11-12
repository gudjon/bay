//! G701 (a fleet finding, 2026-11-12: `bay status` named a change merged 15 days before as the
//! oldest live change, in `detail.flow.oldest_live`). Its chain never recorded the landing and
//! had no landed marker; its newest operation was refused `CHANGE_ALREADY_MERGED`. A change
//! refused that way is on main: it is not live in the flow, the overlaps or the live-change limit.
//!
//! Guard to mutate: in `src/overlap.rs` `counts_live`, the `refused_as_merged` term (the merged
//! change counts as live again).

#![cfg(unix)]

use crate::support;

use support::{TestBox, result_json, unchecked_files};

#[test]
fn a_change_on_main_is_never_counted_live() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");

    // GIVEN a merged change whose chain never recorded its landing, with no landed marker,
    // and whose newest operation was refused as already merged
    let handle = bay::repo::Repo::open(&repo).unwrap();
    let chain = bay::change::Chain::new(&handle.git);
    let (head, mut record) = chain.read(&id).unwrap();
    record.state = bay::change::ChangeState::Ready;
    let op = record.operations.last_mut().unwrap();
    op.outcome = Some("CHANGE_ALREADY_MERGED".to_owned());
    let draft = chain.latest_draft(&id).unwrap();
    let state = chain
        .append(&head, &record, &draft, "refused as already merged")
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

    // WHEN its repository's flow is read
    let (_, status) = result_json(test_box.bay(&repo).args(["status"]));

    // THEN it is not the oldest live change, nor live at all
    let flow = &status["detail"]["flow"];
    assert_eq!(flow["live"], 0, "{flow}");
    assert!(flow["oldest_live"].is_null(), "{flow}");
}
