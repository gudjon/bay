//! G449 (BACKLOG 117, a fleet finding, 2026-10-28). A post check that no worker answers waits,
//! and the next post check runs.
//!
//! The one runner that served the post environments ran one post check a pass, the one it ran
//! longest ago first. A check whose worker could not start its ticket (a drained box) answered
//! `unavailable` after 383-400 s and wrote no record, so it stayed the one run longest ago and
//! came first in every pass: the other post checks were silent for two hours. Now a check no
//! worker answered waits an hour on that box before it is tried again, and its record, from any
//! later run, ends the wait.
//!
//! The guards to mutate, in `src/bisect.rs`: the `waits_for_a_worker` skip in `tip_runs` (the
//! next pass tries the same check, or stops on it), and the `note_unavailable` of an
//! unavailable run in `run_at` (nothing waits).

// Each case runs shell scripts (the adapter), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

/// Two post checks on `src/`: `away`, which no worker answers, first, then `ready`.
const POST: &str = "\n[[check]]\nid = \"away\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n\n[[check]]\nid = \"ready\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

/// The adapter's answer for `away`: no worker answers it.
const AWAY: &str = r#"case "$req" in *'"plan":{"id":"away"'*) printf '%s\n' '{"schema":"bay.check.result/1","unavailable":"no worker can start the ticket"}'; exit 0 ;; esac"#;

/// Every evidence record in `repo`'s notes, as one text.
fn records(test_box: &TestBox, repo: &Path) -> String {
    test_box.git_ok(
        repo,
        &["fetch", "-q", "origin", "+refs/notes/*:refs/notes/*"],
    );
    let notes = test_box.git_ok(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/notes/bay/evidence/",
        ],
    );
    notes
        .lines()
        .flat_map(|notes_ref| {
            test_box
                .git_ok(
                    repo,
                    &["ls-tree", "-r", "--format=%(objectname)", notes_ref],
                )
                .lines()
                .map(|blob| test_box.git_ok(repo, &["cat-file", "blob", blob]))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn a_post_check_no_worker_answers_waits_and_the_next_one_runs() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let mut files = check_files(AWAY, &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(POST);
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\npost = true\nbisect_post = false\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let started = test_box.bay_ok(&repo, &["start", "one", "--paths", "src"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/one.rs"), "fn one() {}\n").unwrap();
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    // Two passes: the first tries `away`, which no worker answers; the second runs `ready`.
    let first = test_box.bay_ok(&repo, &["runner", "run", "--once"]);
    let second = test_box.bay_ok(&repo, &["runner", "run", "--once"]);
    let text = records(&test_box, &repo);
    assert!(
        text.contains("\"id\": \"ready\"") || text.contains("\"id\":\"ready\""),
        "the post check after the unavailable one never ran: {first}\n{second}"
    );
    assert!(
        !text.contains("\"id\": \"away\"") && !text.contains("\"id\":\"away\""),
        "an unavailable check wrote a record: {text}"
    );
}
