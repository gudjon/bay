//! G276 (N-19): a prune adapter runs after each delete.
//!
//! A team on a hosted remote wants more than the branch gone when `bay status --heads --prune`
//! deletes it: the branch's open pull request closed, for example. bay never talks to a host.
//! `[heads] on_prune` in `.bay/config.toml` names an adapter, a program path relative to the
//! work tree; after each branch the prune deletes, bay runs it as
//! `<on_prune> pruned <branch> <sha> <archive ref>`, its program materialized from the trunk's
//! tip (never this machine's work tree), within `[heads] timeout_s`. Its outcome is recorded on
//! the branch's `pruned` row as `on_prune: {ok, why}`, `why` naming the exit status and the
//! first line of its stderr. A failing adapter never undoes the delete and never stops the
//! loop: the next branch is pruned and its adapter runs. Without `[heads] on_prune`, nothing
//! runs and no row carries `on_prune`.
//!
//! Guards to mutate (each makes a case here fail), in `src/heads.rs`:
//!
//! - the `if let Some((command, timeout)) = adapter {` block after a delete in `prune`: drop
//!   it, and the adapter never runs, so its log is empty and no row carries `on_prune`;
//! - `Ok((status, _, _)) if status.success() => json!({"ok": true, "why": null})` in
//!   `on_prune`: answer `"ok": false`, and the passing adapter of `b-second` reads as failed;
//!
//! and in `src/hold.rs`, `run_in`, which the hold adapter shares:
//!
//! - `crate::run::command(scratch, command)`: pass `&repo.work_root` for `scratch`, and
//!   the uncommitted work-tree copy of the adapter runs in place of the trunk's;
//! - `if !scratch.join(program).is_file()`: drop it, and the missing adapter's `why` no longer
//!   names that it is not in trunk.

// Each case runs a shell script (the prune adapter), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files, write_executable};

struct Fixture {
    test_box: TestBox,
    repo: PathBuf,
    remote: PathBuf,
    /// Landed on trunk by fast-forward, first in name order.
    first: String,
    /// Landed on trunk by fast-forward, after `a-first`.
    second: String,
    /// Where the adapter writes each call's arguments.
    calls: PathBuf,
}

/// Commits `file` on the branch checked out in `repo` and returns the commit.
fn commit(test_box: &TestBox, repo: &Path, file: &str) -> String {
    std::fs::write(repo.join(file), format!("{file}\n")).unwrap();
    test_box.git_ok(repo, &["add", file]);
    test_box.git_ok(repo, &["commit", "-q", "-m", file]);
    test_box.git_ok(repo, &["rev-parse", "HEAD"])
}

/// A trunk whose config is `config`, with the adapter at `scripts/on-prune` committed when
/// `adapter` is set: it logs its arguments and fails for the branch `a-first`. Two branches,
/// `a-first` and `b-second` on it, are pushed and land on trunk by fast-forward.
fn fixture(config: &str, adapter: bool) -> Fixture {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let calls = test_box.path().join("on-prune-calls.log");
    if adapter {
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{calls}'\nif [ \"$2\" = a-first ]; then\n  \
             echo 'the host refused: no such pull request' >&2\n  echo 'a second line' >&2\n  \
             exit 3\nfi\nexit 0\n",
            calls = calls.display()
        );
        let full = repo.join("scripts/on-prune");
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        write_executable(&full, &script);
    }
    let mut files = unchecked_files();
    files.push((".bay/config.toml".to_owned(), config.to_owned()));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);

    test_box.git_ok(&repo, &["switch", "-q", "-c", "a-first", "main"]);
    let first = commit(&test_box, &repo, "first.txt");
    test_box.git_ok(&repo, &["switch", "-q", "-c", "b-second", "a-first"]);
    let second = commit(&test_box, &repo, "second.txt");
    test_box.git_ok(&repo, &["push", "-q", "origin", "a-first", "b-second"]);
    test_box.git_ok(&repo, &["switch", "-q", "main"]);
    test_box.git_ok(&repo, &["merge", "-q", "--ff-only", "b-second"]);
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);
    if adapter {
        // An uncommitted copy in this work tree is never the one that runs.
        write_executable(
            &repo.join("scripts/on-prune"),
            &format!(
                "#!/bin/sh\necho 'the work tree copy' >> '{}'\nexit 0\n",
                calls.display()
            ),
        );
    }
    Fixture {
        test_box,
        repo,
        remote,
        first,
        second,
        calls,
    }
}

