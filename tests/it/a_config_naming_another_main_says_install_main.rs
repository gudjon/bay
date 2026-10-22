//! 0.3.1: a config that names another main than this clone's says the flag that sets it,
//! `bay install --main` (0.3.0 renamed `--trunk`; the hint still named the old flag).
//!
//! Guard to mutate: `src/repo.rs` `open_with`: `if key == "trunk" { "main" } else { key }` to
//! `key` (the hint says `--trunk`).

use crate::support;

use std::path::PathBuf;

use support::{TestBox, check_files, passing_result, result_json};

/// A repository whose main holds `config` as its `.bay/config.toml`, and one required check.
fn seeded(test_box: &TestBox, name: &str, config: &str) -> PathBuf {
    let repo = test_box.repo_any(name);
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((".bay/config.toml".to_owned(), config.to_owned()));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    repo
}

#[test]
fn a_config_naming_another_main_names_install_main() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box, "repo", "trunk = \"other\"\n");
    let (_, status) = result_json(test_box.bay(&repo).arg("status"));
    // `detail` holds the command as typed; `say` turns every word into the plain one anyway.
    assert!(
        status["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("bay install --main other")),
        "{status}"
    );
}
