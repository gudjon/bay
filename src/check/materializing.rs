//! A check sees what its `paths` names of the candidate, within its `materialize_timeout_s`
//! (rulings of 2026-10-19 on materialization at scale; plan wave 2, W4).

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::Duration;

use super::tests::MILLISECOND_BOUNDS;
use super::{Candidate, Hooks, Limits, Sandbox, changed_paths, run_all};
use crate::evidence::{EvidenceRecord, Verdict};
use crate::git::Git;

/// Passes when the candidate's tree (its working directory) holds exactly the files `SEEN`
/// names, space-separated and sorted, and the request's `changed_paths` is `CHANGED`; a pass
/// only when both hold. What it saw goes to stderr.
const ADAPTER: &str = r#"#!/bin/sh
req=$(cat)
commit=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\1/p')
tree=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\2/p')
seen=$(find . -type f | LC_ALL=C sort | tr '\n' ' ')
verdict=product_failure
if [ "$seen" = 'SEEN' ] && printf '%s' "$req" | grep -qF '"changed_paths":CHANGED'; then verdict=pass; fi
printf 'saw: %s\n' "$seen" >&2
printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"selected":1,"completed":1},"checks":[{"id":"t1","verdict":"%s"}]}' "$commit" "$tree" "$verdict"
"#;

/// Counts the candidate's files: a pass when there are exactly `$1`.
const COUNTER: &str = r#"#!/bin/sh
req=$(cat)
commit=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\1/p')
tree=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\2/p')
count=$(find . -type f | wc -l | tr -d ' ')
verdict=product_failure
if [ "$count" = "$1" ]; then verdict=pass; fi
printf 'counted: %s\n' "$count" >&2
printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"selected":1,"completed":1},"checks":[{"id":"t1","verdict":"%s"}]}' "$commit" "$tree" "$verdict"
"#;

fn policy(command: &str, extra: &str) -> String {
    format!(
        "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\n\
         command = \"{command}\"\nadapter = [\"scripts/bay-check\"]\n\
         expected_minimum = 1\ntimeout_s = 60\n{extra}"
    )
}

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn commit(git: &Git, message: &str) -> String {
    git.run(&["add", "-A"]).unwrap();
    git.run(&[
        "-c",
        "user.name=bay",
        "-c",
        "user.email=bay@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-q",
        "-m",
        message,
    ])
    .unwrap();
    git.run(&["rev-parse", "HEAD"]).unwrap()
}

