//! G343 (found on a first real save, 2026-10-23): a view of the whole tree takes a new file.
//!
//! `bay start "<what>" --paths .` is an agent's most natural way to say "all of it". As a
//! sparse rule `/.` (or `/` for `--paths /`) matched no file, so the view took no new file: the
//! agent's `git add -A` refused it as outside the sparse definition, and its commit (which
//! main's message now comes from, G344) could not be made. `.` and
//! `/` now name the whole tree, which a change declares by naming no path: its view is a full
//! checkout, and a new file anywhere saves and merges.
//!
//! Guard to mutate: `src/start.rs` `declared_paths`: return `paths` unchanged (the new file is
//! refused).

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use support::{TestBox, result_json, unchecked_files};

#[test]
fn a_view_of_dot_or_slash_saves_a_new_file_anywhere() {
    for (spelling, file) in [(".", "docs/new.md"), ("/", "notes/plan.md")] {
        let test_box = TestBox::new();
        let repo = test_box.repo_any("repo");
        let remote = test_box.path().join("repo-remote.git");
        test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
        let started = test_box.bay_ok(&repo, &["start", "the whole tree", "--paths", spelling]);
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::create_dir_all(view.join(file).parent().unwrap()).unwrap();
        std::fs::write(view.join(file), "new\n").unwrap();
        // As the skill asks, the agent commits its work in the view before it merges.
        let added = test_box.git(&view).args(["add", "-A"]).output().unwrap();
        assert!(
            added.status.success(),
            "--paths {spelling}: git add refused the new file: {added:?}"
        );
        test_box.git_ok(&view, &["commit", "-q", "-m", "a new file"]);
        let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
        assert_eq!(code, 0, "--paths {spelling}: {merged}");
        assert_eq!(merged["decision"], "merged", "--paths {spelling}: {merged}");
        assert_eq!(
            test_box.git_bare(&remote, &["show", &format!("main:{file}")]),
            "new",
            "--paths {spelling}"
        );
    }
}
