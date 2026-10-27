//! G409 (B4; fleet findings, 2026-10-26/27: GitHub refused a passed head's merging push with a
//! bare "(failed)" and with GH009, both cleared on a retry, and bay released the head's place
//! as `failed`, sending a passed change to the back of the line). A push the host refuses
//! while main stood still, other than by bay's own judgement, is a fault of the push: the same
//! candidate is pushed again, and it merges in its turn.
//!
//! Guard to mutate: `src/publish.rs` `after_refused_push`: a refusal on an unmoved main is
//! `failed` (the B4 retry removed).

#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json, write_executable};

#[test]
fn a_transient_refusal_of_the_merging_push_is_retried_and_merges() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let files = check_files("true", &passing_result(1), 1, 60);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    // The host refuses the first push of main, as GitHub's GH009 did, then takes the next.
    let once = test_box.path().join("refused-once");
    write_executable(
        &remote.join("hooks/pre-receive"),
        &format!(
            "#!/bin/sh\nwhile read old new name; do\n  if [ \"$name\" = refs/heads/main ] && [ ! -e '{once}' ]; then\n    : > '{once}'\n    echo 'error: GH009: Git LFS object integrity could not be checked' >&2\n    exit 1\n  fi\ndone\nexit 0\n",
            once = once.display()
        ),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let executor = test_box.clone_of(&remote, "executor");
    let (code, ran) = result_json(test_box.bay(&executor).args(["runner", "run", "--once"]));
    assert_eq!(code, 0, "{ran}");
    assert!(once.exists(), "the host never refused: {ran}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id], "{ran}");
}
