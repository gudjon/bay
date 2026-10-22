//! G36. One intent, one revision, one landing.
//!
//! Generation n+1 is refused before generation n landed; after a landing a replacement
//! session's plain `start` claims generation 2 (G293: `--new` is no longer needed there), and
//! two concurrent such starts make one generation-2 change, which lands once (with
//! `Supersedes:`); each generation lands once; two concurrent `start` calls of one actor yield
//! one change; another actor is refused while the holder works, and adopts an idle change
//! after the takeover window and lands it once; the actor it was adopted from is told so; two
//! changes holding one generation still land once (the landed-intent marker). A resubmission of
//! the same revision attaches to its operation (G10, G13). An observer that reboots waits a full
//! takeover window again; adopting a chain retention already cut re-roots it and lands once. A
//! same-name `start` while another session of that name has a fresh heartbeat naming the change
//! is refused `CHANGE_HELD`, and adopts once that session ends or its heartbeat ages past the
//! takeover window (rulings-open-questions.md Q8).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

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
            .args(["merge", "--wait"]),
    )
}

#[test]
fn after_a_landing_start_claims_generation_two_and_it_lands_once() {
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

    // A replacement session on the landed intent: two concurrent plain starts make one
    // generation-2 change (G293).
    let spawn = || {
        test_box
            .bay_process(&repo)
            .env("BAY_ACTOR", "agent-b")
            .args(["--json", "start", "CARD-9", "--paths", "src/"])
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
    // The start that created it names its claim; the other adopted it.
    let second = results
        .iter()
        .find(|result| result["decision"] == "ok" && result["detail"]["intent"].is_string())
        .unwrap()
        .clone();
    let first_id = first["change"].as_str().unwrap();
    assert_ne!(second["change"], first_id, "{second}");
    assert!(
        second["detail"]["intent"]
            .as_str()
            .is_some_and(|claim| claim.ends_with("/2")),
        "{second}"
    );
    let (code, landed) = edit_and_land(&test_box, "agent-b", &second, "two");
    assert_eq!(code, 0, "{landed}");
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
            .args(["start", &id])
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
    let (code, told) = result_json(test_box.bay(&view).env("BAY_ACTOR", "agent-a").arg("merge"));
    assert_ne!(code, 0, "{told}");
    assert!(
        told.to_string().contains("adopted") || told["reason"] == "CHANGE_ALREADY_MERGED",
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
    let twin_view = test_box.bay_ok(&repo, &["start", &twin]);

    let (code, _) = edit_and_land(&test_box, "agent-a", &first, "first");
    assert_eq!(code, 0);
    let (code, refused) = edit_and_land(&test_box, "agent-a", &twin_view, "twin");
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHANGE_ALREADY_MERGED");
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
        format!("[runner]\ntakeover_s = {TAKEOVER_S}\n"),
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
    test_box.bay_ok(&repo, &["start", &id]);
    let (code, abandoned) = result_json(test_box.bay(&view).args(["abandon"]));
    assert_eq!(code, 0, "{abandoned}");
    let executor = test_box.clone_of(&remote, "executor");
    let ran = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
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
            .args(["merge", "--wait"]),
    );
    assert_eq!(code, 0, "{landed}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id.clone()]);
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:src/half.rs"]),
        "fn half() {}"
    );
    // Live again, the chain is cut once more after its landing.
    let ran = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    assert!(
        ran["detail"]["ran"][0]["retention_cut"].is_string(),
        "{ran}"
    );
}

/// `git <args>` in `dir` with `input` on stdin, committed as `committer`.
fn git_with_input(
    test_box: &TestBox,
    dir: &Path,
    committer: &str,
    args: &[&str],
    input: &[u8],
) -> String {
    use std::io::Write as _;
    let email = format!("{committer}@bay.invalid");
    let mut child = test_box
        .git(dir)
        .env("GIT_AUTHOR_NAME", committer)
        .env("GIT_AUTHOR_EMAIL", &email)
        .env("GIT_COMMITTER_NAME", committer)
        .env("GIT_COMMITTER_EMAIL", &email)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// One session row of a live summary.
struct Row<'a> {
    session: &'a str,
    actor: &'a str,
    change: &'a str,
    state: &'a str,
    at: u64,
}

/// Pushes the remote's `refs/bay/live/<machine>` as that machine would: a summary written now
/// with `row`, committed by `signer` (without a key registry the committer names the actor).
fn heartbeat(test_box: &TestBox, repo: &Path, machine: &str, signer: &str, row: &Row<'_>) {
    let summary = serde_json::json!({
        "schema": "bay.live/1",
        "machine": machine,
        "at": bay::clock::rfc3339_of(unix_now()),
        "sessions": [{
            "session": row.session,
            "actor": {"name": row.actor, "session": row.session},
            "change": row.change,
            "state": row.state,
            "event": "post_tool_batch",
            "at": bay::clock::rfc3339_of(row.at),
        }],
    });
    let blob = git_with_input(
        test_box,
        repo,
        signer,
        &["hash-object", "-w", "--stdin"],
        summary.to_string().as_bytes(),
    );
    let tree = git_with_input(
        test_box,
        repo,
        signer,
        &["mktree"],
        format!("100644 blob {blob}\tlive.json\n").as_bytes(),
    );
    let commit = git_with_input(
        test_box,
        repo,
        signer,
        &["commit-tree", &tree, "--no-gpg-sign", "-m", "live"],
        b"",
    );
    test_box.git_ok(
        repo,
        &[
            "push",
            "-q",
            "origin",
            &format!("+{commit}:refs/bay/live/{machine}"),
        ],
    );
}

/// Rulings-open-questions.md Q8: a same-name `start` while another session of that name has a
/// fresh, actor-signed heartbeat naming the change is `CHANGE_HELD` and appends no adoption;
/// the holder's own `start` adopts its change; once the holder's session ends, or its heartbeat
/// is older than the takeover window, the same-name `start` adopts at once with the change id
/// kept; and a heartbeat signed by another actor holds nothing.
#[test]
fn a_same_name_start_while_the_holder_is_live_is_held() {
    let (test_box, repo, remote) = setup();
    let start = |session: &str, intent: &str| {
        result_json(
            test_box
                .bay(&repo)
                .env("BAY_ACTOR", "agent-a")
                .env("BAY_SESSION", session)
                .args(["start", intent, "--paths", "src/"]),
        )
    };
    let chain_tip =
        |id: &str| test_box.git_bare(&remote, &["rev-parse", &format!("refs/bay/changes/{id}")]);
    let (code, started) = start("s-1", "CARD-HELD");
    assert_eq!(code, 0, "{started}");
    let id = started["change"].as_str().unwrap().to_owned();
    let live = |state: &'static str, at: u64| Row {
        session: "s-1",
        actor: "agent-a",
        change: &id,
        state,
        at,
    };
    heartbeat(
        &test_box,
        &repo,
        "box-a",
        "agent-a",
        &live("working", unix_now()),
    );
    let before = chain_tip(&id);

    let (code, held) = start("s-2", "CARD-HELD");
    assert_eq!(code, 10, "{held}");
    assert_eq!(held["reason"], "CHANGE_HELD", "{held}");
    assert_eq!(held["change"], id.as_str(), "{held}");
    assert_eq!(held["detail"]["holder"]["actor"], "agent-a", "{held}");
    assert_eq!(held["detail"]["holder"]["session"], "s-1", "{held}");
    assert_eq!(held["detail"]["machine"], "box-a", "{held}");
    assert!(held["detail"]["heartbeat_age_s"].is_u64(), "{held}");
    assert_eq!(chain_tip(&id), before, "a held start appended an adoption");

    // The holder's own session re-running `start` is not another session.
    let (code, own) = start("s-1", "CARD-HELD");
    assert_eq!(code, 0, "{own}");
    assert_eq!(own["change"], id.as_str());
    assert_eq!(own["detail"]["adopted"], false, "{own}");

    // The holder's session ends: the same-name start adopts at once, and the id is kept.
    heartbeat(
        &test_box,
        &repo,
        "box-a",
        "agent-a",
        &live("ended", unix_now()),
    );
    let (code, adopted) = start("s-2", "CARD-HELD");
    assert_eq!(code, 0, "{adopted}");
    assert_eq!(adopted["change"], id.as_str());
    assert_eq!(adopted["detail"]["adopted"], true, "{adopted}");
    assert_ne!(chain_tip(&id), before);

    // A heartbeat older than the takeover window (twice the default 60 s lease) holds nothing.
    let (_, aged) = start("s-1", "CARD-AGED");
    let aged = aged["change"].as_str().unwrap().to_owned();
    let old = unix_now() - 600;
    heartbeat(
        &test_box,
        &repo,
        "box-a",
        "agent-a",
        &Row {
            change: &aged,
            ..live("working", old)
        },
    );
    let (code, adopted) = start("s-2", "CARD-AGED");
    assert_eq!(code, 0, "{adopted}");
    assert_eq!(adopted["change"], aged.as_str());

    // A fresh heartbeat naming the change but signed by another actor holds nothing.
    let (_, forged) = start("s-1", "CARD-FORGED");
    let forged = forged["change"].as_str().unwrap().to_owned();
    heartbeat(
        &test_box,
        &repo,
        "box-b",
        "agent-b",
        &Row {
            change: &forged,
            ..live("working", unix_now())
        },
    );
    let (code, adopted) = start("s-2", "CARD-FORGED");
    assert_eq!(code, 0, "{adopted}");
    assert_eq!(adopted["change"], forged.as_str());
}
