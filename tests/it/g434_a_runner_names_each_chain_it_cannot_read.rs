//! G434. A runner names each change chain it cannot read.
//!
//! A pass reads every change chain from the remote; a chain it cannot read is a submitted change
//! no runner places. bay dropped the fetch's error, so fourteen such changes went unnamed for
//! hours (2026-10-28, G432's cause). Now each chain a pass could not read, even alone, is named
//! in the runner's journal with Git's words: `bay runner: <change> could not be read from the
//! remote: <why>`.
//!
//! The guard to mutate: the loop over `fetch_chains`' unread chains in `executor::pass`
//! (`src/executor.rs`); drop it and the change goes unnamed.

use crate::support;

use std::path::PathBuf;

use support::{TestBox, result_json, unchecked_files};

#[test]
fn a_chain_no_fetch_can_read_is_named_in_the_journal() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let executor = test_box.clone_of(&remote, "executor");
    let started = test_box.bay_ok(&repo, &["start", "unreadable", "--paths", "src"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let text = "fn unreadable() {}\n";
    std::fs::write(view.join("src/b.rs"), text).unwrap();
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let id = queued["change"].as_str().unwrap().to_owned();
    // The remote loses the change's one new blob, so no fetch of its chain can complete.
    let file = test_box.path().join("b.rs");
    std::fs::write(&file, text).unwrap();
    let blob = test_box.git_ok(&repo, &["hash-object", file.to_str().unwrap()]);
    let blob = blob.trim();
    let loose = remote.join("objects").join(&blob[..2]).join(&blob[2..]);
    assert!(
        loose.exists(),
        "the pushed blob is loose in the remote: {}",
        loose.display()
    );
    std::fs::remove_file(&loose).unwrap();
    let out = test_box
        .bay(&executor)
        .args(["runner", "run", "--once"])
        .output()
        .unwrap();
    let journal = String::from_utf8_lossy(&out.stderr);
    assert!(
        journal.contains(&format!(
            "bay runner: {id} could not be read from the remote: "
        )),
        "the change no pass can read is unnamed: {journal}"
    );
}