/// A repository whose policy is `policy_text` and whose adapter is `adapter`.
fn repo(policy_text: &str, adapter: &str) -> (tempfile::TempDir, Git) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, ".bay/policy.toml", policy_text);
    write(root, "scripts/bay-check", adapter);
    std::fs::set_permissions(
        root.join("scripts/bay-check"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let git = Git::new(root);
    git.run(&["init", "-q"]).unwrap();
    (dir, git)
}

/// Writes `count` small files in directories of a hundred each.
fn many_files(root: &Path, count: usize) {
    for at in 0..count {
        write(
            root,
            &format!("d{:03}/f{at:05}.txt", at / 100),
            &format!("{at}\n"),
        );
    }
}

/// The one record of running the policy at `base` on `commit`, with the changed paths
/// between them, where the box chose to run checks unconfined.
fn run(git: &Git, scratch: &Path, base: &str, commit: &str) -> EvidenceRecord {
    run_with(git, scratch, base, commit, None, None, &mut || {})
}

/// [`run`] within `budget`, calling `beat` every `heartbeat`.
fn run_with(
    git: &Git,
    scratch: &Path,
    base: &str,
    commit: &str,
    budget: Option<Duration>,
    heartbeat: Option<Duration>,
    beat: &mut dyn FnMut(),
) -> EvidenceRecord {
    let policy = crate::policy::load_at(git, base).unwrap().unwrap();
    let tree = git
        .run(&["rev-parse", &format!("{commit}^{{tree}}")])
        .unwrap();
    let base_tree = git
        .run(&["rev-parse", &format!("{base}^{{tree}}")])
        .unwrap();
    let changed = changed_paths(git, base, commit).unwrap();
    let candidate = Candidate {
        commit,
        tree: &tree,
        base,
        base_tree: &base_tree,
        onto: base,
        change: None,
        changed_paths: &changed,
        id: "op.1",
        policy_provenance: crate::policy::Provenance::Bay,
        inputs: &[],
        turn: None,
        rerun: &[],
        retry: 0,
    };
    let limits = Limits {
        budget,
        finished: Vec::new(),
        heartbeat,
        hidden: Vec::new(),
        sandbox: Sandbox::judge("none", Ok(true)),
        baseline: None,
        parallel: 1,
    };
    let mut keep = |_: &mut EvidenceRecord| Ok(());
    let mut records = run_all(
        git,
        scratch,
        &policy,
        &candidate,
        "local",
        &limits,
        &mut Hooks {
            keep: &mut keep,
            beat,
            seen: None,
        },
    )
    .unwrap()
    .records;
    assert_eq!(records.len(), 1);
    records.remove(0)
}

/// A check with `paths = ["board/", ".bay/"]` sees only those paths of the candidate: a
/// sibling with the same prefix, the adapter's own directory and everything else are absent.
/// Its request still carries every changed path, inside `paths` or not.
#[test]
fn a_check_with_paths_sees_only_those_paths() {
    let adapter = ADAPTER
        .replace(
            "SEEN",
            "./.bay/policy.toml ./board/a.txt ./board/deep/b.txt ",
        )
        .replace("CHANGED", r#"["board/a.txt","src/secret.txt"]"#);
    let (dir, git) = repo(
        &policy("scripts/bay-check", "paths = [\"board/\", \".bay/\"]\n"),
        &adapter,
    );
    let root = dir.path();
    write(root, "board/a.txt", "one\n");
    write(root, "board/deep/b.txt", "deep\n");
    write(root, "boardx/c.txt", "a sibling with the same prefix\n");
    write(root, "README.md", "readme\n");
    let base = commit(&git, "seed");
    write(root, "board/a.txt", "two\n");
    write(root, "src/secret.txt", "outside the check's paths\n");
    let candidate = commit(&git, "candidate");
    let record = run(&git, &root.join("scratch"), &base, &candidate);
    assert_eq!(record.verdict, Verdict::Pass, "{:?}", record.why);
}

/// `materialize_timeout_s` bounds materialization on its own: a tree that takes longer than
/// the bound is `infra_failure` naming the bound, and the check never runs.
#[test]
fn materialization_beyond_its_bound_is_infra_failure() {
    let (dir, git) = repo(
        &policy("scripts/bay-check 2002", "materialize_timeout_s = 1\n"),
        COUNTER,
    );
    many_files(dir.path(), 2000);
    let base = commit(&git, "seed");
    MILLISECOND_BOUNDS.with(|fast| fast.set(true));
    let record = run(&git, &dir.path().join("scratch"), &base, &base);
    MILLISECOND_BOUNDS.with(|fast| fast.set(false));
    assert_eq!(record.verdict, Verdict::InfraFailure, "{:?}", record.why);
    let why = record.why.unwrap_or_default();
    assert!(why.contains("materialize_timeout_s (1s)"), "{why}");
    assert_eq!(record.units.len(), 0, "the check never ran");
}

/// A tree of thousands of files materializes whole within the default bound, every file in
/// place.
#[test]
fn a_large_tree_materializes_within_the_bound() {
    let (dir, git) = repo(&policy("scripts/bay-check 6002", ""), COUNTER);
    many_files(dir.path(), 6000);
    let base = commit(&git, "seed");
    let record = run(&git, &dir.path().join("scratch"), &base, &base);
    assert_eq!(record.verdict, Verdict::Pass, "{:?}", record.why);
}

/// The executor's lease is renewed while a check's candidate is materialized, not only while
/// its command runs: a large tree that takes longer than the lease to write never lets
/// another executor take over a live head.
#[test]
fn materialization_renews_the_lease() {
    let (dir, git) = repo(&policy("scripts/bay-check 2002", ""), COUNTER);
    many_files(dir.path(), 2000);
    let base = commit(&git, "seed");
    let scratch = dir.path().join("scratch");
    let (candidate, adapter) = (
        scratch.join("check-0/candidate"),
        scratch.join("check-0/adapter"),
    );
    let mut while_materializing = 0;
    let mut beat = || {
        if candidate.exists() && !adapter.exists() {
            while_materializing += 1;
        }
    };
    let record = run_with(
        &git,
        &scratch,
        &base,
        &base,
        None,
        Some(Duration::ZERO),
        &mut beat,
    );
    assert_eq!(record.verdict, Verdict::Pass, "{:?}", record.why);
    // The beat runs beside the writer, not once per file (a slow beat never stalls it), so a
    // write this short beats at least once.
    assert!(
        while_materializing >= 1,
        "{while_materializing} beats while the candidate was written"
    );
}

/// A `materialize_timeout_s` too large for the clock is a bound never reached, never a crash
/// of the executor: the check runs and its verdict is on record.
#[test]
fn a_bound_beyond_the_clock_runs_the_check() {
    let (dir, git) = repo(
        &policy(
            "scripts/bay-check 2",
            "materialize_timeout_s = 9223372036854775807\n",
        ),
        COUNTER,
    );
    let base = commit(&git, "seed");
    let record = run(&git, &dir.path().join("scratch"), &base, &base);
    assert_eq!(record.verdict, Verdict::Pass, "{:?}", record.why);
}

/// A candidate whose tree holds a path bay does not write (here a `.git` directory) is on
/// record as `infra_failure` naming the path, and the run goes on: it is decided by the
/// candidate, never a fault that leaves no evidence.
#[test]
fn a_path_bay_does_not_write_is_infra_failure_on_record() {
    let (dir, git) = repo(&policy("scripts/bay-check 2", ""), COUNTER);
    let base = commit(&git, "seed");
    let blob = git
        .run_with(&["hash-object", "-w", "--stdin"], &[], Some(b"[core]\n"))
        .unwrap();
    let raw = |hex: &str| -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
            .collect()
    };
    let mut inner = b"100644 config\0".to_vec();
    inner.extend(raw(&blob));
    let inner = git
        .run_with(
            &["hash-object", "-w", "-t", "tree", "--literally", "--stdin"],
            &[],
            Some(&inner),
        )
        .unwrap();
    let mut top = b"40000 .git\0".to_vec();
    top.extend(raw(&inner));
    let base_tree = git
        .run(&["rev-parse", &format!("{base}^{{tree}}")])
        .unwrap();
    for line in git.run(&["ls-tree", &base_tree]).unwrap().lines() {
        let (head, name) = line.split_once('\t').unwrap();
        let mut words = head.split(' ');
        let (mode, _, oid) = (
            words.next().unwrap(),
            words.next().unwrap(),
            words.next().unwrap(),
        );
        top.extend(format!("{} {name}\0", mode.trim_start_matches('0')).into_bytes());
        top.extend(raw(oid));
    }
    let tree = git
        .run_with(
            &["hash-object", "-w", "-t", "tree", "--literally", "--stdin"],
            &[],
            Some(&top),
        )
        .unwrap();
    let candidate = git
        .run_with(
            &["commit-tree", &tree, "-p", &base, "-m", "candidate"],
            &[
                ("GIT_AUTHOR_NAME", "bay"),
                ("GIT_AUTHOR_EMAIL", "bay@example.invalid"),
                ("GIT_COMMITTER_NAME", "bay"),
                ("GIT_COMMITTER_EMAIL", "bay@example.invalid"),
            ],
            None,
        )
        .unwrap();
    let record = run(&git, &dir.path().join("scratch"), &base, &candidate);
    assert_eq!(record.verdict, Verdict::InfraFailure, "{:?}", record.why);
    let why = record.why.unwrap_or_default();
    assert!(why.contains(".git/config"), "{why}");
}

/// Materialization counts against the execution budget: a budget that runs out while the
/// candidate is written stops the writing there (never only at its `materialize_timeout_s`)
/// and the check is `unknown`.
#[test]
fn materialization_counts_against_the_budget() {
    let (dir, git) = repo(&policy("scripts/bay-check 6002", ""), COUNTER);
    many_files(dir.path(), 6000);
    let base = commit(&git, "seed");
    let scratch = dir.path().join("scratch");
    let (candidate, adapter) = (
        scratch.join("check-0/candidate"),
        scratch.join("check-0/adapter"),
    );
    // At least one beat per file written: how far the writing got.
    let mut written = 0;
    let mut beat = || {
        if candidate.exists() && !adapter.exists() {
            written += 1;
        }
    };
    let record = run_with(
        &git,
        &scratch,
        &base,
        &base,
        Some(Duration::from_millis(20)),
        Some(Duration::ZERO),
        &mut beat,
    );
    assert_eq!(record.verdict, Verdict::Unknown, "{:?}", record.why);
    let why = record.why.unwrap_or_default();
    assert!(
        why.contains("execution budget was exhausted while materializing"),
        "{why}"
    );
    assert!(
        written < 6000,
        "{written} beats: the whole candidate was written"
    );
}
