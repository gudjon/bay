//! G272. A new generation may follow an abandoned one (`DESIGN` § 4).
//!
//! One intent maps to one change per generation. A change that ends without landing
//! (abandoned, superseded or no effect) closes its generation as a landing does: after `bay
//! land --abandon`, `bay start <intent> --new` claims the next generation with a new change
//! whose `supersedes` names the abandoned one, and that change lands. While the newest
//! generation's change is live, `--new` is refused `CHANGE_SUPERSEDED`, and the refusal says the
//! change is live and how to give it up or adopt it. A closed change behind a newer generation
//! is never re-opened: `bay start --resume` of it is refused naming the newer change, and the
//! host refuses a chain update that turns it live again. Plain `bay start` re-opening an
//! abandoned change says how many trunk commits it is behind, and how to bring them in.
//!
//! Guards to mutate:
//! - start side, `src/start.rs` `open`: the match arm
//!   `(false, true) if ended_unlanded(fetched_record(repo, change)?.state) =>` (drop the arm,
//!   or make the guard `false`): the first case's `--new` is refused.
//! - host side, `src/hook.rs` `Judge::generation_closed`:
//!   `return Ok(ended_unlanded(current.state));` (make it `Ok(false)`): the host refuses the
//!   first case's generation-2 claim.
//! - host side, `src/hook.rs` `Judge::chain`: the G272 check `ended_unlanded(before.state) &&
//!   !ended_unlanded(record.state) && self.claim_exists(..)`: the third case's re-opening push
//!   is taken.
//! - start side, `src/start.rs` `resume`: `if let Some(newer) = newer_generation(repo, &record)?`:
//!   the third case's `--resume` of the abandoned change succeeds.
//! - start side, `src/start.rs` `add_behind`: the fourth case's `behind` is missing.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, TestKeys, check_files, passing_result, result_json};

/// A bay-hosted remote (its hook judges every push) with an enrolled key registry.
fn hosted() -> (TestBox, PathBuf, PathBuf, TestKeys) {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    (test_box, host, repo, keys)
}

fn start(test_box: &TestBox, repo: &Path, args: &[&str]) -> (i32, serde_json::Value) {
    result_json(test_box.bay(repo).arg("start").args(args))
}

fn view_of(started: &serde_json::Value) -> PathBuf {
    PathBuf::from(started["detail"]["view"].as_str().unwrap())
}

/// Starts a change on `intent`, edits `file` in its view and abandons it: its id.
fn start_and_abandon(test_box: &TestBox, repo: &Path, intent: &str, file: &str) -> String {
    let (code, started) = start(test_box, repo, &[intent, "--paths", "src/"]);
    assert_eq!(code, 0, "{started}");
    let view = view_of(&started);
    std::fs::write(view.join(file), "fn half() {}\n").unwrap();
    let (code, abandoned) = result_json(test_box.bay(&view).args(["land", "--abandon"]));
    assert_eq!(code, 0, "{abandoned}");
    assert_eq!(abandoned["detail"]["state"], "abandoned", "{abandoned}");
    started["change"].as_str().unwrap().to_owned()
}

/// The change record at the host's head of change `id`'s chain.
fn host_record(test_box: &TestBox, host: &Path, id: &str) -> serde_json::Value {
    let text = test_box.git_bare(
        host,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    serde_json::from_str(&text).unwrap()
}

/// A push from `repo` to origin: whether the host took it, and its stderr.
fn push(test_box: &TestBox, repo: &Path, args: &[&str]) -> (bool, String) {
    let out = test_box
        .git(repo)
        .args(["push", "origin"])
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A chain that writes state commits signed by the enrolled agent key, as `bay` would.
fn agent_chain<'a>(git: &'a bay::git::Git, keys: &TestKeys) -> bay::change::Chain<'a> {
    let mut chain = bay::change::Chain::new(git);
    chain.sign_with(Some(bay::signing::Signer {
        key: keys.agent.clone(),
        name: support::AGENT.to_owned(),
    }));
    chain
}

