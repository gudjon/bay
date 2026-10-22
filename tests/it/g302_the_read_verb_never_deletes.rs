//! G302. The read verb never deletes; one view shows every head.
//!
//! `bay prune` deletes the remote's branches landed by `ancestor` or `patch`, each archived on
//! the remote first and its restore line logged, exactly as `bay status --heads --prune` does:
//! both are one request through one function, so they prune the same branches, skip the same
//! ones and write the same restore lines. `bay status --heads` alone deletes nothing, and its
//! detail names, in `stranded`, the heads only this disk has: the local branches no
//! remote-tracking ref holds and the worktree records whose folder is gone, read from local refs
//! only. The older spellings still work: `bay status --stranded` gives its own result as before,
//! `bay status --heads --prune` prunes, and `bay help --json` lists `prune`.
//!
//! Guards to mutate (each makes a case here fail):
//!
//! - `src/main.rs` `main`: `Command::Prune { stale_days } => heads(&cli.dir, true, stale_days),`
//!   (make `true` `false`): `bay prune` deletes nothing, and the first case finds no pruned row;
//! - `src/main.rs` `status`: `return heads(dir, prune, stale_days);` (make `prune` `true`):
//!   `bay status --heads` deletes the landed branches, and the second case sees them gone;
//! - `src/heads.rs` `run`: `"stranded": crate::stranded::local(git)?,` (drop it): the second
//!   case's detail names no stranded branch or worktree;
//! - `src/stranded.rs` `run`: `detail["fetched"] = json!(false);` (drop it): the third case's
//!   `bay status --stranded` no longer says it never fetched.

// The box's git is a shell wrapper on PATH, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, result_json};

struct Fixture {
    test_box: TestBox,
    repo: PathBuf,
    remote: PathBuf,
    /// Merged into trunk by fast-forward.
    ff: String,
    /// Landed on trunk as an equal patch.
    squashed: String,
}

/// Commits `file` on the branch checked out in `repo` and returns it.
fn commit(test_box: &TestBox, repo: &Path, file: &str) -> String {
    std::fs::write(repo.join(file), format!("{file}\n")).unwrap();
    test_box.git_ok(repo, &["add", file]);
    test_box.git_ok(repo, &["commit", "-q", "-m", file]);
    test_box.git_ok(repo, &["rev-parse", "HEAD"])
}

/// A remote with two landed branches (`ff`, `squashed`) and one `unique`; in the clone, a
/// branch no remote holds (`local-only`) and a worktree record whose folder is gone.
fn fixture() -> Fixture {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    TestBox::bay_config(&repo, "trunk = \"main\"\n");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    test_box.git_ok(&repo, &["switch", "-q", "-c", "squashed", "main"]);
    let squashed = commit(&test_box, &repo, "squashed.txt");
    test_box.git_ok(&repo, &["switch", "-q", "-c", "ff", "main"]);
    let ff = commit(&test_box, &repo, "ff.txt");
    test_box.git_ok(&repo, &["switch", "-q", "-c", "unique", "main"]);
    commit(&test_box, &repo, "unique.txt");
    test_box.git_ok(&repo, &["push", "-q", "origin", "squashed", "ff", "unique"]);
    test_box.git_ok(&repo, &["switch", "-q", "main"]);
    test_box.git_ok(&repo, &["merge", "-q", "--ff-only", "ff"]);
    test_box.git_ok(&repo, &["cherry-pick", &squashed]);
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);
    // Only this disk holds `local-only`'s commit.
    test_box.git_ok(&repo, &["switch", "-q", "-c", "local-only", "main"]);
    commit(&test_box, &repo, "local.txt");
    test_box.git_ok(&repo, &["switch", "-q", "main"]);
    let gone = test_box.path().join("gone");
    test_box.git_ok(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "gone-branch",
            gone.to_str().unwrap(),
            "main",
        ],
    );
    std::fs::remove_dir_all(&gone).unwrap();
    Fixture {
        test_box,
        repo,
        remote,
        ff,
        squashed,
    }
}

impl Fixture {
    fn bay(&self, args: &[&str]) -> Value {
        let (code, result) = result_json(self.test_box.bay(&self.repo).args(args));
        assert_eq!(code, 0, "bay {args:?}: {result}");
        result
    }

    /// The remote's branches, by name, with their tips.
    fn remote_heads(&self) -> BTreeMap<String, String> {
        self.test_box
            .git_bare(
                &self.remote,
                &[
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    "refs/heads/",
                ],
            )
            .lines()
            .filter_map(|line| {
                let (name, sha) = line.split_once(' ')?;
                Some((name.strip_prefix("refs/heads/")?.to_owned(), sha.to_owned()))
            })
            .collect()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.repo.join(".git/bay/pruned-heads.log")).unwrap_or_default()
    }

    /// Runs each restore line of the log in the clone: the pruned branches are back.
    fn restore(&self) {
        for line in self.log().lines() {
            let command = line.split("  #").next().unwrap();
            let words: Vec<&str> = command.split_whitespace().collect();
            assert_eq!(words[0], "git", "{line}");
            self.test_box.git_ok(&self.repo, &words[1..]);
        }
    }
}

