//! G289. A local chain record is never moved back.
//!
//! A change's chain ref in this store can hold a state commit the remote lacks: work not yet
//! pushed, here the request a `bay land` committed while the remote could not be reached
//! (G17). Every reader of the remote's chain reads it by one rule (`Repo::remote_chain`): it
//! moves this store's chain ref only by fast-forward, and keeps a local head the remote lacks.
//! So `bay start --new` on the change's intent, a plain `bay start` that adopts it, and an
//! executor pass each leave that commit reachable from the local chain ref, or push it to the
//! remote first. None rewinds the ref to the remote's head, as a forced fetch would.
//!
//! Guards to mutate, each of which fails a case here:
//! - `Repo::remote_chain` (`src/repo.rs`): force its first fetch (`+{name}:{name}`), or make its
//!   arm for a local head ahead of the remote's (`is_ancestor(&remote, &local)`) move the local
//!   ref to the remote's head (`update-ref <name> <remote>`). Every case fails.
//! - `start::fetched_record` (`src/start.rs`): fetch `+{name}:{name}` before it reads the chain
//!   (`start --new` fails).
//! - `start::adopt_or_refuse` (`src/start.rs`): fetch `+{name}:{name}` in place of its
//!   `repo.remote_chain(change)` (the adoption fails).
//! - `Repo::fetch_chains` (`src/repo.rs`): make the executor pass's batch refspec forced
//!   (`+{prefix}*:{prefix}*`) (the pass whose sync is refused fails).

// Each case runs shell scripts (adapters, a hook, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

const INTENT: &str = "CARD-289";

/// A change opened on [`INTENT`] and pushed, then landed while the remote was away: its local
/// chain is one request ahead of the remote's. Returns the box, the work repository, the
/// remote, the change id and the local head the remote lacks.
fn unpushed() -> (TestBox, PathBuf, PathBuf, String, String) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let started = test_box.bay_ok(&repo, &["start", INTENT, "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    let name = format!("refs/bay/changes/{id}");
    let pushed = test_box.git_bare(&remote, &["rev-parse", &name]);

    // The remote is away while the request is committed, as in G17.
    let away = remote.with_extension("away");
    std::fs::rename(&remote, &away).unwrap();
    let (code, queued) = result_json(test_box.bay(&view).arg("land"));
    std::fs::rename(&away, &remote).unwrap();
    assert_eq!(code, 0, "{queued}");
    assert_eq!(queued["waiting_for"], "remote", "{queued}");

    let ahead = test_box.git_ok(&repo, &["rev-parse", &name]);
    assert_ne!(
        ahead, pushed,
        "the offline request is not on the local chain"
    );
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", &name]),
        pushed,
        "the offline request reached the remote"
    );
    assert!(reaches(&test_box, &repo, &pushed, &name));
    (test_box, repo, remote, id, ahead)
}

/// Whether the ref `name` of the repository `dir` reaches the commit `commit`.
fn reaches(test_box: &TestBox, dir: &Path, commit: &str, name: &str) -> bool {
    test_box
        .git(dir)
        .args(["merge-base", "--is-ancestor", commit, name])
        .output()
        .unwrap()
        .status
        .success()
}

/// Whether the remote's ref `name` reaches the commit `commit`.
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

/// The unpushed commit survives: the local chain ref still reaches it, or the remote's does.
fn survives(test_box: &TestBox, repo: &Path, remote: &Path, id: &str, ahead: &str, what: &str) {
    let name = format!("refs/bay/changes/{id}");
    let local = test_box.git_ok(repo, &["rev-parse", &name]);
    assert!(
        reaches(test_box, repo, ahead, &name) || remote_reaches(test_box, remote, ahead, &name),
        "{what} moved the local chain of {id} back from {ahead} to {local}, and the remote \
         never took {ahead}"
    );
}

#[test]
fn start_new_on_its_intent_keeps_the_unpushed_commit() {
    let (test_box, repo, remote, id, ahead) = unpushed();
    let (code, refused) = result_json(
        test_box
            .bay(&repo)
            .args(["start", INTENT, "--new", "--paths", "src/"]),
    );
    // The change is live, so the next generation is refused; the read must not rewind it.
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHANGE_SUPERSEDED", "{refused}");
    assert_eq!(refused["change"], id.as_str(), "{refused}");
    survives(&test_box, &repo, &remote, &id, &ahead, "start --new");
}

#[test]
fn an_adoption_keeps_the_unpushed_commit() {
    let (test_box, repo, remote, id, ahead) = unpushed();
    let adopted = test_box.bay_ok(&repo, &["start", INTENT, "--paths", "src/"]);
    assert_eq!(adopted["change"], id.as_str(), "{adopted}");
    survives(&test_box, &repo, &remote, &id, &ahead, "an adoption");
}

#[test]
fn an_executor_pass_pushes_the_unpushed_commit_first() {
    let (test_box, repo, remote, id, ahead) = unpushed();
    let ran = test_box.bay_ok(&repo, &["executor", "run", "--once"]);
    survives(&test_box, &repo, &remote, &id, &ahead, "an executor pass");
    let name = format!("refs/bay/changes/{id}");
    assert!(
        remote_reaches(&test_box, &remote, &ahead, &name),
        "the pass never pushed the offline request: {ran}"
    );
}

#[test]
fn an_executor_pass_whose_push_is_refused_keeps_the_unpushed_commit() {
    let (test_box, repo, remote, id, ahead) = unpushed();
    // The remote refuses every update of this change's chain, so the pass's sync cannot push
    // the offline request; the pass's own read of the chains must keep it.
    let hook = remote.join("hooks/pre-receive");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\nwhile read old new name; do\n  if [ \"$name\" = \"refs/bay/changes/{id}\" ]; then\n    echo \"this chain takes no update\" >&2\n    exit 1\n  fi\ndone\nexit 0\n"
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // The pass's own outcome is not this card's: only what it did to the chain ref is.
    let _ = test_box
        .bay(&repo)
        .args(["executor", "run", "--once", "--json"])
        .output()
        .unwrap();
    let name = format!("refs/bay/changes/{id}");
    assert!(
        !remote_reaches(&test_box, &remote, &ahead, &name),
        "the hook let the offline request through"
    );
    survives(&test_box, &repo, &remote, &id, &ahead, "an executor pass");
}
