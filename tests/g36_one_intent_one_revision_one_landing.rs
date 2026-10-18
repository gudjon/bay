//! G36. One intent, one revision, one landing.
//!
//! A replacement session's `start` on an intent whose change landed is `CHANGE_ALREADY_LANDED`
//! unless `--new`; generation n+1 is refused before generation n landed; after a landing
//! `start --new` claims generation 2 and lands once (with `Supersedes:`), and two concurrent
//! `--new` calls make one generation-2 change; two concurrent `start` calls of one actor yield
//! one change; another actor is refused while the holder works, and adopts an idle change
//! after the takeover window and lands it once; the actor it was adopted from is told so; two
//! changes holding one generation still land once (the landed-intent marker). A resubmission of
//! the same revision attaches to its operation (G10, G13). An observer that reboots waits a full
//! takeover window again; adopting a chain retention already cut re-roots it and lands once.

mod support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

fn setup() -> (TestBox, PathBuf, PathBuf) {
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

fn start_as(
    test_box: &TestBox,
    repo: &Path,
    actor: &str,
    args: &[&str],
) -> (i32, serde_json::Value) {
    result_json(
        test_box
            .bay(repo)
            .env("BAY_ACTOR", actor)
            .arg("start")
            .args(args),
    )
}

fn edit_and_land(
    test_box: &TestBox,
    actor: &str,
    started: &serde_json::Value,
    file: &str,
) -> (i32, serde_json::Value) {
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(format!("src/{file}.rs")), "fn x() {}\n").unwrap();
    result_json(
        test_box
            .bay(&view)
            .env("BAY_ACTOR", actor)
            .args(["land", "--wait"]),
    )
}

