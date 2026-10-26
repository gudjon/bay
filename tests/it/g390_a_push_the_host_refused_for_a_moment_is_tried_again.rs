//! G390 (W10): a push of bay's records the host refuses for a moment is tried again before bay
//! calls it refused.
//!
//! A host can refuse a push of `refs/bay/*` for a reason of its own that clears a second later
//! (a hosting check it could not finish), and accept the very same push then. `bay start` tried
//! once and said the change could not be pushed; it now tries twice more, a second and three
//! seconds apart, and a host that refuses every time is still named with its own line.
//!
//! Guard to mutate, in `src/git.rs`: `host_refused` returning `false` (the first case fails).

#![cfg(unix)]

use crate::support;

use std::os::unix::fs::PermissionsExt as _;

use support::{TestBox, result_json, unchecked_files};

/// A remote whose pre-receive refuses each push of a change chain, the first `times` times.
fn refusing(test_box: &TestBox, times: u32) -> std::path::PathBuf {
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let remote = test_box.path().join("repo-remote.git");
    let count = test_box.path().join("refused");
    let hook = remote.join("hooks/pre-receive");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\nwhile read old new name; do\n  case \"$name\" in refs/bay/changes/*)\n    n=$(cat {count} 2>/dev/null || echo 0)\n    if [ \"$n\" -lt {times} ]; then echo $((n+1)) > {count}; echo \"error: the hosting check could not finish\" >&2; exit 1; fi;;\n  esac\ndone\nexit 0\n",
            count = count.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    repo
}

#[test]
fn a_start_the_host_refused_once_is_tried_again_and_succeeds() {
    let test_box = TestBox::new();
    let repo = refusing(&test_box, 1);
    let (code, started) = result_json(
        test_box
            .bay(&repo)
            .args(["start", "CARD-1", "--paths", "src/"]),
    );
    assert_eq!(code, 0, "{started}");
    assert!(started["detail"]["view"].is_string(), "{started}");
}

#[test]
fn a_host_that_refuses_every_time_is_named_with_its_own_line() {
    let test_box = TestBox::new();
    let repo = refusing(&test_box, 99);
    let (code, refused) = result_json(
        test_box
            .bay(&repo)
            .args(["start", "CARD-1", "--paths", "src/"]),
    );
    assert_ne!(code, 0, "{refused}");
    assert!(
        refused
            .to_string()
            .contains("the hosting check could not finish"),
        "{refused}"
    );
    assert_eq!(
        std::fs::read_to_string(test_box.path().join("refused"))
            .unwrap()
            .trim(),
        "3",
        "tried three times"
    );
}
