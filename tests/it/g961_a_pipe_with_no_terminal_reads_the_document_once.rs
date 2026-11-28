//! G961 (owner ruling 2026-11-28): bay writes its human copy on stderr only for a person
//! who may read it live.
//!
//! Through a pipe, stdout is the `bay.result/1` document (G313). bay wrote the same result again
//! as plain text on stderr, so a program that captured both streams read it twice. Now that copy
//! goes out only when stderr is a terminal; `--json` still writes none. A test holds no
//! terminal, so a debug build reads `BAY_TEST_STDERR_TERMINAL` as one.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/main.rs` `shown`: `err: if json || !stderr_terminal {` to `err: if json {` (a pipe
//!   with no terminal on stderr gets the copy).
//! - `src/main.rs` `shown`: `err: if json || !stderr_terminal {` to `err: if true {` (a
//!   terminal on stderr gets no copy).

use crate::support;

use support::TestBox;

/// `bay <args>` in `dir` through a pipe: (stdout, stderr), stderr shown as a terminal when
/// `terminal`.
fn run(
    test_box: &TestBox,
    dir: &std::path::Path,
    args: &[&str],
    terminal: bool,
) -> (String, String) {
    let mut cmd = test_box.bay(dir);
    cmd.args(args);
    if terminal {
        cmd.env("BAY_TEST_STDERR_TERMINAL", "1");
    }
    let out = cmd.output().unwrap();
    (
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

#[test]
fn a_pipe_with_no_terminal_reads_the_document_once() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);

    let (piped, quiet) = run(&test_box, &repo, &["status"], false);
    let document: serde_json::Value = serde_json::from_str(&piped)
        .unwrap_or_else(|error| panic!("a pipe got no document ({error}): {piped}"));
    let say = document["say"].as_str().unwrap().to_owned();
    assert!(
        !quiet.contains(&say),
        "no terminal read stderr, yet it got the copy: {quiet}"
    );

    let (shown, copy) = run(&test_box, &repo, &["status"], true);
    let shown: serde_json::Value = serde_json::from_str(&shown)
        .unwrap_or_else(|error| panic!("stdout is no longer the document ({error}): {shown}"));
    assert_eq!(shown["say"], say.as_str());
    assert!(
        copy.contains(&say),
        "a terminal on stderr got no copy: {copy}"
    );

    let (asked, none) = run(&test_box, &repo, &["status", "--json"], true);
    let asked: serde_json::Value = serde_json::from_str(&asked)
        .unwrap_or_else(|error| panic!("--json got no document ({error}): {asked}"));
    assert_eq!(asked["say"], say.as_str());
    assert!(!none.contains(&say), "--json wrote the copy: {none}");
}
