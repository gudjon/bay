//! G756 (owner, 2026-11-16: a panel for busy people must feel instant, and it waited 23 s on
//! `bay queue` and 15 s on `bay doctor` while `bay status --deadline 0` answered in 0.03 s).
//! `bay queue --deadline 0` lists the line from this machine's copy, with no call to the
//! remote, and says so; `bay doctor --deadline 0` probes nothing and answers with this
//! machine's last verdict and its age, or says it holds none. Past the store's sweep window the
//! read starts the sweep and leaves it running (G837): the remote calls are the sweep's, made by
//! the process the read names, none by the read.
//!
//! Guards to mutate: in `src/line.rs` `queue`, read with `from_store` false, and the line's read
//! calls the remote; in `src/main.rs`, run the probe for `--deadline 0`.

#![cfg(unix)]

use crate::support;

use support::queued::{Check, host};

#[test]
fn a_read_at_deadline_zero_answers_from_this_machine() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 60\nheartbeat_s = 1\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let one = held.queue("one", "src");
    let two = held.queue("two", "lib");

    // GIVEN a remote that refuses every call, each one logged
    let calls = held.test_box.path().join("remote-calls");
    held.test_box.wrap_git(&format!(
        "case \" $* \" in\n  *\" fetch \"*|*\" ls-remote \"*|*\" push \"*) echo \"$PPID $*\" >> '{}'; exit 1 ;;\nesac",
        calls.display()
    ));

    // WHEN the line is read at deadline zero
    let line = held
        .test_box
        .bay_ok(&held.repo, &["queue", "--deadline", "0"]);

    // THEN it lists this machine's copy of the line, says so, and called nothing remote
    let detail = &line["detail"];
    assert_eq!(detail["from_store"], true, "{line}");
    let ids: Vec<&str> = detail["places"]
        .as_array()
        .unwrap()
        .iter()
        .map(|place| place["change"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![one.as_str(), two.as_str()], "{line}");
    let sweep = detail["sweep"]["by"]["pid"].as_u64();
    if let Some(pid) = sweep {
        support::process_ended(pid);
    }
    let callers: Vec<String> = std::fs::read_to_string(&calls)
        .unwrap_or_default()
        .lines()
        .map(|call| call.split(' ').next().unwrap_or_default().to_owned())
        .collect();
    assert!(
        callers
            .iter()
            .all(|caller| Some(caller.as_str()) == sweep.map(|pid| pid.to_string()).as_deref()),
        "the read called the remote: {callers:?} {line}"
    );
    let _ = std::fs::remove_file(&calls);

    // AND doctor at deadline zero says it holds no verdict, probing nothing
    let doctor = held
        .test_box
        .bay_ok(&held.repo, &["doctor", "--deadline", "0"]);
    assert_eq!(doctor["detail"]["from_store"], true, "{doctor}");
    assert!(doctor["detail"]["verdict"].is_null(), "{doctor}");
    assert!(!calls.exists(), "doctor called the remote: {doctor}");
}

#[test]
fn doctor_at_deadline_zero_serves_its_last_verdict_with_its_age() {
    let test_box = support::TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &support::unchecked_files(),
    );
    // A real probe publishes this machine's verdict.
    let (_, probed) = support::result_json(test_box.bay(&repo).arg("doctor"));
    let saved = test_box.bay_ok(&repo, &["doctor", "--deadline", "0"]);
    let verdict = &saved["detail"]["verdict"];
    assert_eq!(verdict["decision"], probed["decision"], "{saved} {probed}");
    assert!(saved["detail"]["age_s"].as_u64().is_some(), "{saved}");
}
