//! G693 (a fleet finding, 2026-11-10: three runners that did not serve a change's environment
//! took it twelve times in fifteen minutes, each spending a minute to record the same wait). A
//! visit judges an operation's wait for a worker on the chain its pass read; a claim made since
//! ends the wait until its holder records it again (G690), so that read can show none. The claim
//! reads the chain again: an operation that waits there for an environment this box does not
//! serve is left to a box that does, unclaimed.
//!
//! Guard to mutate: in `src/publish.rs` `claim`, the `waits_for_worker` call (the box that does
//! not serve the check takes the operation over at once, as G425 lets a serving box do).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use support::{TestBox, check_files, passing_result, result_json};

/// An executor clone of `remote` named `name`, whose clone declares `environment`.
fn executor(test_box: &TestBox, remote: &Path, name: &str, environment: &str) -> PathBuf {
    let clone = test_box.clone_of(remote, name);
    test_box.git_ok(&clone, &["config", "bay.runnerName", name]);
    test_box.git_ok(
        &clone,
        &["config", "bay.producerName", &format!("{name}-checks")],
    );
    test_box.git_ok(&clone, &["config", "bay.checkEnvironment", environment]);
    clone
}

/// The claims written on change `id`'s chain at the remote, oldest first.
fn claims(test_box: &TestBox, remote: &Path, id: &str) -> Vec<String> {
    test_box
        .git_bare(
            remote,
            &[
                "log",
                "--reverse",
                "--format=%s",
                &format!("refs/bay/changes/{id}"),
            ],
        )
        .lines()
        .filter(|line| line.starts_with("claim "))
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_box_that_cannot_serve_a_wait_never_claims_it_on_an_old_read() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    // One required check, in env-b, which neither executor here serves.
    let files: Vec<(String, String)> =
        check_files(&test_box.relay("true"), &passing_result(1), 1, 60)
            .into_iter()
            .map(|(path, text)| (path, text.replace("\"local\"", "\"env-b\"")))
            .chain([(
                ".bay/config.toml".to_owned(),
                "[runner]\nlease_s = 60\nheartbeat_s = 1\n".to_owned(),
            )])
            .collect();
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");

    // GIVEN a box that read the operation before any wait, held just before its claim
    let late = executor(&test_box, &remote, "exec-c", "env-c");
    let go = test_box.path().join("go");
    let mut held = test_box
        .bay_process(&late)
        .env("BAY_TEST_PAUSE_AT", "visit-gated")
        .env("BAY_TEST_PAUSE_FILE", &go)
        .args(["--json", "runner", "run", "--once"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let paused = test_box.path().join("go.paused");
    let deadline = Instant::now() + Duration::from_secs(120);
    while !paused.exists() {
        assert!(
            held.try_wait().unwrap().is_none(),
            "exec-c ended before its claim"
        );
        assert!(Instant::now() < deadline, "exec-c never reached its claim");
        std::thread::sleep(Duration::from_millis(50));
    }
    // AND another box that cannot serve it recorded the wait meanwhile
    let first = executor(&test_box, &remote, "exec-a", "env-a");
    let waited = test_box.bay_ok(&first, &["runner", "run", "--once"]);
    assert_eq!(
        waited["detail"]["ran"][0]["reason"], "CHECK_UNAVAILABLE",
        "{waited}"
    );

    // WHEN the held box goes on to its claim
    std::fs::write(&go, "go").unwrap();
    let out = held.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    // THEN it leaves the operation waiting, unclaimed
    let claimed = claims(&test_box, &remote, &id);
    assert!(
        !claimed.iter().any(|claim| claim.contains("by exec-c")),
        "exec-c took a wait it cannot serve: {claimed:?}"
    );
}