impl Fixture {
    fn prune(&self) -> Value {
        let (code, result) = result_json(
            self.test_box
                .bay(&self.repo)
                .args(["status", "--heads", "--prune"]),
        );
        assert_eq!(code, 0, "{result}");
        result
    }

    /// The branches the remote holds.
    fn remote_heads(&self) -> Vec<String> {
        self.test_box
            .git_bare(
                &self.remote,
                &["for-each-ref", "--format=%(refname:strip=2)", "refs/heads/"],
            )
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(&self.calls).unwrap_or_default()
    }
}

fn pruned<'a>(result: &'a Value, name: &str) -> &'a Value {
    result["detail"]["pruned"]
        .as_array()
        .unwrap_or_else(|| panic!("no detail.pruned: {result}"))
        .iter()
        .find(|row| row["name"] == name)
        .unwrap_or_else(|| panic!("{name} not pruned: {result}"))
}

#[test]
fn the_adapter_runs_once_per_deleted_branch_and_a_failure_undoes_nothing() {
    let fixture = fixture(
        "trunk = \"main\"\n\n[heads]\non_prune = \"scripts/on-prune\"\ntimeout_s = 20\n",
        true,
    );
    let result = fixture.prune();

    // Both are deleted: the failing adapter of the first neither restored it nor stopped the
    // loop before the second.
    assert_eq!(fixture.remote_heads(), ["main"], "{result}");
    let first = pruned(&result, "a-first");
    let second = pruned(&result, "b-second");

    // The trunk's copy ran, once per branch, after its delete, with the branch, its tip and
    // its archive ref.
    let calls = fixture.calls();
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        [
            format!(
                "pruned a-first {} refs/bay/archive/heads/a-first",
                fixture.first
            ),
            format!(
                "pruned b-second {} refs/bay/archive/heads/b-second",
                fixture.second
            ),
        ],
        "{calls}"
    );
    assert!(!calls.contains("the work tree copy"), "{calls}");

    // Each outcome is on its row: the failure names its exit status and its first stderr line.
    assert_eq!(
        first["archive"], "refs/bay/archive/heads/a-first",
        "{first}"
    );
    assert_eq!(first["on_prune"]["ok"], false, "{first}");
    let why = first["on_prune"]["why"].as_str().unwrap_or_default();
    assert!(why.contains('3'), "names the exit status: {first}");
    assert!(
        why.contains("the host refused: no such pull request"),
        "{first}"
    );
    assert!(
        !why.contains("a second line"),
        "the first line only: {first}"
    );
    assert_eq!(
        second["on_prune"],
        serde_json::json!({"ok": true, "why": null}),
        "{second}"
    );

    // The archive refs stay, so a failed adapter leaves each branch restorable as before.
    for name in ["a-first", "b-second"] {
        let held = fixture.test_box.git_bare(
            &fixture.remote,
            &[
                "for-each-ref",
                "--format=%(objectname)",
                &format!("refs/bay/archive/heads/{name}"),
            ],
        );
        assert!(!held.is_empty(), "{name} archived");
    }
}

#[test]
fn without_an_adapter_nothing_runs_and_a_missing_one_is_recorded() {
    // No `[heads] on_prune`: the prune is as it was, and no row carries an outcome.
    let plain = fixture("trunk = \"main\"\n", false);
    let result = plain.prune();
    assert_eq!(plain.remote_heads(), ["main"], "{result}");
    for name in ["a-first", "b-second"] {
        let row = pruned(&result, name);
        assert!(row.get("on_prune").is_none(), "{row}");
    }
    assert_eq!(plain.calls(), "");

    // An adapter the trunk does not hold is a failure on each row, and the prune goes on.
    let missing = fixture(
        "trunk = \"main\"\n\n[heads]\non_prune = \"scripts/not-here\"\n",
        false,
    );
    let result = missing.prune();
    assert_eq!(missing.remote_heads(), ["main"], "{result}");
    for name in ["a-first", "b-second"] {
        let row = pruned(&result, name);
        assert_eq!(row["on_prune"]["ok"], false, "{row}");
        let why = row["on_prune"]["why"].as_str().unwrap_or_default();
        assert!(why.contains("not in trunk"), "{row}");
    }
}
