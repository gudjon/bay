//! G269. A landed remote head is pruned, and restorable.
//!
//! `bay status --branches` lists every branch on the remote with its tip, the time of its tip and
//! one class: the trunk, a `[[target]]` ref, a `[[lane]]` ref, `landed` with its proof
//! (`ancestor`: merged by fast-forward; `patch`: landed as an equal patch; `change`: its commit
//! carries a landed change's `Change-Id`), `stale` or `unique`. It deletes nothing and leaves no
//! census ref behind. `--prune` deletes the branches landed by `ancestor` or `patch` only: the
//! trunk, the target, the lane ref, the stale and the unique branch stay where they were, and so
//! does the branch landed by `change` alone, whose diff differs from what landed. Each pruned
//! tip is archived on the remote first (`refs/bay/archive/heads/<name>`), its restore line is in
//! `<common dir>/bay/pruned-heads.log`, and that line puts the branch back at its tip, here and
//! from another clone that fetched the archive. A landed branch that moved after the census is
//! refused by its lease and stays.
//!
//! Guards to mutate (each makes a case here fail):
//!
//! - `if row.proof == Some(Proof::Change)` in `prune`: drop it, and `claimed`, whose diff never
//!   landed, is pruned;
//! - the `archive(git, remote, &archived, &row.sha, on_remote)` call in `prune`: drop it, and
//!   the remote holds no archive ref and the restore line fails;
//! - `Class::kept` in `src/heads.rs`: return `None` for `Unique` (or `Stale`), and the unique
//!   (or stale) branch is pruned;
//! - the `[[target]]` and `[[lane]]` arms of `classify`: drop one, and `staging` (or
//!   `research/idea`), whose tip is on trunk, is classed landed and pruned;
//! - `ancestors.contains(name)` in `classify`: drop it, and `ff` is proven by `patch`, not
//!   `ancestor`;
//! - `plus.is_empty()` in `classify`: make it false, and the squash-landed branch is `unique`;
//! - the `append(log, …)` of the restore line before the delete in `prune`: drop it, and no
//!   restore line is logged;
//! - the `--force-with-lease=refs/heads/<name>:<sha>` of the delete: make it `--force`, and the
//!   branch moved after the census is deleted.

// The lease case runs a shell wrapper around git, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, result_json};

/// A target `staging` and a lane `research`, committed on trunk.
const CONFIG: &str = "main = \"main\"\n\n[[target]]\nname = \"staging\"\n\n[[lane]]\nname = \"research\"\nrefs = [\"refs/heads/research/*\"]\nwindow_s = 3600\n";

struct Fixture {
    test_box: TestBox,
    repo: PathBuf,
    remote: PathBuf,
    base: String,
    /// Merged into trunk by fast-forward.
    ff: String,
    /// Its one commit landed on trunk as an equal patch (another commit, the same diff).
    squashed: String,
    /// A commit trunk does not have, made now.
    unique: String,
    /// A commit trunk does not have, made in 2020.
    old: String,
    /// A commit whose diff trunk does not have, carrying the `Change-Id` of a landed change.
    claimed: String,
}

/// The landed change whose `Change-Id` `claimed` carries.
const LANDED_CHANGE: &str = "abcd2345";

