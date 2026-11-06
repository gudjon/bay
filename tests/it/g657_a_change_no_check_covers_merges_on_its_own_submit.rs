//! G657 (owner, 2026-11-05; a fleet finding: a dossier reached main 45 s after its request, 40 s of
//! them waiting for a runner to claim it). `bay merge` on a change no pre-merge check covers (G403)
//! pushes it itself: nothing is left for a runner to check. A machine that may not write as a
//! runner, as under a registry that signs everything, leaves it to one as before, and a change a
//! check covers still goes to a runner.
//!
//! G666 (2026-11-06, the owner's ruling): a repository with no checks is the same, since nothing is
//! left to check. Guard to mutate: `nothing_to_check` (the first case here stays queued, or with
//! the `.bay/` test gone the last merges).
//!
//! Guards to mutate, in `src/publish.rs` `submit_land`: the `needs_no_check` test (the first case
//! stays queued, or with it always true, the second merges); the `let Ok` that leaves a fault to
//! a runner (the third fails for want of a runner key).
//! And in `red_gate`, the read of the post records before it judges: without it, a push sees no red
//! another machine recorded (G577 finds the change on main).

// Each case runs a shell script (the check's adapter), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::talk::hosted_talk_seeded;
use support::{TestBox, check_files, passing_result, result_json};

/// One pre-merge check that reads only `src/`.
fn files() -> Vec<(String, String)> {
    let mut files = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            *text = text.replacen("timeout_s", "when_paths = [\"src/**\"]\ntimeout_s", 1);
        }
    }
    files
}

const SEED: &[(&str, &str)] = &[("src/a.rs", "fn a() {}\n"), ("docs/a.md", "a\n")];

fn seeded() -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, SEED, &files());
    (test_box, repo, remote)
}

fn edit(
    test_box: &TestBox,
    repo: &Path,
    intent: &str,
    paths: &str,
    file: &str,
) -> (String, PathBuf) {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", paths]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(file), "new\n").unwrap();
    (started["change"].as_str().unwrap().to_owned(), view)
}

#[test]
fn a_document_is_on_main_when_its_merge_returns() {
    let (test_box, repo, remote) = seeded();
    let (id, view) = edit(&test_box, &repo, "NOTE", "docs/", "docs/note.md");

    let (code, landed) = result_json(test_box.bay(&view).args(["merge"]));

    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert!(test_box.trunk_change_ids(&remote).contains(&id), "{landed}");
}

#[test]
fn a_change_a_check_covers_still_goes_to_a_runner() {
    let (test_box, repo, remote) = seeded();
    let (id, view) = edit(&test_box, &repo, "CODE", "src/", "src/b.rs");

    let (code, queued) = result_json(test_box.bay(&view).args(["merge"]));

    assert_eq!(code, 0, "{queued}");
    assert_eq!(queued["decision"], "queued", "{queued}");
    assert!(
        !test_box.trunk_change_ids(&remote).contains(&id),
        "{queued}"
    );
}

#[test]
fn a_machine_with_no_runner_key_leaves_it_to_a_runner() {
    let case = hosted_talk_seeded(|test_box, repo| {
        test_box.seed_with_check(repo, SEED, &files());
    });
    let started = {
        let (code, started) = result_json(
            case.bay_as("agent-b")
                .args(["start", "NOTE", "--paths", "docs/"]),
        );
        assert_eq!(code, 0, "{started}");
        started
    };
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let id = started["change"].as_str().unwrap().to_owned();
    std::fs::write(view.join("docs/note.md"), "new\n").unwrap();

    let (code, queued) = result_json(
        case.test_box
            .bay(&view)
            .env("BAY_ACTOR", "agent-b")
            .env("BAY_SESSION", "s-agent-b")
            .arg("merge"),
    );

    assert_eq!(code, 0, "{queued}");
    assert_eq!(queued["decision"], "queued", "{queued}");
    assert_eq!(queued["waiting_for"], "runner", "{queued}");
    assert!(!case.test_box.trunk_change_ids(&case.host).contains(&id));
}

#[test]
fn a_change_in_a_repository_with_no_checks_is_on_main_when_its_merge_returns() {
    // G666: nothing is left for a runner to check there either.
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &support::unchecked_files(),
    );
    let (id, view) = edit(&test_box, &repo, "NOTE", "src/", "src/note.rs");

    let (code, landed) = result_json(test_box.bay(&view).args(["merge"]));

    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert!(test_box.trunk_change_ids(&remote).contains(&id), "{landed}");
}

#[test]
fn a_change_to_the_policy_of_a_repository_with_no_checks_still_goes_to_a_runner() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &support::unchecked_files(),
    );
    let (_, view) = edit(&test_box, &repo, "POLICY", ".bay/", ".bay/note.txt");

    let (code, queued) = result_json(test_box.bay(&view).args(["merge"]));

    assert_eq!(code, 0, "{queued}");
    assert_eq!(queued["decision"], "queued", "{queued}");
}
