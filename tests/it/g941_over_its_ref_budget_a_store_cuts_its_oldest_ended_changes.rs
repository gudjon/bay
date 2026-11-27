//! G941 (plan S4: a history of 30,000 landings must leave its remote under 5,000 live refs, and a
//! seven-day window kept every change of the week as refs): over `[retention] ref_budget`, the
//! budget is the window. An idle runner pass cuts the oldest ended changes the remote has no room
//! for, even inside `window_s`, and compaction moves their refs into the shard index; each still
//! answers `status` by id. Over the budget, a finished queue place goes at the next idle pass.
//! A change that has not ended (open, queued, in check) is never cut, nor is a landing still owed
//! a post check. Under the budget, nothing inside the window is cut.
//!
//! Guards to mutate: in `src/compact.rs`, `to_cut` cutting nothing (the ended changes stay);
//! `Over::excess` ignoring the budget (under it, changes are cut); `to_cut` without its
//! `owed` skip (an owed landing is cut); in `src/executor.rs`, `queue_retention` keeping a pair
//! for its hour over the budget; in `src/status.rs`, `held_records` without its fall back to
//! `compact::kept` (a cut change no longer reads by id); in `src/publish.rs`, `retention_due`
//! and `retain` taking an open change as ended (the host refuses the cut, and the pass names it).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{EXECUTOR, TestBox, TestKeys, check_files, passing_result, result_json};

/// A post check `suite` on `src/**`. No runner here runs post work, so a landing on `src/`
/// stays owed it.
const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

/// A host with `config` as its `.bay/config.toml` (and `post` in its policy), an agent's
/// clone of it with enrolled keys, and a runner's clone.
fn host(test_box: &TestBox, config: &str, post: bool) -> (PathBuf, PathBuf, PathBuf, TestKeys) {
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    let mut files = check_files("true", &passing_result(1), 1, 60);
    if post {
        for (path, text) in &mut files {
            if path == ".bay/policy.toml" {
                text.push_str(POST);
            }
        }
    }
    files.push((".bay/config.toml".to_owned(), config.to_owned()));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    let runner = test_box.clone_of(&host, "runner");
    test_box.use_keys(&runner, &keys);
    (host, repo, runner, keys)
}

