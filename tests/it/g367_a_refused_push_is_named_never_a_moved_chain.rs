//! G367 (a hosted product's finding, 2026-10-25: a remote that refused a push past its history
//! limit read as "the change chain moved"). A refused push is named, never a moved chain.
//!
//! When the push of a change's chain and request fails and the remote's chain did not move, the
//! remote refused the push itself (its hook, a limit, its rights). `bay merge` says so, with the
//! first line of the remote's reason, and does not try again: a retry would be refused the same
//! way. A chain that really moved is read again, as before.
//!
//! Guard to mutate: `src/publish.rs` `Ctx::refused`: return `None` at once (the refusal reads as
//! a moved chain).

// The remote's hook is a shell script, so this runs on Unix only.
#![cfg(unix)]

use crate::support;

use std::os::unix::fs::PermissionsExt;

use support::{TestBox, result_json, unchecked_files};

#[test]
fn a_remote_that_refuses_the_request_is_named_with_its_reason() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let remote = test_box.path().join("repo-remote.git");
    let (_, view) = test_box.start_and_edit(&repo);

    // The remote now refuses every request: as a host past its history limit would.
    let hook = remote.join("hooks/pre-receive");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(
        &hook,
        "#!/bin/sh\nwhile read old new ref; do case \"$ref\" in refs/bay/requests/*) echo 'this project is over its history limit' >&2; exit 1;; esac; done\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let (code, result) = result_json(test_box.bay(&view).arg("merge"));
    assert_ne!(code, 0, "{result}");
    let text = result.to_string();
    assert!(
        text.contains("the remote refused the push"),
        "the refusal is not named: {result}"
    );
    assert!(
        text.contains("over its history limit"),
        "the remote's reason is lost: {result}"
    );
    assert!(
        !text.contains("chain moved"),
        "a refusal reads as a moved chain: {result}"
    );
}