#[test]
fn a_landed_intent_needs_new_and_generation_two_lands_once() {
    let (test_box, repo, remote) = setup();
    let (code, first) = start_as(&test_box, &repo, "agent-a", &["CARD-9", "--paths", "src/"]);
    assert_eq!(code, 0, "{first}");
    // Generation 2 before generation 1 landed is refused.
    let (code, early) = start_as(
        &test_box,
        &repo,
        "agent-a",
        &["CARD-9", "--new", "--paths", "src/"],
    );
    assert_eq!(code, 10, "{early}");
    assert_eq!(early["reason"], "CHANGE_SUPERSEDED");
    let (code, _) = edit_and_land(&test_box, "agent-a", &first, "one");
    assert_eq!(code, 0);

    // A replacement session on the landed intent.
    let (code, again) = start_as(&test_box, &repo, "agent-b", &["CARD-9", "--paths", "src/"]);
    assert_eq!(code, 10, "{again}");
    assert_eq!(again["reason"], "CHANGE_ALREADY_LANDED");

    // Two concurrent --new calls make one generation-2 change.
    let spawn = || {
        test_box
            .bay_process(&repo)
            .env("BAY_ACTOR", "agent-b")
            .args(["--json", "start", "CARD-9", "--new", "--paths", "src/"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let (one, two) = (spawn(), spawn());
    let results: Vec<serde_json::Value> = [one, two]
        .into_iter()
        .map(|child| serde_json::from_slice(&child.wait_with_output().unwrap().stdout).unwrap())
        .collect();
    assert_eq!(results[0]["change"], results[1]["change"], "{results:?}");
    let second = results
        .iter()
        .find(|result| result["decision"] == "ok")
        .unwrap()
        .clone();
    let (code, landed) = edit_and_land(&test_box, "agent-b", &second, "two");
    assert_eq!(code, 0, "{landed}");
    let first_id = first["change"].as_str().unwrap();
    let trailer = test_box.git_bare(
        &remote,
        &[
            "log",
            "-1",
            "--format=%(trailers:key=Supersedes,valueonly)",
            "main",
        ],
    );
    assert_eq!(trailer.trim(), first_id);
    assert_eq!(test_box.trunk_change_ids(&remote).len(), 2);
    assert!(
        test_box
            .git_bare(
                &remote,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    "refs/bay/landed-intent/"
                ]
            )
            .ends_with("/2")
    );
}

#[test]
fn concurrent_starts_of_one_actor_yield_one_change() {
    let (test_box, repo, _) = setup();
    let spawn = || {
        test_box
            .bay_process(&repo)
            .env("BAY_ACTOR", "agent-a")
            .args(["--json", "start", "CARD-1", "--paths", "src/"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let (one, two) = (spawn(), spawn());
    let ids: Vec<String> = [one, two]
        .into_iter()
        .map(|child| {
            let out: serde_json::Value =
                serde_json::from_slice(&child.wait_with_output().unwrap().stdout).unwrap();
            out["change"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(ids[0], ids[1]);
}

#[test]
fn another_actor_is_refused_while_the_holder_works_and_adopts_an_idle_change() {
    let (test_box, repo, remote) = setup();
    let (_, held) = start_as(&test_box, &repo, "agent-a", &["CARD-2", "--paths", "src/"]);
    let id = held["change"].as_str().unwrap().to_owned();

    // The holder checkpoints during the takeover window: agent-b is refused.
    let late = test_box
        .bay_process(&repo)
        .env("BAY_ACTOR", "agent-b")
        .args(["--json", "start", "CARD-2", "--paths", "src/"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // The holder keeps checkpointing until the other start returns.
    let view = PathBuf::from(held["detail"]["view"].as_str().unwrap());
    let mut late = late;
    let mut round = 0;
    let status = loop {
        if let Some(status) = late.try_wait().unwrap() {
            break status;
        }
        round += 1;
        std::fs::write(
            view.join("src/working.rs"),
            format!("fn w() {{ {round} }}\n"),
        )
        .unwrap();
        test_box
            .bay(&repo)
            .env("BAY_ACTOR", "agent-a")
            .args(["start", "--resume", &id])
            .output()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
    };
    assert!(status.code().is_some());
    let mut stdout = String::new();
    std::io::Read::read_to_string(&mut late.stdout.take().unwrap(), &mut stdout).unwrap();
    let out: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(out["reason"], "CHANGE_SUPERSEDED", "{out}");

    // The holder goes idle: after the takeover window agent-b adopts it and lands it once.
    let (code, adopted) = start_as(&test_box, &repo, "agent-b", &["CARD-2", "--paths", "src/"]);
    assert_eq!(code, 0, "{adopted}");
    assert_eq!(adopted["change"], id.as_str());
    assert_eq!(adopted["detail"]["adopted"], true);
    let (code, _) = edit_and_land(&test_box, "agent-b", &adopted, "adopted");
    assert_eq!(code, 0);
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id.clone()]);

    // agent-a, back, is told its change was adopted.
    let (code, told) = result_json(test_box.bay(&view).env("BAY_ACTOR", "agent-a").arg("land"));
    assert_ne!(code, 0, "{told}");
    assert!(
        told.to_string().contains("adopted") || told["reason"] == "CHANGE_ALREADY_LANDED",
        "{told}"
    );
}

#[test]
fn two_changes_holding_one_generation_land_once() {
    let (test_box, repo, remote) = setup();
    let (_, first) = start_as(&test_box, &repo, "agent-a", &["CARD-3", "--paths", "src/"]);
    let first_id = first["change"].as_str().unwrap().to_owned();
    // A second change that holds the same claim (as one opened before claims existed would).
    let git = bay::git::Git::new(&repo);
    let chain = bay::change::Chain::new(&git);
    let (_, mut record) = chain.read(&first_id).unwrap();
    let twin = bay::ids::new_change_id().unwrap();
    record.id = twin.clone();
    let draft = chain.drafts(&first_id).unwrap()[0].clone();
    chain.create(&record, &draft, "a twin claim").unwrap();
    test_box.git_ok(
        &repo,
        &["push", "-q", "origin", &format!("refs/bay/changes/{twin}")],
    );
    let twin_view = test_box.bay_ok(&repo, &["start", "--resume", &twin]);

    let (code, _) = edit_and_land(&test_box, "agent-a", &first, "first");
    assert_eq!(code, 0);
    let (code, refused) = edit_and_land(&test_box, "agent-a", &twin_view, "twin");
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHANGE_ALREADY_LANDED");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![first_id]);
}

const TAKEOVER_S: u64 = 6;

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// An observer that reboots during the takeover window starts the window again: the takeover
/// is delayed by up to one window, never advanced (DESIGN § 4).
#[test]
fn an_observer_reboot_delays_a_takeover_and_never_advances_it() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        format!("[executor]\ntakeover_s = {TAKEOVER_S}\n"),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let started = test_box.bay_ok(&repo, &["start", "CARD-REBOOT", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();

    // Another actor starts watching the idle change, then its machine reboots mid-window.
    let other = test_box.clone_of(&remote, "other");
    let mut first = test_box
        .bay_process(&other)
        .env("BAY_ACTOR", "agent-b")
        .args(["start", "CARD-REBOOT", "--paths", "src/"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(TAKEOVER_S / 2));
    first.kill().unwrap();
    let _ = first.wait();

    // After the reboot it waits a full window from its own new sighting.
    let relaunched = unix_now();
    let (code, adopted) = result_json(test_box.bay(&other).env("BAY_ACTOR", "agent-b").args([
        "start",
        "CARD-REBOOT",
        "--paths",
        "src/",
    ]));
    assert_eq!(code, 0, "{adopted}");
    assert_eq!(adopted["detail"]["adopted"], true, "{adopted}");
    let log = test_box.git_bare(
        &remote,
        &["log", "--format=%ct %s", &format!("refs/bay/changes/{id}")],
    );
    let claimed: u64 = log
        .lines()
        .find(|line| line.contains("adopt"))
        .and_then(|line| line.split(' ').next())
        .and_then(|time| time.parse().ok())
        .unwrap_or_else(|| panic!("no adoption in:\n{log}"));
    // Whole seconds: a wait of d >= TAKEOVER_S moves the whole-second clock by at least that.
    assert!(
        claimed >= relaunched + TAKEOVER_S,
        "the adoption came {} s after the reboot, under one window",
        claimed.saturating_sub(relaunched)
    );
}

/// Adopting a chain that retention already cut re-roots it from the kept head (DESIGN § 4): an
/// abandoned change is cut, another actor adopts its intent, gets the kept draft in a new view,
/// and lands the change once. The re-rooted chain is live again, so its landing is cut in turn.
#[test]
fn adopting_a_cut_chain_re_roots_it_and_lands_once() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[retention]\nwindow_s = 0\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let started = test_box.bay_ok(&repo, &["start", "CARD-36", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/half.rs"), "fn half() {}\n").unwrap();
    // A checkpoint records the half-done work in a draft; abandoning keeps that draft.
    test_box.bay_ok(&repo, &["start", "--resume", &id]);
    let (code, abandoned) = result_json(test_box.bay(&view).args(["land", "--abandon"]));
    assert_eq!(code, 0, "{abandoned}");
    let executor = test_box.clone_of(&remote, "executor");
    let ran = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert!(
        ran["detail"]["ran"][0]["retention_cut"].is_string(),
        "{ran}"
    );

    // Another actor, on another clone, adopts the intent of the cut chain.
    let other = test_box.clone_of(&remote, "other");
    let (code, adopted) = result_json(
        test_box
            .bay(&other)
            .env("BAY_ACTOR", "agent-b")
            .args(["start", "CARD-36", "--paths", "src/"]),
    );
    assert_eq!(code, 0, "{adopted}");
    assert_eq!(adopted["change"], id.as_str());
    assert_eq!(adopted["detail"]["adopted"], true);
    let adopted_view = PathBuf::from(adopted["detail"]["view"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(adopted_view.join("src/half.rs")).unwrap(),
        "fn half() {}\n",
        "the adopter's view holds the kept draft"
    );
    let record = test_box.git_bare(
        &remote,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    assert!(
        !record.contains("\"retention_cut\": true"),
        "re-rooted: {record}"
    );

    let (code, landed) = result_json(
        test_box
            .bay(&adopted_view)
            .env("BAY_ACTOR", "agent-b")
            .args(["land", "--wait"]),
    );
    assert_eq!(code, 0, "{landed}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id.clone()]);
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:src/half.rs"]),
        "fn half() {}"
    );
    // Live again, the chain is cut once more after its landing.
    let ran = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert!(
        ran["detail"]["ran"][0]["retention_cut"].is_string(),
        "{ran}"
    );
}
