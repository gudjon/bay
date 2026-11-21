//! G615 (a fleet finding, 2026-10-31: on a hosted remote a `bay status` spent about 7 s of 12 to
//! 19 s on the network: the trunk fetch alone first, then eight fetches and listings beside each
//! other, three of its sets two round trips one after another). A status asks the remote once:
//! one `git fetch --dry-run` of every namespace it judges, which writes no ref, and then only the
//! moves each namespace's own fetch would take are made. Read from the trace a test build writes
//! when `BAY_TEST_GIT_TRACE` names a file (G171): a chain the remote moved forward, the trunk and
//! a target copy the remote no longer holds move as they did; a chain ahead of the remote's that
//! the remote will not take (G289) and this store's own evidence notes are kept; and a status
//! with nothing moved writes no ref.
//!
//! Guards to mutate, in `src/status.rs`: in `fetch_swept`, never sweep (`None`), and the status
//! asks the remote many times; in `settle_swept`, sync the receipts with no sweep, and they list
//! the remote again; in `settle_listed`, take a chain the remote's is not a fast-forward of as a
//! plain update, and the chain ahead is moved back; in `settle_swept`, take no target move, and
//! the copy the remote no longer holds is kept; in `src/repo.rs` `evidence_refspecs`, ask for
//! this store's own notes too, and they are fetched over.

// The remote that refuses every push is a shell script, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files};

const CONFIG: &str = "main = \"main\"\n\n[[target]]\nname = \"staging\"\n";

