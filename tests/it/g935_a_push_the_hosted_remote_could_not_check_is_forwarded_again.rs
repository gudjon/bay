//! G935 (a fleet measurement, 2026-11-27: in 15 h of one fleet's runner journal, 8 refused
//! landing pushes said `missing necessary objects` and one GH009, each a landing lost for
//! 42 to 138 s): a push the hosted remote refused only because it could not check it is
//! forwarded again.
//!
//! An origin in `through` mode forwards each push to its hosted remote, and a refusal there
//! refuses the push. A hosted remote that could not check a push (it did not see the objects
//! the push needs, or it could not check their large-file content, GH009) takes the same push
//! a moment later, so the origin now forwards it again, after 1 s and then 3 s, within the
//! hook's bound. Any other refusal is said at once, as before.
//!
//! Guards to mutate (each makes a case here, or the unit case in `src/origin.rs`, fail):
//! - `src/origin.rs` `forward`: call `push` in place of `push_checked` (the first case's push
//!   is refused).
//! - `src/origin.rs` `Pushed::unchecked`: drop the GH009 test of what the hosted remote said
//!   (the second case's hook is called three times).

// Each case runs shell scripts (the check adapter and a hook), so these run on Unix only.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::g879_an_origin_sends_its_hosted_remote_every_push_it_takes::{commit_on, fleet, value};
use crate::support::TestBox;

/// Gives `hosted` a `pre-receive` hook that writes a line to `calls` each time it runs, then
/// runs `body`.
fn hosted_hook(test_box: &TestBox, hosted: &Path, calls: &Path, body: &str) {
    let hooks = test_box.path().join("hosted-hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-receive");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\necho call >> '{}'\n{body}\n", calls.display()),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    test_box.git_bare(
        hosted,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
}

fn count(calls: &Path) -> usize {
    std::fs::read_to_string(calls).map_or(0, |text| text.lines().count())
}

#[test]
fn a_push_the_hosted_remote_could_not_check_once_is_taken_on_the_next_forward() {
    let test_box = TestBox::new();
    let (hosted, origin, repo) = fleet(&test_box);
    let trunk = value(&test_box, &origin, "refs/heads/main").unwrap();

    // The hosted remote cannot check the first push it is sent, and takes the next.
    let calls = test_box.path().join("calls");
    let once = test_box.path().join("refused-once");
    hosted_hook(
        &test_box,
        &hosted,
        &calls,
        &format!(
            "[ -e '{0}' ] && exit 0\ntouch '{0}'\necho 'error: GH009: Git LFS object integrity could not be checked.' >&2\nexit 1",
            once.display()
        ),
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
    assert!(out.status.success(), "the push was refused: {stderr}");
    assert!(
        stderr.contains("could not check the push; forwarding it again in 1s"),
        "{stderr}"
    );
    assert_eq!(count(&calls), 2, "{stderr}");
    // Both the hosted remote and the origin took it.
    assert_eq!(
        value(&test_box, &hosted, "refs/heads/main"),
        Some(mine.clone())
    );
    assert_eq!(value(&test_box, &origin, "refs/heads/main"), Some(mine));
}

#[test]
fn a_push_a_hosted_remote_rule_refuses_is_forwarded_once() {
    let test_box = TestBox::new();
    let (hosted, origin, repo) = fleet(&test_box);
    let trunk = value(&test_box, &origin, "refs/heads/main").unwrap();

    let calls = test_box.path().join("calls");
    hosted_hook(
        &test_box,
        &hosted,
        &calls,
        "echo 'error: GH013: Repository rule violations found' >&2\nexit 1",
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
    assert!(stderr.contains("it said: error: GH013"), "{stderr}");
    assert!(!stderr.contains("forwarding it again"), "{stderr}");
    assert_eq!(count(&calls), 1, "{stderr}");
    assert_eq!(
        value(&test_box, &hosted, "refs/heads/main"),
        Some(trunk.clone())
    );
    assert_eq!(value(&test_box, &origin, "refs/heads/main"), Some(trunk));
}