#[test]
fn after_an_abandon_new_claims_the_next_generation_and_lands() {
    let (test_box, host, repo, _) = hosted();
    let abandoned = start_and_abandon(&test_box, &repo, "CARD-272", "src/half.rs");

    let (code, next) = start(&test_box, &repo, &["CARD-272", "--new", "--paths", "src/"]);
    assert_eq!(code, 0, "{next}");
    let next_id = next["change"].as_str().unwrap().to_owned();
    assert_ne!(next_id, abandoned, "--new made a new change: {next}");
    assert!(
        next["detail"]["intent"]
            .as_str()
            .is_some_and(|claim| claim.ends_with("/2")),
        "{next}"
    );
    assert_eq!(
        host_record(&test_box, &host, &next_id)["supersedes"],
        abandoned.as_str()
    );

    let view = view_of(&next);
    std::fs::write(view.join("src/again.rs"), "fn again() {}\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(test_box.trunk_change_ids(&host), vec![next_id]);
    let trailer = test_box.git_bare(
        &host,
        &[
            "log",
            "-1",
            "--format=%(trailers:key=Supersedes,valueonly)",
            "main",
        ],
    );
    assert_eq!(trailer.trim(), abandoned);
}

#[test]
fn while_the_generation_is_live_new_is_refused_and_says_why() {
    let (test_box, _host, repo, keys) = hosted();
    let (code, live) = start(&test_box, &repo, &["CARD-273", "--paths", "src/"]);
    assert_eq!(code, 0, "{live}");
    let live_id = live["change"].as_str().unwrap().to_owned();

    let (code, refused) = start(&test_box, &repo, &["CARD-273", "--new", "--paths", "src/"]);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHANGE_SUPERSEDED", "{refused}");
    assert_eq!(refused["change"], live_id.as_str(), "{refused}");
    let why = refused["detail"]["why"].as_str().unwrap_or_default();
    assert!(why.contains("is live"), "{refused}");
    let recovery = refused["detail"]["recovery"].as_str().unwrap_or_default();
    assert!(recovery.contains("bay land --abandon"), "{refused}");
    assert!(recovery.contains("bay start CARD-273"), "{refused}");

    // The host refuses a generation-2 claim while generation 1 is live, even one signed by the
    // change's own agent.
    let git = bay::git::Git::new(&repo);
    let chain = agent_chain(&git, &keys);
    let (_, mut record) = chain.read(&live_id).unwrap();
    let forged = bay::ids::new_change_id().unwrap();
    record.id = forged.clone();
    record.claim.as_mut().unwrap().generation = 2;
    record.supersedes = Some(live_id.clone());
    let draft = chain.drafts(&live_id).unwrap()[0].clone();
    let head = chain.create(&record, &draft, "start CARD-273").unwrap();
    let claim = live["detail"]["intent"].as_str().unwrap();
    let next_claim = format!("{}/2", claim.strip_suffix("/1").unwrap());
    let (pushed, stderr) = push(
        &test_box,
        &repo,
        &[
            "--atomic",
            &format!("refs/bay/changes/{forged}"),
            &format!("{head}:{next_claim}"),
        ],
    );
    assert!(
        !pushed,
        "generation 2 was claimed while generation 1 is live"
    );
    assert!(
        stderr.contains("needs generation 1 landed or given up"),
        "{stderr}"
    );
}

#[test]
fn a_closed_change_behind_a_newer_generation_is_never_re_opened() {
    let (test_box, host, repo, keys) = hosted();
    let abandoned = start_and_abandon(&test_box, &repo, "CARD-274", "src/half.rs");
    let (code, next) = start(&test_box, &repo, &["CARD-274", "--new", "--paths", "src/"]);
    assert_eq!(code, 0, "{next}");
    let next_id = next["change"].as_str().unwrap().to_owned();

    let (code, refused) = start(&test_box, &repo, &["--resume", &abandoned]);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHANGE_SUPERSEDED", "{refused}");
    assert_eq!(refused["change"], abandoned.as_str(), "{refused}");
    assert_eq!(
        refused["detail"]["superseded_by"],
        next_id.as_str(),
        "{refused}"
    );

    // Plain start adopts the newest generation's change, never the abandoned one.
    let (code, adopted) = start(&test_box, &repo, &["CARD-274", "--paths", "src/"]);
    assert_eq!(code, 0, "{adopted}");
    assert_eq!(adopted["change"], next_id.as_str(), "{adopted}");

    // The host refuses a state commit that turns the abandoned change live again.
    test_box.git_ok(
        &repo,
        &[
            "fetch",
            "-q",
            "origin",
            &format!("+refs/bay/changes/{abandoned}:refs/bay/changes/{abandoned}"),
        ],
    );
    let git = bay::git::Git::new(&repo);
    let chain = agent_chain(&git, &keys);
    let (head, mut record) = chain.read(&abandoned).unwrap();
    assert_eq!(record.state, bay::change::ChangeState::Abandoned);
    record.state = bay::change::ChangeState::Open;
    let draft = chain.drafts(&abandoned).unwrap()[0].clone();
    chain.append(&head, &record, &draft, "re-open").unwrap();
    let (pushed, stderr) = push(
        &test_box,
        &repo,
        &[&format!("refs/bay/changes/{abandoned}")],
    );
    assert!(
        !pushed,
        "the abandoned change was re-opened behind generation 2"
    );
    assert!(stderr.contains("re-opens change"), "{stderr}");
    assert_eq!(
        host_record(&test_box, &host, &abandoned)["state"],
        "abandoned"
    );
}

#[test]
fn re_opening_an_abandoned_change_says_how_far_behind_it_is() {
    let (test_box, _host, repo, _) = hosted();
    let abandoned = start_and_abandon(&test_box, &repo, "CARD-275", "src/half.rs");

    // Trunk moves past the abandoned change's base.
    let (_, other) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&other).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");

    let (code, reopened) = start(&test_box, &repo, &["CARD-275", "--paths", "src/"]);
    assert_eq!(code, 0, "{reopened}");
    assert_eq!(reopened["change"], abandoned.as_str(), "{reopened}");
    let behind = reopened["detail"]["behind"].as_u64().unwrap_or_default();
    assert!(behind > 0, "{reopened}");
    let recovery = reopened["detail"]["recovery"].as_str().unwrap_or_default();
    assert!(
        recovery.contains(&format!("bay start --resume {abandoned}")),
        "{reopened}"
    );
}
