//! G321 (learned in a fleet rollout, 2026-10-22): each command says what it writes, and what it
//! never writes.
//!
//! An operator refused a safe step because nothing said what it would write. So every command
//! that is not hidden declares `writes` and `never` in `bay help --json`, one table in the code
//! (`contract::WRITES`), shown under each command on the `bay help` page. The read commands that
//! say they change nothing of yours are held to it: run in a repository, they move no branch, no
//! tag, no `HEAD`, and change no file of the work tree.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/contract.rs` `WRITES`: drop the `("prune", ...)` row (a command declares nothing).
//! - `src/contract.rs` `walk`: `"writes": writes.map(...)` to `"writes": null` (no command declares).
//! - `src/guide.rs` `section`: drop the `Writes:` line (the page shows nothing).

// The repository is seeded with a check script, so Unix only.
#![cfg(unix)]

use crate::support;

use support::TestBox;

#[test]
fn every_command_declares_what_it_writes_and_the_page_shows_it() {
    let test_box = TestBox::new();
    // `bay_ok` adds `--json`: this is `bay help --json`.
    let contract = test_box.bay_ok(test_box.path(), &["help"]);
    let commands = contract["detail"]["commands"].as_array().unwrap();
    let mut declared = 0;
    for command in commands {
        let path = command["command"].as_str().unwrap();
        if path.contains(' ') || command["hidden"] == true {
            continue;
        }
        for key in ["writes", "never"] {
            assert!(
                command[key]
                    .as_array()
                    .is_some_and(|items| !items.is_empty()),
                "bay {path} declares no {key}: {command}"
            );
        }
        declared += 1;
    }
    assert!(declared >= 20, "{declared}");
    let page = String::from_utf8(
        test_box
            .bay(test_box.path())
            .arg("help")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let prune = page
        .find("\n  bay prune")
        .unwrap_or_else(|| panic!("{page}"));
    let rest = &page[prune..];
    let writes = rest.find("Writes:").unwrap_or_else(|| panic!("{rest}"));
    let next = rest[1..].find("\n  bay ").map_or(rest.len(), |at| at + 1);
    assert!(writes < next, "bay prune shows no Writes line:\n{rest}");
}

/// The branches, tags and `HEAD` of `repo`, and its work tree's changes.
fn snapshot(test_box: &TestBox, repo: &std::path::Path) -> (String, String, String) {
    (
        test_box.git_ok(
            repo,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads",
                "refs/tags",
            ],
        ),
        test_box.git_ok(repo, &["rev-parse", "HEAD"]),
        test_box.git_ok(repo, &["status", "--porcelain"]),
    )
}

#[test]
fn the_read_commands_change_nothing_of_yours() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    std::fs::write(repo.join("notes.txt"), "mine\n").unwrap();
    let before = snapshot(&test_box, &repo);
    for args in [
        &["status"][..],
        &["waiting"],
        &["watch"],
        &["doctor"],
        &["help"],
    ] {
        let _ = test_box.bay(&repo).args(args).output().unwrap();
        assert_eq!(
            snapshot(&test_box, &repo),
            before,
            "bay {args:?} changed something"
        );
    }
    assert_eq!(
        std::fs::read_to_string(repo.join("notes.txt")).unwrap(),
        "mine\n"
    );
}
