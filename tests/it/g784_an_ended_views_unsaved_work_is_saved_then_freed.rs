//! G784 (owner ruling, 2026-11-18: "save then free"; a Mac held about 4 GB in views of ended
//! changes kept only for the unsaved work in them). A view of an ended change past its
//! retention window that holds unsaved work — a tracked edit, an untracked file — has that work
//! saved in one commit of bay's own under the local ref `refs/bay/view-kept/<change>`, read
//! back, and then goes. Files its ignore rules match are build output and are not saved. A view
//! whose save cannot be read back stays, a live change's view stays, and a copy `bay start` set
//! aside as unsaved goes the same way under `refs/bay/view-kept/<change>.unsaved-<seconds>`.
//!
//! Guard to mutate: in `src/view.rs` `release`, make the save-then-free branch never save
//! (`saved_work` answering `None`), and the merged change's view stays.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

fn seeded(test_box: &TestBox) -> PathBuf {
    let repo = test_box.repo_any("repo");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[retention]\nwindow_s = 0\n".to_owned(),
    ));
    files.push((".gitignore".to_owned(), "build/\n".to_owned()));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    repo
}

/// Starts, edits and merges change `name`. Returns (id, view).
fn merged(test_box: &TestBox, repo: &Path, name: &str) -> (String, PathBuf) {
    let started = test_box.bay_ok(repo, &["start", &format!("CARD-{name}"), "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(
        view.join(format!("src/{name}.rs")),
        format!("fn {name}() {{}}\n"),
    )
    .unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    (id, view)
}

/// The paths of the tree of `rev` in `repo`.
fn paths(test_box: &TestBox, repo: &Path, rev: &str) -> Vec<String> {
    test_box
        .git_ok(repo, &["ls-tree", "-r", "--name-only", rev])
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn an_ended_views_edits_and_new_files_are_kept_under_a_ref_and_the_view_goes() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    let (done, done_view) = merged(&test_box, &repo, "done");
    // GIVEN, after the merge, a tracked edit, an untracked file and an ignored build directory
    std::fs::write(done_view.join("src/done.rs"), "fn done() { edited() }\n").unwrap();
    std::fs::write(done_view.join("src/notes.txt"), "mine\n").unwrap();
    std::fs::create_dir_all(done_view.join("src/build")).unwrap();
    std::fs::write(done_view.join("src/build/out.bin"), "regenerable\n").unwrap();
    // AND a copy an earlier start set aside as unsaved, long ago: a whole view, its ignore
    // rules with it
    let home = done_view.parent().unwrap().to_path_buf();
    let aside = home.join(format!("{done}.unsaved-1000"));
    std::fs::create_dir_all(aside.join("build")).unwrap();
    std::fs::write(aside.join(".gitignore"), "build/\n").unwrap();
    std::fs::write(aside.join("kept.txt"), "set aside\n").unwrap();
    std::fs::write(aside.join("build/x"), "regenerable\n").unwrap();
    // AND a live change with an edit of its own
    let live = test_box.bay_ok(&repo, &["start", "CARD-live", "--paths", "src/"]);
    let live_view = PathBuf::from(live["detail"]["view"].as_str().unwrap());
    std::fs::write(live_view.join("src/live.rs"), "fn live() {}\n").unwrap();

    // WHEN the next change starts, which releases views of ended changes
    let _ = test_box.bay_ok(&repo, &["start", "CARD-NEXT", "--paths", "src/"]);

    // THEN the ended change's view is gone and its work is kept by a ref of bay's
    assert!(!done_view.exists(), "the view of {done} stayed");
    let kept = format!("refs/bay/view-kept/{done}");
    let saved = paths(&test_box, &repo, &kept);
    assert!(saved.contains(&"src/notes.txt".to_owned()), "{saved:?}");
    assert!(
        !saved.iter().any(|path| path.contains("build/")),
        "{saved:?}"
    );
    let edited = test_box.git_ok(&repo, &["show", &format!("{kept}:src/done.rs")]);
    assert_eq!(edited.trim(), "fn done() { edited() }");
    let author = test_box.git_ok(&repo, &["log", "-1", "--format=%ae", &kept]);
    assert_eq!(author.trim(), "bay@bay.invalid");
    // AND the set-aside copy is gone, its file kept, its build output not
    assert!(!aside.exists(), "the set-aside copy stayed");
    let aside_paths = paths(&test_box, &repo, &format!("{kept}.unsaved-1000"));
    assert_eq!(
        aside_paths,
        vec![".gitignore".to_owned(), "kept.txt".to_owned()],
        "{aside_paths:?}"
    );
    // AND the live change's view stays, untouched
    assert!(
        live_view.join("src/live.rs").exists(),
        "the live view lost its work"
    );
}

#[test]
fn a_view_whose_save_cannot_be_read_back_stays() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    let (done, view) = merged(&test_box, &repo, "done");
    // GIVEN an untracked file no one can read, so Git cannot save it
    let locked = view.join("src/locked.txt");
    std::fs::write(&locked, "unreadable\n").unwrap();
    std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();

    // WHEN the next change starts
    let _ = test_box.bay_ok(&repo, &["start", "CARD-NEXT", "--paths", "src/"]);

    // THEN the view stays, with its file, and no ref claims to keep it
    std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    assert!(
        locked.exists(),
        "the view of {done} was freed though its save failed"
    );
    let kept = test_box
        .git(&repo)
        .args([
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/bay/view-kept/{done}"),
        ])
        .output()
        .unwrap();
    assert!(
        kept.stdout.is_empty(),
        "a ref kept a save that failed: {}",
        String::from_utf8_lossy(&kept.stdout)
    );
}
