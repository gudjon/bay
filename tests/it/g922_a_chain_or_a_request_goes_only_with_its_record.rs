//! G922 (plan S4, CL-1). Once a chain and a request move into the shard index, the record kept
//! there is all that answers for their change, so the host's hook takes the delete of a chain or
//! a request only in a push that appends its shard with its record: the target the delete
//! removes, the record file that target holds and its step log, and nothing else; and only when
//! the change's record says it ended, merged or abandoned, with its chain cut by retention.
//!
//! Guards to mutate, in `src/hook.rs`: `read_appended` taking any record file or log (the forged
//! record and log are taken); `cut_ended` answering `Ok` (the open change's refs are deleted);
//! `compacted_delete` taking a delete its shard records at another target.

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::shards::{index, path};
use support::{TestBox, TestKeys, check_files, passing_result, result_json};

/// The id of the blob holding `content`, written in `repo`.
fn blob(test_box: &TestBox, repo: &Path, content: &str) -> String {
    let file = test_box.path().join("forged-blob");
    std::fs::write(&file, content).unwrap();
    test_box.git_ok(repo, &["hash-object", "-w", file.to_str().unwrap()])
}

/// The step log line of `commit`: its committer time and the first line of its message.
fn step(test_box: &TestBox, repo: &Path, commit: &str) -> String {
    let time = test_box.git_ok(repo, &["log", "-1", "--format=%ct", commit]);
    let message = test_box.git_ok(repo, &["log", "-1", "--format=%B", commit]);
    format!("{time} {}\n", message.lines().next().unwrap_or_default())
}

/// The record of `name` at `target`, as compaction writes it: each path and its blob.
fn record(test_box: &TestBox, repo: &Path, name: &str, target: &str) -> Vec<(String, String)> {
    let file = if name.starts_with("refs/bay/changes/") {
        "change.json"
    } else {
        "request.json"
    };
    let at = path(name);
    vec![
        (
            format!("{at}/target"),
            blob(test_box, repo, &format!("{target}\n")),
        ),
        (
            format!("{at}/{file}"),
            test_box.git_ok(repo, &["rev-parse", &format!("{target}:{file}")]),
        ),
        (
            format!("{at}/log"),
            blob(test_box, repo, &step(test_box, repo, target)),
        ),
    ]
}

