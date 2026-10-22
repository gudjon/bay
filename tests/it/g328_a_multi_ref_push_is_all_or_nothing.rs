//! G328 (0.3.1, found in a survey of bay's pushes): a push of several refs is all or nothing.
//!
//! Every push of more than one ref is `--atomic`, as merges, queues and starts already were:
//! `bay share` (the change and its evidence notes) and a talk's send. A host that refuses one
//! ref of `bay share`'s push then holds none of them, never the change without its evidence.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/share.rs` `run`: drop `"--atomic"` (the remote keeps the change ref without its
//!   notes).

// The remote's hook is a shell script, so Unix only.
#![cfg(unix)]

use crate::support;

use std::os::unix::fs::PermissionsExt;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_share_the_host_refuses_in_part_leaves_nothing_there() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    let notes = test_box.git_ok(
        &repo,
        &["for-each-ref", "--format=%(refname)", "refs/notes/"],
    );
    assert!(
        !notes.is_empty(),
        "the merge left no evidence notes to share"
    );

    // A host that takes changes but refuses every notes ref.
    let other = test_box.bare_any("other.git");
    let hook = other.join("hooks/update");
    std::fs::write(
        &hook,
        "#!/bin/sh\ncase \"$1\" in refs/notes/*) echo refused >&2; exit 1;; esac\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    test_box.git_ok(&repo, &["remote", "add", "other", other.to_str().unwrap()]);

    let (code, shared) = result_json(
        test_box
            .bay(&repo)
            .args(["share", &id, "--remote", "other"]),
    );
    assert_ne!(
        code, 0,
        "the host refused the notes, yet share says {shared}"
    );
    let held = test_box.git_bare(&other, &["for-each-ref", "--format=%(refname)"]);
    assert!(
        !held.contains(&format!("refs/bay/changes/{id}")),
        "the host holds the change without its evidence:\n{held}"
    );
}
