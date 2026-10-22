//! G155. Stale instructions are named at `SessionStart`.
//!
//! A session that starts on a tree behind trunk on its instruction files follows rules trunk
//! has already changed. `bay hook session.start` names the files (`[agent] instructions` in the
//! config, default `AGENTS.md` and `CLAUDE.md`) and how many trunk commits since the tree's HEAD
//! changed them, from local refs only (the trunk tracking ref; no fetch). The notice is given
//! once per session for one trunk state, and recorded on the session's line.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use serde_json::{Value, json};
use support::TestBox;

/// `bay hook session.start` for `session` in `cwd`; returns its stdout.
fn start(test_box: &TestBox, cwd: &Path, session: &str) -> String {
    let payload = json!({"session_id": session, "hook_event_name": "SessionStart",
        "source": "startup", "cwd": cwd});
    let mut child = test_box
        .bay_process(cwd)
        .args(["hook", "session.start"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

fn last_line(repo: &Path, session: &str) -> Value {
    let path = repo.join(format!(".git/bay/sessions/{session}.jsonl"));
    let text = std::fs::read_to_string(&path).unwrap();
    serde_json::from_str(text.lines().last().unwrap()).unwrap()
}

/// Commits `files` on the checkout and pushes them to trunk.
fn trunk_commit(test_box: &TestBox, repo: &Path, files: &[(&str, &str)]) {
    test_box.seed_trunk(repo, files);
}

/// A repository whose checkout is at trunk's first commit, with trunk two commits ahead on
/// `AGENTS.md`, one on `CLAUDE.md` (the same commit as one of them), and one on code only.
fn behind() -> (TestBox, std::path::PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let base = test_box.seed_trunk(
        &repo,
        &[
            ("AGENTS.md", "rule one\n"),
            ("CLAUDE.md", "see AGENTS.md\n"),
            ("src/a.rs", "fn a() {}\n"),
        ],
    );
    trunk_commit(&test_box, &repo, &[("AGENTS.md", "rule one\nrule two\n")]);
    trunk_commit(&test_box, &repo, &[("src/a.rs", "fn a() { }\n")]);
    trunk_commit(
        &test_box,
        &repo,
        &[
            ("AGENTS.md", "rule one\nrule two\nrule three\n"),
            ("CLAUDE.md", "see AGENTS.md, always\n"),
        ],
    );
    test_box.git_ok(&repo, &["reset", "-q", "--hard", &base]);
    // The checkout's own commit on an instruction file is not trunk's: it is not counted.
    std::fs::write(repo.join("CLAUDE.md"), "local\n").unwrap();
    test_box.git_ok(&repo, &["commit", "-q", "-am", "local"]);
    (test_box, repo)
}

#[test]
fn a_tree_behind_trunk_on_its_instructions_is_told_once() {
    let (test_box, repo) = behind();
    let out = start(&test_box, &repo, "s-1");
    let line = out
        .lines()
        .find(|line| line.contains("instruction"))
        .unwrap_or_else(|| panic!("no notice: {out}"));
    assert!(line.contains("AGENTS.md (2 trunk commits)"), "{line}");
    assert!(line.contains("CLAUDE.md (1 trunk commit)"), "{line}");
    let recorded = last_line(&repo, "s-1");
    let behind = &recorded["instructions_behind"];
    assert_eq!(behind["files"]["AGENTS.md"], 2, "{recorded}");
    assert_eq!(behind["files"]["CLAUDE.md"], 1, "{recorded}");
    assert!(behind["trunk"].is_string(), "{recorded}");

    // Once: the same session starting again (a resume) on the same trunk is not told again.
    let again = start(&test_box, &repo, "s-1");
    assert!(!again.contains("instruction"), "{again}");
    assert!(last_line(&repo, "s-1").get("instructions_behind").is_none());
    // Another session is told.
    assert!(start(&test_box, &repo, "s-2").contains("AGENTS.md (2 trunk commits)"));
}

#[test]
fn a_tree_at_trunk_is_told_nothing() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(
        &repo,
        &[("AGENTS.md", "rules\n"), ("src/a.rs", "fn a() {}\n")],
    );
    // Trunk moved on code only.
    trunk_commit(&test_box, &repo, &[("src/a.rs", "fn a() { }\n")]);
    let out = start(&test_box, &repo, "s-1");
    assert!(!out.contains("instruction"), "{out}");
    assert!(last_line(&repo, "s-1").get("instructions_behind").is_none());
}

#[test]
fn the_config_names_the_instruction_files() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let base = test_box.seed_trunk(
        &repo,
        &[("AGENTS.md", "rules\n"), ("docs/RULES.md", "one\n")],
    );
    test_box.commit_config(
        &repo,
        "trunk = \"main\"\n\n[agent]\nproduct = \"claude\"\ninstructions = [\"docs/RULES.md\"]\n",
    );
    test_box.git_ok(&repo, &["pull", "-q", "--ff-only", "origin", "main"]);
    trunk_commit(
        &test_box,
        &repo,
        &[
            ("AGENTS.md", "rules, changed\n"),
            ("docs/RULES.md", "two\n"),
        ],
    );
    test_box.git_ok(&repo, &["reset", "-q", "--hard", &base]);
    let out = start(&test_box, &repo, "s-1");
    let line = out
        .lines()
        .find(|line| line.contains("instruction"))
        .unwrap_or_else(|| panic!("no notice: {out}"));
    assert!(line.contains("docs/RULES.md (1 trunk commit)"), "{line}");
    assert!(!line.contains("AGENTS.md"), "{line}");
}
