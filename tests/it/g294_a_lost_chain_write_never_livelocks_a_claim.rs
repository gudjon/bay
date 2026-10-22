//! G294. A lost chain write never livelocks a claim.
//!
//! Every reader of a remote chain moves this store's chain ref only forward (G289). So a write
//! an executor made on its chain ref, which the remote then refused because another executor
//! moved that chain first, must never stay on the ref: it is apart from the remote's chain,
//! no push can ever take it, and every later compare-and-swap on the ref would lose to it.
//! That was G35's livelock, `the change chain kept moving under the claim of <op>`: with three
//! executors, one wrote `queued <op> at <seq>` on a chain another had just moved, its push
//! was refused, and once that operation became the queue's head no pass of that executor
//! could claim it.
//!
//! - The process that wrote it takes it back, whichever path wrote it: a write its context
//!   never pushed leaves the chain ref when the context ends, or when it reads the remote's
//!   chain again. Here an executor's `queued` write loses to another executor's write; after
//!   its pass its chain ref holds nothing the remote lacks, and its next pass claims the
//!   operation and lands it.
//! - A head apart from the remote's that no live process will take back (its writer died
//!   between its write and its push) is set aside under `refs/bay/kept/<id>/<sha>`, and the
//!   chain ref goes on from the remote's head: nothing is lost, and the pass lands the change.
//!
//! Guards to mutate, each of which fails a case here:
//! - `impl Drop for Ctx` (`src/publish.rs`): make `drop` do nothing. The first case fails: the
//!   lost `queued` write stays on the executor's chain ref.
//! - `Repo::remote_chain` (`src/repo.rs`): make its last arm (`Some(local) =>`, the apart one)
//!   keep the local head where it is. The second case fails: its pass ends `failed`, the
//!   change chain kept moving under the claim. With both mutated, the first case's second
//!   pass fails the same way, as G35 did.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;

use support::queued::ran_for;
use support::{TestBox, check_files, passing_result, result_json};

/// A commit on the chain state `head` that records what `head` records, made in the git
/// directory `store` (a clone's `.git`, or the remote): another writer's state, as the chain
/// links it, on `head` and its draft.
fn another_write(test_box: &TestBox, store: &Path, head: &str, message: &str) -> String {
    let git_dir = format!("--git-dir={}", store.display());
    let git = |args: &[&str]| {
        let mut all = vec![git_dir.as_str()];
        all.extend_from_slice(args);
        test_box.git_ok(test_box.path(), &all)
    };
    let draft = git(&["rev-parse", &format!("{head}^2")]);
    git(&[
        "commit-tree",
        &format!("{head}^{{tree}}"),
        "-p",
        head,
        "-p",
        &draft,
        "-m",
        message,
    ])
}

/// Whether the remote holds `commit` on its ref `name`.
fn remote_reaches(test_box: &TestBox, remote: &Path, commit: &str, name: &str) -> bool {
    let git_dir = format!("--git-dir={}", remote.display());
    test_box
        .git(test_box.path())
        .args([
            git_dir.as_str(),
            "merge-base",
            "--is-ancestor",
            commit,
            name,
        ])
        .output()
        .unwrap()
        .status
        .success()
}

/// A trunk whose one check passes, and a change submitted on it. Returns the box, the work
/// repository, the remote and the change id.
fn submitted() -> (TestBox, std::path::PathBuf, std::path::PathBuf, String) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    (test_box, repo, remote, id)
}

#[test]
fn a_write_whose_push_lost_leaves_the_chain_ref_and_the_next_pass_claims() {
    let (test_box, repo, remote, id) = submitted();
    // A live queue whose head no pass here runs: a change with no operation holds place 1, so
    // the submitted change waits behind it, and the executor writes it `queued`.
    let started = test_box.bay_ok(&repo, &["start", "CARD-2941", "--paths", "src/"]);
    let holder = started["change"].as_str().unwrap().to_owned();
    let opened = bay::repo::Repo::open(&repo).unwrap();
    let holder_op = format!("{holder}.1");
    let place = bay::queue::join(&opened, None, &holder_op, &holder).unwrap();
    let executor = test_box.clone_of(&remote, "executor");

    // Another executor writes the chain just before this one's push of its `queued` state:
    // the remote's compare-and-swap refuses that push.
    let chain = format!("refs/bay/changes/{id}");
    let read = test_box.git_bare(&remote, &["rev-parse", &chain]);
    let moved = another_write(&test_box, &remote, &read, "another executor's write");
    let armed = test_box.path().join("armed");
    test_box.wrap_git(&format!(
        "case \" $* \" in\n  *\" push \"*'{chain}'*)\n    if mv '{armed}' '{armed}.fired' 2>/dev/null; then\n      \"$real\" --git-dir='{remote}' update-ref '{chain}' '{moved}' '{read}'\n    fi\n    ;;\nesac",
        armed = armed.display(),
        remote = remote.display(),
    ));
    std::fs::write(&armed, "").unwrap();

    let first = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    assert!(
        test_box.path().join("armed.fired").exists(),
        "the executor never pushed the chain: {first}"
    );
    assert_eq!(ran_for(&first, &id)["decision"], "queued", "{first}");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", &chain]), moved);
    let local = test_box.git_ok(&executor, &["rev-parse", &chain]);
    assert!(
        remote_reaches(&test_box, &remote, &local, &chain),
        "the executor's chain ref kept its write the remote refused: {local}"
    );

    // The head's place goes; the waiting change is head, and the next pass claims it.
    bay::queue::release(&opened, None, place, &holder_op, "superseded").unwrap();
    let second = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    let ran = ran_for(&second, &id);
    assert_eq!(ran["decision"], "merged", "{second}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}

#[test]
fn a_dead_writers_head_apart_from_the_remote_is_set_aside_and_the_pass_lands() {
    let (test_box, _repo, remote, id) = submitted();
    let executor = test_box.clone_of(&remote, "executor");
    let chain = format!("refs/bay/changes/{id}");
    test_box.git_ok(
        &executor,
        &[
            "fetch",
            "-q",
            "--no-tags",
            "origin",
            &format!("+{chain}:{chain}"),
        ],
    );
    let read = test_box.git_ok(&executor, &["rev-parse", &chain]);
    // A process here wrote on the chain and died before its push; another executor moved the
    // remote's chain meanwhile. Nothing will ever push the dead write.
    let dead = another_write(
        &test_box,
        &executor.join(".git"),
        &read,
        "a write whose process died before its push",
    );
    test_box.git_ok(&executor, &["update-ref", &chain, &dead, &read]);
    let moved = another_write(&test_box, &remote, &read, "another executor's write");
    test_box.git_bare(&remote, &["update-ref", &chain, &moved, &read]);

    let pass = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    let ran = ran_for(&pass, &id);
    assert_eq!(ran["decision"], "merged", "{pass}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id.clone()]);
    let kept = format!("refs/bay/kept/{id}/{dead}");
    assert_eq!(
        test_box.git_ok(&executor, &["rev-parse", &kept]),
        dead,
        "the dead write was not set aside"
    );
    let local = test_box.git_ok(&executor, &["rev-parse", &chain]);
    assert!(
        remote_reaches(&test_box, &remote, &moved, &chain)
            && remote_reaches(&test_box, &remote, &local, &chain),
        "the chain ref did not go on from the remote's head: {local}"
    );
}
