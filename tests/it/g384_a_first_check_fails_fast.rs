//! G384 (a fleet finding, 2026-10-25: half the night's refused changes failed a check that takes
//! seconds, yet paid for every slow check, which started beside it). A first check fails fast.
//!
//! A check with `first = true` runs before every other check of the run, even with
//! `parallel_checks` above one, and a failure there ends the run: no other check starts. A
//! first check that passes lets the rest run as before.
//!
//! Guard to mutate: `src/check.rs` `run_all`: drop the `firsts` phase (the slow check starts
//! beside the first one, in parallel).

// Each case runs shell scripts (adapters, the relay), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

const HEAVY: &str = "\n[[check]]\nid = \"heavy\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check heavy\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n";

/// A trunk whose check `tests` is first and fails when `fails`, beside a check `heavy` that
/// notes each run of it. Returns the box, the repo and the note.
fn trunk(fails: bool) -> (TestBox, std::path::PathBuf, std::path::PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let ran = test_box.path().join("heavy-ran");
    let note = test_box.relay(&format!("exec >/dev/null; touch '{}'", ran.display()));
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let tests = if fails {
        format!("{{ {failing}; exit 0; }}")
    } else {
        ":".to_owned()
    };
    let mut files = check_files(
        // The relay is a function and a heredoc: each branch stands on lines of its own.
        &format!("if [ \"$1\" = heavy ]; then\n{note}\nelse\n{tests}\nfi"),
        &passing_result(1),
        1,
        60,
    );
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            *text = text.replacen("id = \"tests\"\n", "id = \"tests\"\nfirst = true\n", 1);
            text.push_str(HEAVY);
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\nparallel_checks = 4\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    (test_box, repo, ran)
}

#[test]
fn a_failed_first_check_ends_the_run_before_any_other_starts() {
    let (test_box, repo, ran) = trunk(true);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    assert!(
        !ran.exists(),
        "the slow check started beside a failed first check"
    );
}

#[test]
fn a_passing_first_check_lets_the_rest_run() {
    let (test_box, repo, ran) = trunk(false);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    assert!(ran.exists(), "the slow check never ran");
}