/// Commits `file` on the branch checked out in `repo` (at `date` when given) and returns it.
fn commit(test_box: &TestBox, repo: &Path, file: &str, date: Option<&str>) -> String {
    std::fs::write(repo.join(file), format!("{file}\n")).unwrap();
    test_box.git_ok(repo, &["add", file]);
    let mut cmd = test_box.git(repo);
    if let Some(date) = date {
        cmd.env("GIT_COMMITTER_DATE", date)
            .env("GIT_AUTHOR_DATE", date);
    }
    let out = cmd.args(["commit", "-q", "-m", file]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    test_box.git_ok(repo, &["rev-parse", "HEAD"])
}

fn fixture() -> Fixture {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    TestBox::bay_config(&repo, CONFIG);
    let base = test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    // The target and the lane ref sit on trunk's tip: by ancestry alone they would be landed.
    test_box.git_ok(
        &repo,
        &[
            "push",
            "-q",
            "origin",
            "main:refs/heads/staging",
            "main:refs/heads/research/idea",
        ],
    );
    test_box.git_ok(&repo, &["switch", "-q", "-c", "squashed", "main"]);
    let squashed = commit(&test_box, &repo, "squashed.txt", None);
    test_box.git_ok(&repo, &["switch", "-q", "-c", "old", "main"]);
    let old = commit(&test_box, &repo, "old.txt", Some("2020-01-01T00:00:00Z"));
    test_box.git_ok(&repo, &["switch", "-q", "-c", "ff", "main"]);
    let ff = commit(&test_box, &repo, "ff.txt", None);
    test_box.git_ok(&repo, &["switch", "-q", "-c", "unique", "main"]);
    let unique = commit(&test_box, &repo, "unique.txt", None);
    test_box.git_ok(&repo, &["switch", "-q", "-c", "claimed", "main"]);
    std::fs::write(repo.join("claimed.txt"), "edits that never landed\n").unwrap();
    test_box.git_ok(&repo, &["add", "claimed.txt"]);
    test_box.git_ok(
        &repo,
        &[
            "commit",
            "-q",
            "-m",
            "claimed",
            "-m",
            &format!("Change-Id: {LANDED_CHANGE}"),
        ],
    );
    let claimed = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);
    test_box.git_ok(
        &repo,
        &[
            "push", "-q", "origin", "squashed", "old", "ff", "unique", "claimed",
        ],
    );
    // `ff` lands by fast-forward; `squashed` lands as a new commit with its patch, on a parent
    // it was not made on.
    test_box.git_ok(&repo, &["switch", "-q", "main"]);
    test_box.git_ok(&repo, &["merge", "-q", "--ff-only", "ff"]);
    test_box.git_ok(&repo, &["cherry-pick", &squashed]);
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);
    // The change `claimed` names has landed: its marker names trunk's tip.
    test_box.git_ok(
        &repo,
        &[
            "push",
            "-q",
            "origin",
            &format!("main:refs/bay/landed/{LANDED_CHANGE}"),
        ],
    );
    Fixture {
        test_box,
        repo,
        remote,
        base,
        ff,
        squashed,
        unique,
        old,
        claimed,
    }
}

impl Fixture {
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

    fn heads(&self, args: &[&str]) -> Value {
        let mut all = vec!["status", "--branches"];
        all.extend_from_slice(args);
        let (code, result) = result_json(self.test_box.bay(&self.repo).args(&all));
        assert_eq!(code, 0, "{result}");
        result
    }

    /// `bay prune`: the one command that deletes heads (G302).
    fn prune(&self) -> Value {
        let (code, result) = result_json(self.test_box.bay(&self.repo).arg("prune"));
        assert_eq!(code, 0, "{result}");
        result
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.repo.join(".git/bay/pruned-heads.log")).unwrap_or_default()
    }

    /// The tip of `name` on the remote, if it holds the ref.
    fn remote_ref(&self, name: &str) -> Option<String> {
        let out = self
            .test_box
            .git(self.test_box.path())
            .arg(format!("--git-dir={}", self.remote.display()))
            .args(["rev-parse", "-q", "--verify", name])
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8(out.stdout).unwrap().trim().to_owned())
    }

    /// No census ref is left in the clone.
    fn assert_no_census(&self) {
        let left = self
            .test_box
            .git_ok(&self.repo, &["for-each-ref", "refs/bay/census/"]);
        assert_eq!(left, "", "census refs left behind");
    }
}

