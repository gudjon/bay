//! G372 (a fleet finding, 2026-10-25: a runner landing a long queue read as stale for twenty
//! minutes, and seats were told no runner was running). A busy runner never reads as gone.
//!
//! One runner pass carries the queue as far as it can, so it can run long. Its presence beats
//! before each operation it runs (at most once a heartbeat), naming the operation it holds, never
//! only at the pass's start.
//!
//! Guard to mutate: `src/executor.rs` `pass`: drop the `beat.publish` before `publish::execute`
//! (the presence still names the first operation while the second one runs).
//!
//! 0.5.3 (the 0.5.1 acceptance finding): it beats once a heartbeat during one long check too,
//! so a check longer than `stale_after_s` never reads as gone. Guard to mutate:
//! `src/presence.rs` `Beat::during`: drop the ticker's `publish` (the presence's time stays put
//! while the check runs).

// The check is a shell script, so this runs on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn the_presence_names_each_operation_as_the_pass_reaches_it() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let log = test_box.path().join("holding.log");
    // Each check run waits past a heartbeat, then records what the remote's presence holds.
    let seen = test_box.relay(&format!(
        "git --git-dir='{remote}' show refs/bay/executor/local:presence.json | sed -n 's/.*\"holding\": *\"\\([^\"]*\\)\".*/\\1/p' >> '{log}'",
        remote = remote.display(),
        log = log.display()
    ));
    let mut files = check_files(&format!("sleep 2; {seen}"), &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\nheartbeat_s = 1\nlease_s = 30\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let mut ops = Vec::new();
    for n in 0..2 {
        let (_, view) = test_box.start_and_edit(&repo);
        // Each change a file of its own, so the second still has work after the first lands.
        std::fs::remove_file(view.join("src/new.rs")).unwrap();
        std::fs::write(
            view.join(format!("src/own{n}.rs")),
            format!("fn own{n}() {{}}\n"),
        )
        .unwrap();
        let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
        assert_eq!(code, 0, "{queued}");
        ops.push(queued["op_id"].as_str().unwrap().to_owned());
    }
    let runner = test_box.clone_of(&remote, "runner");
    let (code, ran) = result_json(test_box.bay(&runner).args(["runner", "run", "--once"]));
    assert_eq!(code, 0, "{ran}");

    let holding = std::fs::read_to_string(&log).unwrap_or_default();
    let last = holding.lines().last().unwrap_or_default();
    assert!(
        ops.iter().any(|op| op == last) && holding.lines().collect::<Vec<_>>().len() >= 2,
        "the runs saw {holding:?}, ops {ops:?}"
    );
    let first = holding.lines().next().unwrap_or_default();
    assert_ne!(
        first, last,
        "the presence still named the pass's first operation while the next one ran: {holding:?}"
    );
}

#[test]
fn the_presence_beats_while_one_long_check_runs() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let log = test_box.path().join("at.log");
    // The check notes the presence's time, waits past several heartbeats, and notes it again.
    let at = format!(
        "git --git-dir='{remote}' show refs/bay/executor/local:presence.json | sed -n 's/.*\"at\": *\"\\([^\"]*\\)\".*/\\1/p' >> '{log}'",
        remote = remote.display(),
        log = log.display()
    );
    let seen = test_box.relay(&format!("{at}; sleep 4; {at}"));
    let mut files = check_files(&seen, &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\nheartbeat_s = 1\nlease_s = 30\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let runner = test_box.clone_of(&remote, "runner");
    let (code, ran) = result_json(test_box.bay(&runner).args(["runner", "run", "--once"]));
    assert_eq!(code, 0, "{ran}");

    let seen = std::fs::read_to_string(&log).unwrap_or_default();
    let times: Vec<&str> = seen.lines().collect();
    assert_eq!(times.len(), 2, "{seen:?}");
    assert_ne!(times[0], times[1], "no beat during the check: {seen:?}");
}
