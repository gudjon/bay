//! G929 (a fleet measurement, 2026-11-27: of 16 refused landing pushes in 12 h on one fleet,
//! the runner's journal named the cause of none past `pre-receive hook declined`): a refusal
//! through an origin names what the hosted remote said.
//!
//! An origin in `through` mode forwards each push to its hosted remote, and a refusal there
//! refuses the push. The origin kept only each refused ref's summary, so a hosted remote hook's
//! reason, its `remote:` lines, never reached the runner, and the ref whose refusal refused an
//! atomic push could stand behind four refs refused with it, past the 300 characters a runner's
//! journal keeps. The origin now says what the hosted remote said first, then the cause ref.
//!
//! Guards to mutate (each makes a case here, or the unit cases in `src/origin.rs`, fail):
//! - `src/origin.rs` `push`: `Pushed::read(&out)` without `.heard(&err)` (the hook's reason
//!   is lost).
//! - `src/origin.rs` `Pushed::refusals`: drop the `sort_by_key` (a ref refused as `failed`
//!   comes before the ref that caused it).

// Each case runs shell scripts (the check adapter and a hook), so these run on Unix only.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;

use crate::g879_an_origin_sends_its_hosted_remote_every_push_it_takes::{commit_on, fleet, value};
use crate::support::TestBox;

#[test]
fn a_push_the_hosted_remote_refuses_names_what_it_said_before_the_ref() {
    let test_box = TestBox::new();
    let (hosted, origin, repo) = fleet(&test_box);
    let trunk = value(&test_box, &origin, "refs/heads/main").unwrap();

    // The hosted remote's hook refuses every push and says why on stderr.
    let hooks = test_box.path().join("hosted-hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-receive");
    std::fs::write(
        &hook,
        "#!/bin/sh\necho 'GH013: rule R1 says no' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    test_box.git_bare(
        &hosted,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );

    let mine = commit_on(&test_box, &repo, &trunk, "mine");
    let out = test_box
        .git(&repo)
        .args([
            "push",
            origin.to_str().unwrap(),
            &format!("{mine}:refs/heads/main"),
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "the refused push was taken");
    // The origin's refusal names the hosted remote's words, then the refused ref.
    let said = stderr
        .find("it said: GH013: rule R1 says no")
        .unwrap_or_else(|| panic!("the hosted remote's reason is lost: {stderr}"));
    let named = stderr
        .find("refs/heads/main [remote rejected] (pre-receive hook declined)")
        .unwrap_or_else(|| panic!("{stderr}"));
    assert!(said < named, "{stderr}");
    // Neither the hosted remote nor the origin took it.
    assert_eq!(
        value(&test_box, &hosted, "refs/heads/main"),
        Some(trunk.clone())
    );
    assert_eq!(value(&test_box, &origin, "refs/heads/main"), Some(trunk));
}
