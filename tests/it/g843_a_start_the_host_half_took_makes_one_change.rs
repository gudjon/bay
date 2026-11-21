//! G843 (a fleet finding, 2026-11-21: one `bay start <intent>` made two changes; the first stayed
//! open and empty on the remote, and `bay status` asked its actor to merge it). The host refused
//! the start's atomic push of its chain and its claim, yet it kept the chain and not the claim.
//! The start then made a new change for the same claim. A start whose refused push left its
//! chain on the remote claims for that chain: the intent gets one change, and that change holds
//! the claim. While another live change holds the intent, the start ends its chain and adopts
//! that change, so no empty change stays open. A refusal with no words of a moved ref is tried
//! again (W10), and the retry finds the kept chain up to date and pushes the claim, so that start
//! too makes one change.
//!
//! Guards to mutate: `src/start.rs` `create` deleting the chain and making a new change after a
//! refused push whose chain the remote holds; `claim_taken_chain` leaving the chain live when
//! another change holds the intent.

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

/// A trunk with one passing check, whose remote, once, keeps the chain of a push that also
/// creates a claim and refuses the push with `words`. With `claim_to`, it also points that
/// claim at the commit `claim_to`, as another start's claim would. The flag file it writes
/// then.
fn half_taking(test_box: &TestBox, words: &str, claim_to: Option<&str>) -> PathBuf {
    let remote = test_box.path().join("repo-remote.git");
    let once = test_box.path().join("half-taken");
    let claim = claim_to.map_or(String::new(), |commit| {
        format!(
            "    refs/bay/intent/*) env -u GIT_QUARANTINE_PATH -u GIT_OBJECT_DIRECTORY \\\n      -u GIT_ALTERNATE_OBJECT_DIRECTORIES git update-ref \"$name\" {commit} || exit 1;;\n"
        )
    });
    let hook = format!(
        "#!/bin/sh\nlines=$(cat)\n[ -e {once} ] && exit 0\ncase \"$lines\" in *refs/bay/intent/*) ;; *) exit 0;; esac\ntouch {once}\ncp -R \"$GIT_QUARANTINE_PATH\"/. \"${{GIT_DIR:-.}}/objects/\"\necho \"$lines\" | while read old new name; do\n  case \"$name\" in\n    refs/bay/changes/*) env -u GIT_QUARANTINE_PATH -u GIT_OBJECT_DIRECTORY \\\n      -u GIT_ALTERNATE_OBJECT_DIRECTORIES git update-ref \"$name\" \"$new\" || exit 1;;\n{claim}  esac\ndone\necho \"{words}\" >&2\nexit 1\n",
        once = once.display()
    );
    let path = remote.join("hooks/pre-receive");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, hook).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    once
}

fn seeded() -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    (test_box, repo, remote)
}

/// The change ids under `refs/bay/changes/` in the store at `dir` (a bare one when `bare`).
fn changes(test_box: &TestBox, dir: &Path, bare: bool) -> Vec<String> {
    let args = ["for-each-ref", "--format=%(refname)", "refs/bay/changes/"];
    let listed = if bare {
        test_box.git_bare(dir, &args)
    } else {
        test_box.git_ok(dir, &args)
    };
    listed
        .lines()
        .filter_map(|name| name.strip_prefix("refs/bay/changes/"))
        .map(str::to_owned)
        .collect()
}

/// The one start, after the remote refused its push with `words` and kept its chain: one change
/// that holds the claim, and it merges.
fn one_change_after(words: &str) {
    let (test_box, repo, remote) = seeded();
    let once = half_taking(&test_box, words, None);

    // WHEN a start's push is refused, and the remote kept its chain without its claim
    let started = test_box.bay_ok(&repo, &["start", "CARD-843", "--paths", "src/"]);
    assert!(once.exists(), "the remote took half a push: {started}");

    // THEN the start answers one change, and it is the only one, here and on the remote
    let id = started["change"].as_str().unwrap().to_owned();
    assert_eq!(
        changes(&test_box, &remote, true),
        vec![id.clone()],
        "{started}"
    );
    assert_eq!(
        changes(&test_box, &repo, false),
        vec![id.clone()],
        "{started}"
    );
    // AND the remote's claim is on that change's chain
    let claims = test_box.git_bare(
        &remote,
        &["for-each-ref", "--format=%(objectname)", "refs/bay/intent/"],
    );
    assert_eq!(claims.lines().count(), 1, "{claims}");
    let chain = test_box.git_bare(&remote, &["rev-list", &format!("refs/bay/changes/{id}")]);
    assert!(
        chain.lines().any(|commit| commit == claims.trim()),
        "{claims}"
    );
    // AND it merges.
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/b.rs"), "// CARD-843\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}

#[test]
fn a_start_whose_push_met_a_held_lock_and_left_its_chain_makes_one_change() {
    one_change_after("error: cannot lock ref: the host's lock is held");
}

#[test]
fn a_start_whose_push_the_host_refused_and_left_its_chain_makes_one_change() {
    one_change_after("the host could not finish a check of its own");
}

#[test]
fn a_start_whose_half_taken_intent_another_change_holds_ends_its_chain_and_adopts_that_one() {
    let (test_box, repo, remote) = seeded();
    // GIVEN a live change, which stands in for another start's change of the intent
    let holder = test_box.bay_ok(&repo, &["start", "CARD-843-HOLDER", "--paths", "src/"]);
    let holder = holder["change"].as_str().unwrap().to_owned();
    let held = test_box.git_bare(
        &remote,
        &["rev-parse", &format!("refs/bay/changes/{holder}")],
    );
    let once = half_taking(
        &test_box,
        "error: cannot lock ref: the host's lock is held",
        Some(held.trim()),
    );

    // WHEN a start's push is refused, and the remote kept its chain and gave its claim to that
    // change
    let started = test_box.bay_ok(&repo, &["start", "CARD-843-WAIT", "--paths", "src/"]);
    assert!(once.exists(), "the remote took half a push: {started}");

    // THEN the start answers the change that holds the claim
    assert_eq!(started["change"], serde_json::json!(holder), "{started}");
    // AND the chain the remote kept has ended: no change but the holder is live
    let kept: Vec<String> = changes(&test_box, &remote, true)
        .into_iter()
        .filter(|id| *id != holder)
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    let record = test_box.git_bare(
        &remote,
        &["show", &format!("refs/bay/changes/{}:change.json", kept[0])],
    );
    let record: serde_json::Value = serde_json::from_str(&record).unwrap();
    assert_eq!(record["state"], "abandoned", "{record}");
}