fn names(result: &Value, key: &str) -> Vec<String> {
    let mut names: Vec<String> = result["detail"][key]
        .as_array()
        .unwrap_or_else(|| panic!("no detail.{key}: {result}"))
        .iter()
        .map(|row| row["name"].as_str().unwrap().to_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn prune_deletes_what_status_heads_prune_deletes_with_the_same_archive_and_log() {
    let fixture = fixture();
    let before = fixture.remote_heads();

    let pruned = fixture.bay(&["prune", "--stale-days", "100000"]);
    assert_eq!(names(&pruned, "pruned"), ["ff", "squashed"], "{pruned}");
    assert_eq!(names(&pruned, "skipped"), ["main", "unique"], "{pruned}");
    assert_eq!(pruned["detail"]["stale_days"], 100_000, "{pruned}");
    let after = fixture.remote_heads();
    assert!(!after.contains_key("ff"), "{after:?}");
    assert!(!after.contains_key("squashed"), "{after:?}");
    assert_eq!(after.get("unique"), before.get("unique"), "{after:?}");
    for (name, sha) in [("ff", &fixture.ff), ("squashed", &fixture.squashed)] {
        let archive = format!("refs/bay/archive/heads/{name}");
        let held = fixture
            .test_box
            .git_bare(&fixture.remote, &["rev-parse", &archive]);
        assert_eq!(&held, sha, "{name}");
    }
    let first_log = fixture.log();
    let lines: Vec<&str> = first_log.lines().collect();
    assert_eq!(lines.len(), 2, "{first_log}");
    for (name, sha) in [("ff", &fixture.ff), ("squashed", &fixture.squashed)] {
        let line =
            format!("git push origin refs/bay/archive/heads/{name}:refs/heads/{name}  # {sha}");
        assert!(lines.contains(&line.as_str()), "{first_log}");
    }

    // Put both back from the log, then prune through the older spelling: the same rows, the
    // same archive refs reused, the same restore lines once more.
    fixture.restore();
    assert_eq!(fixture.remote_heads(), before);
    let alias = fixture.bay(&["status", "--heads", "--prune"]);
    assert_eq!(names(&alias, "pruned"), names(&pruned, "pruned"), "{alias}");
    assert_eq!(
        names(&alias, "skipped"),
        names(&pruned, "skipped"),
        "{alias}"
    );
    let archives = |result: &Value| -> Vec<String> {
        let mut archives: Vec<String> = result["detail"]["pruned"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["archive"].as_str().unwrap().to_owned())
            .collect();
        archives.sort();
        archives
    };
    assert_eq!(archives(&alias), archives(&pruned), "{alias}");
    assert_eq!(fixture.remote_heads(), after);
    let log = fixture.log();
    let mut second: Vec<&str> = log.lines().skip(2).collect();
    let mut first = lines;
    first.sort_unstable();
    second.sort_unstable();
    assert_eq!(second, first, "{log}");
}

#[test]
fn status_heads_deletes_nothing_and_names_the_stranded_heads_too() {
    let fixture = fixture();
    let before = fixture.remote_heads();
    let local_before = fixture
        .test_box
        .git_ok(&fixture.repo, &["for-each-ref", "refs/heads/"]);

    let result = fixture.bay(&["status", "--heads"]);
    assert_eq!(result["detail"]["counts"]["landed"], 2, "{result}");
    assert!(result["detail"].get("pruned").is_none(), "{result}");

    // The heads only this disk has, in their own field.
    let stranded = &result["detail"]["stranded"];
    let branches = stranded["branches"]
        .as_array()
        .unwrap_or_else(|| panic!("no detail.stranded.branches: {result}"));
    let named: Vec<&str> = branches
        .iter()
        .filter_map(|row| row["branch"].as_str())
        .collect();
    assert_eq!(named, ["local-only"], "{result}");
    assert_eq!(branches[0]["ahead"], 1, "{result}");
    let prunable = stranded["prunable_worktrees"]
        .as_array()
        .unwrap_or_else(|| panic!("no detail.stranded.prunable_worktrees: {result}"));
    assert_eq!(prunable.len(), 1, "{result}");
    assert_eq!(prunable[0]["branch"], "gone-branch", "{result}");

    // Nothing deleted: not on the remote, not here, and no restore line.
    assert_eq!(fixture.remote_heads(), before);
    assert_eq!(
        fixture
            .test_box
            .git_ok(&fixture.repo, &["for-each-ref", "refs/heads/"]),
        local_before
    );
    assert_eq!(fixture.log(), "");
    let records = fixture
        .test_box
        .git_ok(&fixture.repo, &["worktree", "list", "--porcelain"]);
    assert!(records.contains("prunable"), "{records}");
}

#[test]
fn the_older_spellings_still_work_and_help_lists_prune() {
    let fixture = fixture();

    // `--stranded` alone: its own result, as before.
    let stranded = fixture.bay(&["status", "--stranded"]);
    let detail = &stranded["detail"];
    assert_eq!(detail["fetched"], false, "{stranded}");
    assert!(detail["note"].is_string(), "{stranded}");
    assert!(detail.get("heads").is_none(), "{stranded}");
    assert_eq!(detail["branches"][0]["branch"], "local-only", "{stranded}");
    assert_eq!(
        detail["prunable_worktrees"][0]["branch"], "gone-branch",
        "{stranded}"
    );

    // `--prune` still needs `--heads`; `bay prune` needs nothing else.
    let out = fixture
        .test_box
        .bay(&fixture.repo)
        .args(["status", "--prune"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");

    let help = fixture.bay(&["help"]);
    let commands = help["detail"]["commands"]
        .as_array()
        .unwrap_or_else(|| panic!("no detail.commands: {help}"));
    let prune = commands
        .iter()
        .find(|command| command["command"] == "prune")
        .unwrap_or_else(|| panic!("bay help --json lists no prune: {help}"));
    assert_eq!(prune["hidden"], false, "{prune}");
    assert!(
        prune["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|arg| arg["long"] == "--stale-days"),
        "{prune}"
    );
}
