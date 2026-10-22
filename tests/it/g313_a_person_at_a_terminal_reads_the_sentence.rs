//! G313 (owner ruling 2026-10-22, research ch. 35 R5 and R6): a person at a terminal reads plain
//! words first; a program always reads the document.
//!
//! When stdout is a terminal and `--json` is not given, bay prints the result's plain sentence
//! first, then its fields, and no JSON. Through a pipe, or with `--json`, stdout is the
//! `bay.result/1` document, unchanged. `bay --help` (and `-h`) with no command is the page
//! `bay help` prints (G303), grouped for people, agents and admins. A test holds no terminal,
//! so a debug build reads `BAY_TEST_STDOUT_TERMINAL` as one.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/main.rs` `render`: `if !json && stdout_is_terminal() {` to `if false {` (a terminal
//!   gets the document).
//! - `src/main.rs` `render`: `if !json && stdout_is_terminal() {` to `if stdout_is_terminal() {`
//!   (`--json` at a terminal gets plain text).
//! - `src/main.rs` `parse`: drop the `--help` branch (`bay --help` prints clap's list).

use crate::support;

use support::TestBox;

/// `bay <args>` in `dir`, its stdout as text, as a terminal would show it when `terminal`.
fn run(test_box: &TestBox, dir: &std::path::Path, args: &[&str], terminal: bool) -> String {
    let mut cmd = test_box.bay(dir);
    cmd.args(args);
    if terminal {
        cmd.env("BAY_TEST_STDOUT_TERMINAL", "1");
    }
    String::from_utf8(cmd.output().unwrap().stdout).unwrap()
}

#[test]
fn a_terminal_gets_the_sentence_and_a_pipe_or_json_gets_the_document() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);

    let piped = run(&test_box, &repo, &["status"], false);
    let document: serde_json::Value = serde_json::from_str(&piped)
        .unwrap_or_else(|error| panic!("a pipe got no document ({error}): {piped}"));
    let say = document["say"].as_str().unwrap().to_owned();

    let shown = run(&test_box, &repo, &["status"], true);
    assert!(
        serde_json::from_str::<serde_json::Value>(&shown).is_err(),
        "a terminal got the document: {shown}"
    );
    assert_eq!(shown.lines().next(), Some(say.as_str()), "{shown}");
    assert!(!shown.contains('{'), "{shown}");

    let asked = run(&test_box, &repo, &["status", "--json"], true);
    let asked: serde_json::Value = serde_json::from_str(&asked)
        .unwrap_or_else(|error| panic!("--json at a terminal got no document ({error}): {asked}"));
    assert_eq!(asked["say"], say.as_str());
}

#[test]
fn bay_help_and_bay_dash_dash_help_are_one_page() {
    let test_box = TestBox::new();
    let page = run(&test_box, test_box.path(), &["help"], false);
    assert!(page.contains("For people"), "{page}");
    assert_eq!(run(&test_box, test_box.path(), &["--help"], false), page);
    assert_eq!(run(&test_box, test_box.path(), &["-h"], false), page);
}