/// A change started as `name` on `src/`, the names a merge of it leaves on the host, and its
/// view.
fn started(test_box: &TestBox, repo: &Path, name: &str) -> (String, Vec<String>, PathBuf) {
    let started = test_box.bay_ok(repo, &["start", name, "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(format!("src/{id}.rs")), "fn x() {}\n").unwrap();
    let intent = started["detail"]["intent"].as_str().unwrap().to_owned();
    let landed_intent = intent.replace("refs/bay/intent/", "refs/bay/landed-intent/");
    let names = vec![
        format!("refs/bay/changes/{id}"),
        format!("refs/bay/requests/{id}"),
        format!("refs/bay/landed/{id}"),
        intent,
        landed_intent,
    ];
    (id, names, view)
}

/// A change submitted as `name`, its place in the queue joined as an executor joins it, and
/// landed by the runner in `runner`, which releases the place: its id and the names it leaves on
/// the host.
fn merged(
    test_box: &TestBox,
    (host, repo, runner): (&Path, &Path, &Path),
    keys: &TestKeys,
    name: &str,
) -> (String, Vec<String>) {
    let (id, names, view) = started(test_box, repo, name);
    let (code, submitted) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");
    let op_id = submitted["op_id"].as_str().unwrap();
    let signer = bay::signing::Signer {
        key: keys.executor.clone(),
        name: EXECUTOR.to_owned(),
    };
    let opened = bay::repo::Repo::open(repo).unwrap();
    bay::queue::join(&opened, Some(&signer), op_id, &id).unwrap();
    let marker = names[2..3].to_vec();
    for _ in 0..10 {
        if !held(test_box, host, &marker).is_empty() {
            return (id, names);
        }
        pass(test_box, runner);
    }
    panic!("{id} never landed");
}

/// A change started as `name` and abandoned, never submitted: its id and the name that moves
/// once it is cut, its chain (its intent claim stays, G921).
fn abandoned(test_box: &TestBox, repo: &Path, name: &str) -> (String, Vec<String>) {
    let (id, names, view) = started(test_box, repo, name);
    let (code, ended) = result_json(test_box.bay(&view).arg("abandon"));
    assert_eq!(code, 0, "{ended}");
    (id, names.into_iter().take(1).collect())
}

/// The refs `host` holds under `prefix`.
fn listed(test_box: &TestBox, host: &Path, prefix: &str) -> Vec<String> {
    let listed = test_box.git_bare(host, &["for-each-ref", "--format=%(refname)", prefix]);
    listed.lines().map(str::to_owned).collect()
}

/// The queue places `host` holds, live and released.
fn queue_refs(test_box: &TestBox, host: &Path) -> Vec<String> {
    let mut refs = listed(test_box, host, "refs/bay/queue/");
    refs.extend(listed(test_box, host, "refs/bay/queue-done/"));
    refs
}

/// The refs of `names` that `host` holds.
fn held(test_box: &TestBox, host: &Path, names: &[String]) -> Vec<String> {
    let listed = listed(test_box, host, "refs/bay/");
    names
        .iter()
        .filter(|name| listed.contains(name))
        .cloned()
        .collect()
}

/// One runner pass in `runner`, which must name no error.
fn pass(test_box: &TestBox, runner: &Path) -> Value {
    let ran = test_box.bay_ok(runner, &["runner", "run", "--once"]);
    for entry in ran["detail"]["ran"].as_array().into_iter().flatten() {
        assert!(entry.get("error").is_none(), "{ran}");
    }
    ran
}

/// Runner passes in `runner` until `host` holds none of `names`, at most 40.
fn compact_away(test_box: &TestBox, host: &Path, runner: &Path, names: &[String]) {
    for _ in 0..40 {
        if held(test_box, host, names).is_empty() {
            return;
        }
        pass(test_box, runner);
    }
    assert_eq!(held(test_box, host, names), Vec::<String>::new());
}

/// The change `status` reads for `id` in a fresh clone of `host`.
fn status_of(test_box: &TestBox, host: &Path, keys: &TestKeys, id: &str) -> Value {
    let fresh = test_box.path().join(format!("fresh-{id}"));
    if !fresh.exists() {
        let made = test_box.clone_of(host, &format!("fresh-{id}"));
        assert_eq!(made, fresh);
        test_box.use_keys(&fresh, keys);
    }
    let status = test_box.bay_ok(&fresh, &["status", id]);
    let change = status["detail"]["changes"][0].clone();
    assert_eq!(change["change"], id, "{status}");
    change
}

#[test]
fn over_its_budget_a_store_cuts_its_oldest_ended_changes_and_each_still_answers() {
    let test_box = TestBox::new();
    // the default seven-day window, and a budget no store fits under
    let (host, repo, runner, keys) = host(&test_box, "[retention]\nref_budget = 1\n", false);
    let at = (host.as_path(), repo.as_path(), runner.as_path());
    let (one, one_names) = merged(&test_box, at, &keys, "BUDGET-1");
    let (two, two_names) = merged(&test_box, at, &keys, "BUDGET-2");
    let (gone, gone_names) = abandoned(&test_box, &repo, "BUDGET-3");
    let (open, open_names, _view) = started(&test_box, &repo, "BUDGET-4");
    let open_held = held(&test_box, &host, &open_names);
    assert!(!open_held.is_empty(), "the host holds the open change");
    let places = queue_refs(&test_box, &host);
    assert_eq!(
        places.len(),
        4,
        "a place and its release a merge: {places:?}"
    );

    // WHEN runner passes go on until every ended change has left the host
    let ended: Vec<String> = [&one_names, &two_names, &gone_names]
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    compact_away(&test_box, &host, &runner, &ended);

    // THEN each ended change still answers by id, cut, from a store that never held it
    for (id, state) in [(&one, "merged"), (&two, "merged"), (&gone, "abandoned")] {
        let change = status_of(&test_box, &host, &keys, id);
        assert_eq!(change["state"], state, "{change}");
        assert_eq!(change["retention_cut"], true, "{change}");
    }
    // AND the open change was never cut: the host holds all it held
    assert_eq!(held(&test_box, &host, &open_names), open_held);
    let change = status_of(&test_box, &host, &keys, &open);
    assert_ne!(change["retention_cut"], true, "{change}");
    // AND the finished queue pair below the newest went without waiting its hour
    let left = queue_refs(&test_box, &host);
    assert_eq!(left.len(), 2, "{left:?} of {places:?}");
    assert!(left.iter().all(|name| places.contains(name)), "{left:?}");
}

#[test]
fn a_landing_owed_a_post_check_is_never_cut_for_the_budget() {
    let test_box = TestBox::new();
    let (host, repo, runner, keys) = host(&test_box, "[retention]\nref_budget = 1\n", true);
    let at = (host.as_path(), repo.as_path(), runner.as_path());
    // GIVEN a landing on src/ whose post check never answered, and an abandoned change
    let (owed, owed_names) = merged(&test_box, at, &keys, "OWED-1");
    let (gone, gone_names) = abandoned(&test_box, &repo, "OWED-2");
    let owed_held = held(&test_box, &host, &owed_names);
    assert_eq!(owed_held, owed_names);

    // WHEN runner passes go on until the abandoned change has left the host
    compact_away(&test_box, &host, &runner, &gone_names);

    // THEN the abandoned change was cut
    let change = status_of(&test_box, &host, &keys, &gone);
    assert_eq!(change["retention_cut"], true, "{change}");
    // AND the owed landing was not: the host holds all of it, uncut
    assert_eq!(held(&test_box, &host, &owed_names), owed_held);
    let change = status_of(&test_box, &host, &keys, &owed);
    assert_eq!(change["state"], "merged", "{change}");
    assert_ne!(change["retention_cut"], true, "{change}");
}

#[test]
fn under_its_budget_a_store_cuts_nothing_inside_its_window() {
    let test_box = TestBox::new();
    let (host, repo, runner, keys) = host(&test_box, "[retention]\nref_budget = 100000\n", false);
    let at = (host.as_path(), repo.as_path(), runner.as_path());
    let (one, one_names) = merged(&test_box, at, &keys, "ROOM-1");
    let (two, two_names) = merged(&test_box, at, &keys, "ROOM-2");
    let (gone, gone_names) = abandoned(&test_box, &repo, "ROOM-3");
    let names: Vec<String> = [one_names, two_names, gone_names].concat();
    let before = held(&test_box, &host, &names);
    let places = queue_refs(&test_box, &host);
    assert_eq!(
        places.len(),
        4,
        "a place and its release a merge: {places:?}"
    );

    // WHEN runners pass
    for _ in 0..3 {
        pass(&test_box, &runner);
    }

    // THEN no change was cut, and the host holds every ref of each
    for id in [&one, &two, &gone] {
        let change = status_of(&test_box, &host, &keys, id);
        assert_ne!(change["retention_cut"], true, "{change}");
    }
    assert_eq!(held(&test_box, &host, &names), before);
    // AND the finished queue places wait their hour
    assert_eq!(queue_refs(&test_box, &host), places);
}