/// A repository with two live changes and a `staging` target at the trunk, and a reader clone
/// that has read them all once: the box, the first clone, the remote, the reader and the change
/// ids.
fn store() -> (TestBox, PathBuf, PathBuf, PathBuf, Vec<String>) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = unchecked_files();
    files.push((".bay/config.toml".to_owned(), CONFIG.to_owned()));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let main = test_box.git_bare(&remote, &["rev-parse", "refs/heads/main"]);
    test_box.git_bare(&remote, &["update-ref", "refs/heads/staging", &main]);
    let ids = (0..2).map(|_| test_box.start_and_edit(&repo).0).collect();
    let reader = test_box.clone_of(&remote, "reader");
    test_box.git_ok(
        &reader,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/bay/changes/*:refs/bay/changes/*",
        ],
    );
    let (_, first) = status(&test_box, &reader, "first");
    assert_eq!(first["detail"]["main"], main.as_str(), "{first}");
    assert_eq!(
        test_box.git_ok(&reader, &["rev-parse", "refs/bay/target/staging"]),
        main
    );
    (test_box, repo, remote, reader, ids)
}

/// One git spawn of bay's own process: its subcommand and its arguments.
struct Spawn {
    sub: String,
    args: Vec<String>,
}

impl Spawn {
    fn asks_the_remote(&self) -> bool {
        matches!(self.sub.as_str(), "fetch" | "ls-remote" | "push")
    }
}

/// `bay status` as `reader` in `dir`: the result, and bay's own git spawns.
fn status(test_box: &TestBox, dir: &Path, name: &str) -> (Vec<Spawn>, Value) {
    let trace = test_box.path().join(format!("{name}-trace.jsonl"));
    let (code, result) = result_json(
        test_box
            .bay(dir)
            .arg("status")
            .env("BAY_ACTOR", "reader")
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{result}");
    let text = std::fs::read_to_string(&trace).expect("the trace was written");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {line}")))
        .collect();
    let pid = lines.first().map(|line| line["pid"].clone());
    let spawns = lines
        .iter()
        .filter(|line| Some(&line["pid"]) == pid.as_ref())
        .map(|line| Spawn {
            sub: line["sub"].as_str().unwrap().to_owned(),
            args: line["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|arg| arg.as_str().unwrap().to_owned())
                .collect(),
        })
        .collect();
    (spawns, result)
}

/// The one spawn of `spawns` that asked the remote, which must be a sweep.
fn one_sweep(spawns: &[Spawn]) {
    let asked: Vec<&Vec<String>> = spawns
        .iter()
        .filter(|spawn| spawn.asks_the_remote())
        .map(|spawn| &spawn.args)
        .collect();
    assert_eq!(asked.len(), 1, "the status asked the remote: {asked:#?}");
    assert!(
        asked[0].iter().any(|arg| arg == "--dry-run"),
        "{:?}",
        asked[0]
    );
}

#[test]
fn a_status_asks_the_remote_once_and_takes_each_move_by_its_rule() {
    let (test_box, repo, remote, reader, ids) = store();
    // The remote moves: one change is abandoned, the trunk gains a commit, and the target goes.
    let moved = format!("refs/bay/changes/{}", ids[0]);
    let (code, result) = result_json(test_box.bay(&repo).args(["abandon", &ids[0]]));
    assert_eq!(code, 0, "{result}");
    let main = test_box.git_bare(&remote, &["rev-parse", "refs/heads/main"]);
    let next = test_box.git_bare(
        &remote,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t.invalid",
            "commit-tree",
            &format!("{main}^{{tree}}"),
            "-p",
            &main,
            "-m",
            "the trunk moves",
        ],
    );
    test_box.git_bare(&remote, &["update-ref", "refs/heads/main", &next, &main]);
    test_box.git_bare(&remote, &["update-ref", "-d", "refs/heads/staging"]);

    let (spawns, result) = status(&test_box, &reader, "moved");
    one_sweep(&spawns);
    assert_eq!(result["detail"]["main"], next.as_str(), "{result}");
    assert!(
        !result["freshness"]["fetched_at"]
            .as_str()
            .unwrap_or_default()
            .starts_with("not fetched"),
        "{result}"
    );
    assert_eq!(
        test_box.git_ok(&reader, &["rev-parse", "refs/remotes/origin/main"]),
        next
    );
    assert_eq!(
        test_box.git_ok(&reader, &["rev-parse", &moved]),
        test_box.git_bare(&remote, &["rev-parse", &moved]),
        "the status left the moved chain behind"
    );
    let still = format!("refs/bay/changes/{}", ids[1]);
    assert_eq!(
        test_box.git_ok(&reader, &["rev-parse", &still]),
        test_box.git_bare(&remote, &["rev-parse", &still])
    );
    let left = test_box.git_ok(
        &reader,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/bay/target/",
            "refs/bay/fetch/",
        ],
    );
    assert!(left.is_empty(), "refs left: {left}");
}

#[test]
fn a_status_with_nothing_moved_writes_no_ref() {
    let (test_box, _, _, reader, _) = store();
    let (spawns, _) = status(&test_box, &reader, "quiet");
    one_sweep(&spawns);
    let writes: Vec<&Vec<String>> = spawns
        .iter()
        .filter(|spawn| spawn.sub == "update-ref")
        .map(|spawn| &spawn.args)
        .collect();
    assert!(writes.is_empty(), "the status wrote refs: {writes:#?}");
}

/// A commit of the empty tree with `message` in `dir`, or in the remote when `dir` is `None`.
fn empty_commit(test_box: &TestBox, remote: &Path, dir: Option<&Path>, message: &str) -> String {
    let run = |args: &[&str]| match dir {
        Some(dir) => test_box.git_ok(dir, args),
        None => test_box.git_bare(remote, args),
    };
    let tree = run(&["hash-object", "-w", "-t", "tree", "/dev/null"]);
    run(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t.invalid",
        "commit-tree",
        &tree,
        "-m",
        message,
    ])
}

#[test]
fn work_of_this_store_a_remote_refuses_is_kept() {
    let (test_box, _, remote, reader, _) = store();
    // The reader's own change, merged while the remote was away (G17): its chain here is one
    // request ahead of the remote's.
    let (id, view) = test_box.start_and_edit(&reader);
    let name = format!("refs/bay/changes/{id}");
    let pushed = test_box.git_bare(&remote, &["rev-parse", &name]);
    let away = remote.with_extension("away");
    std::fs::rename(&remote, &away).unwrap();
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    std::fs::rename(&away, &remote).unwrap();
    assert_eq!(code, 0, "{queued}");
    assert_eq!(queued["waiting_for"], "remote", "{queued}");
    let ahead = test_box.git_ok(&reader, &["rev-parse", &name]);
    assert_ne!(ahead, pushed, "the request is not on the reader's chain");
    // Evidence notes of this store's own producer, which the remote holds otherwise.
    let notes = "refs/notes/bay/evidence/local";
    let mine = empty_commit(&test_box, &remote, Some(&reader), "notes of mine");
    test_box.git_ok(&reader, &["update-ref", notes, &mine]);
    let theirs = empty_commit(&test_box, &remote, None, "the remote's notes");
    test_box.git_bare(&remote, &["update-ref", notes, &theirs]);
    // The remote now refuses every push, so the receipts' sync cannot take the state there.
    let refuse = test_box.path().join("refuse-receive-pack");
    std::fs::write(&refuse, "#!/bin/sh\nexit 1\n").unwrap();
    let mut mode = std::fs::metadata(&refuse).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
    std::fs::set_permissions(&refuse, mode).unwrap();
    test_box.git_ok(
        &reader,
        &[
            "config",
            "remote.origin.receivepack",
            refuse.to_str().unwrap(),
        ],
    );

    let _ = status(&test_box, &reader, "ahead");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", &name]), pushed);
    assert_eq!(
        test_box.git_ok(&reader, &["rev-parse", &name]),
        ahead,
        "the status moved the chain ahead of the remote's back"
    );
    assert_eq!(
        test_box.git_ok(&reader, &["rev-parse", notes]),
        mine,
        "the status fetched over this store's own evidence notes"
    );
}
