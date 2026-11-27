//! G923 (plan S4, CL-2: a shard index held every record at its top level, so each append
//! rewrote one tree of every record its shard held, about 600 at 50,000 changes and growing). A
//! shard index holds each record two levels down, under the second byte of its name's sha256 in
//! hex, written through a temporary index, so an append writes the shard's top tree of at most
//! 256 entries, one small tree for each second byte it touches, and its new records: never a
//! tree of every record.
//!
//! Guard to mutate: in `src/compact.rs`, `path` answering the flat path (the append rewrites a
//! top tree of every record in the shard).

#![cfg(unix)]

use crate::support;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use support::shards::{index, path, shard, write_shards};
use support::{TestBox, check_files, passing_result, result_json};

/// The records each shard already holds before the append.
const HELD: usize = 300;

/// The type of each object `commit` brings that its parent lacks.
fn new_objects(test_box: &TestBox, host: &Path, commit: &str) -> Vec<String> {
    let listed = test_box.git_bare(
        host,
        &[
            "rev-list",
            "--objects",
            commit,
            "--not",
            &format!("{commit}^"),
        ],
    );
    listed
        .lines()
        .map(|line| {
            let oid = line.split(' ').next().unwrap();
            test_box.git_bare(host, &["cat-file", "-t", oid])
        })
        .collect()
}

/// HELD landed markers of old compactions, each holding `blob`, in each of `shards`.
fn old_records(shards: &BTreeSet<u8>, blob: &str) -> BTreeMap<u8, Vec<(String, String)>> {
    let mut held: BTreeMap<u8, Vec<(String, String)>> = BTreeMap::new();
    let alphabet = b"abcdefghijklmnopqrstuvwxyz234567";
    for n in 0_usize.. {
        if shards
            .iter()
            .all(|number| held.get(number).is_some_and(|list| list.len() >= HELD))
        {
            break;
        }
        let old: String = (0..8)
            .map(|place| char::from(alphabet[(n >> (5 * place)) & 31]))
            .collect();
        let name = format!("refs/bay/landed/{old}");
        let number = shard(&name);
        if shards.contains(&number) && held.get(&number).is_none_or(|list| list.len() < HELD) {
            held.entry(number)
                .or_default()
                .push((path(&name), blob.to_owned()));
        }
    }
    held
}

/// Asserts the append `commit` wrote one commit, its shard's top tree of at most 256 entries,
/// one small tree for each second byte it touched and one for each record, and blobs only for
/// what it added.
fn assert_a_small_append(test_box: &TestBox, host: &Path, commit: &str) {
    let added = test_box.git_bare(
        host,
        &[
            "diff-tree",
            "-r",
            "--name-only",
            "--no-commit-id",
            &format!("{commit}^"),
            commit,
        ],
    );
    let added: Vec<&str> = added.lines().collect();
    let bytes: BTreeSet<&str> = added
        .iter()
        .map(|at| at.split('/').next().unwrap())
        .collect();
    let records: BTreeSet<&str> = added
        .iter()
        .filter(|at| at.matches('/').count() == 2)
        .map(|at| at.rsplit_once('/').unwrap().0)
        .collect();
    let objects = new_objects(test_box, host, commit);
    let count = |kind: &str| objects.iter().filter(|object| *object == kind).count();
    assert_eq!(count("commit"), 1, "{commit}: {objects:?}");
    assert_eq!(
        count("tree"),
        1 + bytes.len() + records.len(),
        "{commit}: {added:?}"
    );
    assert!(count("blob") <= added.len(), "{commit}: {objects:?}");
    let top = test_box.git_bare(host, &["ls-tree", commit]);
    assert!(
        top.lines().count() <= 256,
        "{commit} has a top tree of every record"
    );
    for byte in &bytes {
        let tree = test_box.git_bare(host, &["ls-tree", &format!("{commit}:{byte}")]);
        assert!(tree.lines().count() <= 16, "{commit}:{byte} holds {tree}");
    }
}

#[test]
fn an_append_writes_the_top_tree_and_one_small_tree_for_each_second_byte() {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[retention]\nwindow_s = 0\n".to_owned(),
    ));
    let trunk = test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    let started = test_box.bay_ok(&repo, &["start", "LEDGER-3", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let intent = started["detail"]["intent"].as_str().unwrap().to_owned();
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    let landed_intent = intent.replace("refs/bay/intent/", "refs/bay/landed-intent/");
    let names = [
        format!("refs/bay/changes/{id}"),
        format!("refs/bay/requests/{id}"),
        format!("refs/bay/landed/{id}"),
        intent,
        landed_intent,
    ];

    // GIVEN the shard of each of its names holding HELD records of old compactions
    let shards: BTreeSet<u8> = names.iter().map(|name| shard(name)).collect();
    let blob = {
        let file = test_box.path().join("old-marker");
        std::fs::write(&file, format!("{trunk}\n")).unwrap();
        test_box.git_bare(&host, &["hash-object", "-w", file.to_str().unwrap()])
    };
    let held = old_records(&shards, &blob);
    write_shards(&test_box, &host, &held);
    let before: BTreeMap<String, String> = names
        .iter()
        .map(|name| {
            (
                index(name),
                test_box.git_bare(&host, &["rev-parse", &index(name)]),
            )
        })
        .collect();

    // WHEN retention cuts the change and runners move its refs
    let executor = test_box.clone_of(&host, "executor");
    test_box.use_keys(&executor, &keys);
    for _ in 0..12 {
        let left = test_box.git_bare(&host, &["for-each-ref", "--format=%(refname)", "refs/bay/"]);
        if names
            .iter()
            .all(|name| !left.lines().any(|line| line == name.as_str()))
        {
            break;
        }
        test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    }

    // THEN each append wrote one commit, the shard's top tree, one tree for each second byte it
    // touched and one for each record, and blobs only for what it added
    let mut appends = 0;
    for (shard_index, old) in &before {
        let commits = test_box.git_bare(&host, &["rev-list", shard_index, "--not", old]);
        for commit in commits.lines() {
            appends += 1;
            assert_a_small_append(&test_box, &host, commit);
        }
    }
    assert!(appends >= shards.len(), "{appends} appends");
    for name in &names {
        let at = format!("{}:{}", index(name), path(name));
        test_box.git_bare(&host, &["cat-file", "-e", &at]);
    }
}