/// A runner-signed append of `entries` to the shard index of `name`, on its tip on the host.
fn forge(
    test_box: &TestBox,
    repo: &Path,
    keys: &TestKeys,
    name: &str,
    entries: &[(String, String)],
) -> String {
    let shard = index(name);
    test_box.git_ok(
        repo,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/bay/landed-index/*:refs/bay/landed-index/*",
        ],
    );
    let tip = test_box.git_ok(repo, &["for-each-ref", "--format=%(objectname)", &shard]);
    let index_file = test_box.path().join("forged-index");
    let _ = std::fs::remove_file(&index_file);
    let with_index = |args: &[&str]| {
        let out = test_box
            .git(repo)
            .env("GIT_INDEX_FILE", &index_file)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    if !tip.is_empty() {
        with_index(&["read-tree", &tip]);
    }
    for (at, blob) in entries {
        with_index(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},{at}"),
        ]);
    }
    let tree = with_index(&["write-tree"]);
    let signing_key = format!("user.signingkey={}", keys.executor.display());
    let mut forge = vec![
        "-c",
        "gpg.format=ssh",
        "-c",
        signing_key.as_str(),
        "commit-tree",
        tree.as_str(),
        "-m",
        "forged",
        "-S",
    ];
    if !tip.is_empty() {
        forge.extend(["-p", tip.as_str()]);
    }
    let forged = test_box
        .git(repo)
        .env("GIT_COMMITTER_NAME", support::EXECUTOR)
        .env("GIT_COMMITTER_EMAIL", "exec@bay.invalid")
        .env("GIT_AUTHOR_NAME", support::EXECUTOR)
        .env("GIT_AUTHOR_EMAIL", "exec@bay.invalid")
        .args(&forge)
        .output()
        .unwrap();
    assert!(forged.status.success(), "{forged:?}");
    String::from_utf8(forged.stdout).unwrap().trim().to_owned()
}

/// An atomic push of `refspecs` from `repo` to origin: whether it was taken, and its stderr.
fn push(test_box: &TestBox, repo: &Path, refspecs: &[String]) -> (bool, String) {
    let out = test_box
        .git(repo)
        .args(["push", "--atomic", "origin"])
        .args(refspecs)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A host whose changes retention cuts at once and whose runners never compact, with a merged
/// change `merged` cut by a runner (its chain at `cut`, and `uncut` before the cut), and an
/// open change `open` with a merge request.
struct Store {
    host: PathBuf,
    repo: PathBuf,
    keys: TestKeys,
    merged: String,
    cut: String,
    uncut: String,
    open: String,
}

fn store(test_box: &TestBox) -> Store {
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[retention]\nwindow_s = 0\nref_budget = 100000\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);

    let (merged, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let chain = format!("refs/bay/changes/{merged}");
    let uncut = test_box.git_bare(&host, &["rev-parse", &chain]);
    let executor = test_box.clone_of(&host, "executor");
    test_box.use_keys(&executor, &keys);
    let ran = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    assert!(
        ran["detail"]["ran"][0]["retention_cut"].is_string(),
        "{ran}"
    );
    let cut = test_box.git_bare(&host, &["rev-parse", &chain]);
    assert_ne!(cut, uncut);

    let (open, view) = test_box.start_and_edit(&repo);
    let (code, submitted) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");
    test_box.git_ok(
        &repo,
        &[
            "fetch",
            "-q",
            "origin",
            &chain,
            &format!("refs/bay/requests/{merged}"),
            &format!("refs/bay/changes/{open}"),
            &format!("refs/bay/requests/{open}"),
        ],
    );
    Store {
        host,
        repo,
        keys,
        merged,
        cut,
        uncut,
        open,
    }
}

#[test]
fn a_chain_or_a_request_goes_only_with_its_record_in_its_shard() {
    let test_box = TestBox::new();
    let Store {
        host,
        repo,
        keys,
        merged,
        cut,
        uncut,
        open,
    } = store(&test_box);
    let chain = format!("refs/bay/changes/{merged}");
    let request = format!("refs/bay/requests/{merged}");
    let at = |name: &str| test_box.git_bare(&host, &["rev-parse", name]);
    let refused = |name: &str, entries: &[(String, String)], why: &str| {
        let mut refspecs = Vec::new();
        if !entries.is_empty() {
            let forged = forge(&test_box, &repo, &keys, name, entries);
            refspecs.push(format!("{forged}:{}", index(name)));
        }
        refspecs.push(format!(":{name}"));
        let (pushed, stderr) = push(&test_box, &repo, &refspecs);
        assert!(!pushed, "{name} was deleted, though {why}");
        assert!(stderr.contains(why), "{why}: {stderr}");
    };
    let good = record(&test_box, &repo, &chain, &cut);

    // A delete with no record in the same push.
    refused(
        &chain,
        &[],
        &format!(
            "except by its compaction into {} in the same push",
            index(&chain)
        ),
    );
    // A record of a chain written as a marker's.
    refused(
        &chain,
        &[(path(&chain), good[0].1.clone())],
        "is not a compacted name of shard",
    );
    // A record without its log.
    refused(
        &chain,
        &good[..2],
        "holds its target, its change.json and its log, and only them",
    );
    // A record whose record file is another commit's.
    let mut other_file = good.clone();
    other_file[1].1 = test_box.git_ok(&repo, &["rev-parse", &format!("{uncut}:change.json")]);
    refused(
        &chain,
        &other_file,
        &format!("the record of {chain} is not the one {cut} holds"),
    );
    // A record whose log is not its target's.
    let mut other_log = good.clone();
    other_log[2].1 = blob(&test_box, &repo, "0 forged\n");
    refused(
        &chain,
        &other_log,
        &format!("the record of {chain} is not the one {cut} holds"),
    );
    // A whole record of the chain before its cut, while the delete removes the cut.
    refused(
        &chain,
        &record(&test_box, &repo, &chain, &uncut),
        "except by its compaction",
    );
    // The whole records of an open change's chain and request: it has not ended.
    let open_chain = format!("refs/bay/changes/{open}");
    let open_request = format!("refs/bay/requests/{open}");
    let ended = format!("change {open} has not ended with its chain cut by retention");
    refused(
        &open_chain,
        &record(&test_box, &repo, &open_chain, &at(&open_chain)),
        &ended,
    );
    refused(
        &open_request,
        &record(&test_box, &repo, &open_request, &at(&open_request)),
        &ended,
    );
    assert_eq!(at(&chain), cut);

    // AND the whole records of the cut change's request and chain take their deletes
    for name in [&request, &chain] {
        let forged = forge(
            &test_box,
            &repo,
            &keys,
            name,
            &record(&test_box, &repo, name, &at(name)),
        );
        let (pushed, stderr) = push(
            &test_box,
            &repo,
            &[format!("{forged}:{}", index(name)), format!(":{name}")],
        );
        assert!(pushed, "{name}: {stderr}");
    }
    for name in [&request, &chain] {
        assert_eq!(
            test_box.git_bare(&host, &["for-each-ref", "--format=%(refname)", name]),
            "",
            "{name} is gone"
        );
    }
}