fn row<'a>(result: &'a Value, name: &str) -> &'a Value {
    result["detail"]["branches"]
        .as_array()
        .unwrap_or_else(|| panic!("no detail.heads: {result}"))
        .iter()
        .find(|row| row["name"] == name)
        .unwrap_or_else(|| panic!("no head {name}: {result}"))
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
fn the_census_classes_every_remote_branch_and_deletes_nothing() {
    let fixture = fixture();
    let before = fixture.remote_heads();
    let result = fixture.heads(&[]);

    let trunk_tip = fixture
        .test_box
        .git_ok(&fixture.repo, &["rev-parse", "main"]);
    for (name, sha, class, proof) in [
        ("main", trunk_tip.as_str(), "main", Value::Null),
        ("staging", fixture.base.as_str(), "target", Value::Null),
        ("research/idea", fixture.base.as_str(), "lane", Value::Null),
        ("ff", fixture.ff.as_str(), "merged", "ancestor".into()),
        (
            "squashed",
            fixture.squashed.as_str(),
            "merged",
            "patch".into(),
        ),
        ("unique", fixture.unique.as_str(), "unique", Value::Null),
        ("old", fixture.old.as_str(), "stale", Value::Null),
        (
            "claimed",
            fixture.claimed.as_str(),
            "merged",
            "change".into(),
        ),
    ] {
        let head = row(&result, name);
        assert_eq!(head["sha"], sha, "{head}");
        assert_eq!(head["class"], class, "{head}");
        assert_eq!(head["proof"], proof, "{head}");
        assert!(head["newest_commit_at"].is_string(), "{head}");
    }
    assert_eq!(
        row(&result, "old")["newest_commit_at"],
        "2020-01-01T00:00:00Z"
    );
    assert_eq!(result["detail"]["counts"]["merged"], 3, "{result}");

    // A stale age past 2020 makes the old branch unique: never landed either way.
    let longer = fixture.heads(&["--stale-days", "100000"]);
    assert_eq!(row(&longer, "old")["class"], "unique", "{longer}");

    // Read only: the remote is as it was, nothing is logged, no census ref stays.
    assert_eq!(fixture.remote_heads(), before);
    assert_eq!(fixture.log(), "");
    fixture.assert_no_census();
}

#[test]
fn prune_deletes_only_the_landed_branches_and_the_log_restores_them() {
    let fixture = fixture();
    let before = fixture.remote_heads();
    let result = fixture.prune();

    assert_eq!(names(&result, "pruned"), ["ff", "squashed"], "{result}");
    assert_eq!(
        names(&result, "skipped"),
        [
            "claimed",
            "main",
            "old",
            "research/idea",
            "staging",
            "unique"
        ],
        "{result}"
    );
    let skipped = result["detail"]["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "claimed")
        .unwrap();
    assert_eq!(skipped["class"], "merged", "{skipped}");
    assert_eq!(skipped["proof"], "change", "{skipped}");
    assert_eq!(
        skipped["why"], "merged by Change-Id only; its commits may differ from what merged",
        "{skipped}"
    );
    let after = fixture.remote_heads();
    assert!(!after.contains_key("ff"), "{after:?}");
    assert!(!after.contains_key("squashed"), "{after:?}");
    for kept in [
        "main",
        "staging",
        "research/idea",
        "unique",
        "old",
        "claimed",
    ] {
        assert_eq!(after.get(kept), before.get(kept), "{kept} moved: {after:?}");
    }
    fixture.assert_no_census();

    // Each pruned tip is archived on the remote, so its objects outlive the branch.
    for (name, sha) in [("ff", &fixture.ff), ("squashed", &fixture.squashed)] {
        let archive = format!("refs/bay/archive/heads/{name}");
        assert_eq!(fixture.remote_ref(&archive).as_ref(), Some(sha), "{name}");
        let pruned = result["detail"]["pruned"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == name)
            .unwrap();
        assert_eq!(pruned["archive"], archive.as_str(), "{pruned}");
    }
    assert_eq!(fixture.remote_ref("refs/bay/archive/heads/claimed"), None);

    // One restore line per pruned branch: the push that puts it back, and the tip it had.
    let log = fixture.log();
    let lines: Vec<&str> = log.lines().collect();
    let ff_line = format!(
        "git push origin refs/bay/archive/heads/ff:refs/heads/ff  # {}",
        fixture.ff
    );
    let squashed_line = format!(
        "git push origin refs/bay/archive/heads/squashed:refs/heads/squashed  # {}",
        fixture.squashed
    );
    assert!(lines.contains(&ff_line.as_str()), "{log}");
    assert!(lines.contains(&squashed_line.as_str()), "{log}");
    assert_eq!(lines.len(), 2, "{log}");
    let command = |line: &str| -> Vec<String> {
        let command = line.split("  #").next().unwrap();
        let words: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
        assert_eq!(words[0], "git");
        words[1..].to_vec()
    };
    // Here, where the archive ref is held, the line runs as written.
    let ff_words = command(&ff_line);
    let ff_args: Vec<&str> = ff_words.iter().map(String::as_str).collect();
    fixture.test_box.git_ok(&fixture.repo, &ff_args);
    // In another clone, once it fetches the archive from the remote, the same line runs.
    let other = fixture.test_box.clone_of(&fixture.remote, "other");
    fixture.test_box.git_ok(
        &other,
        &[
            "fetch",
            "-q",
            "origin",
            "refs/bay/archive/heads/squashed:refs/bay/archive/heads/squashed",
        ],
    );
    let squashed_words = command(&squashed_line);
    let squashed_args: Vec<&str> = squashed_words.iter().map(String::as_str).collect();
    fixture.test_box.git_ok(&other, &squashed_args);
    let restored = fixture.remote_heads();
    assert_eq!(restored.get("ff"), Some(&fixture.ff), "{restored:?}");
    assert_eq!(
        restored.get("squashed"),
        Some(&fixture.squashed),
        "{restored:?}"
    );

    // A second prune finds them landed again; nothing else is ever touched.
    let again = fixture.prune();
    assert_eq!(names(&again, "pruned"), ["ff", "squashed"], "{again}");
}

#[test]
fn a_landed_branch_moved_after_the_census_is_not_deleted() {
    let fixture = fixture();
    // A new commit on `ff`, on the remote under a name that is no branch.
    fixture
        .test_box
        .git_ok(&fixture.repo, &["switch", "-q", "ff"]);
    let moved = commit(&fixture.test_box, &fixture.repo, "later.txt", None);
    fixture.test_box.git_ok(
        &fixture.repo,
        &[
            "push",
            "-q",
            "origin",
            &format!("{moved}:refs/scratch/moved"),
        ],
    );
    fixture
        .test_box
        .git_ok(&fixture.repo, &["switch", "-q", "main"]);
    // Another writer moves `ff` on the remote just before bay's delete of it is sent.
    let marker = fixture.test_box.path().join("moved-once");
    fixture.test_box.wrap_git(&format!(
        "case \" $* \" in\n  *\" --delete \"*\"refs/heads/ff \"*)\n    if [ ! -e '{marker}' ]; then\n      : > '{marker}'\n      \"$real\" --git-dir='{remote}' update-ref refs/heads/ff {moved}\n    fi\n    ;;\nesac",
        marker = marker.display(),
        remote = fixture.remote.display(),
    ));

    let result = fixture.prune();
    assert!(marker.exists(), "the delete of ff never ran: {result}");
    assert_eq!(names(&result, "pruned"), ["squashed"], "{result}");
    let skipped = result["detail"]["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "ff")
        .unwrap_or_else(|| panic!("ff not skipped: {result}"));
    assert_eq!(skipped["class"], "merged", "{skipped}");
    assert!(
        skipped["why"]
            .as_str()
            .is_some_and(|why| why.contains("refused")),
        "{skipped}"
    );
    assert_eq!(fixture.remote_heads().get("ff"), Some(&moved));
}

#[test]
fn prune_and_stale_days_need_heads() {
    let fixture = fixture();
    for args in [
        &["status", "--prune"][..],
        &["status", "--stale-days", "3"][..],
        &["status", "--branches", "--stranded"][..],
    ] {
        let out = fixture
            .test_box
            .bay(&fixture.repo)
            .args(args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
    }
}
