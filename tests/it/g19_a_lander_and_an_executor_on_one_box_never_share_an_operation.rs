//! G19. Two executors never publish one operation twice: on one box too.
//!
//! An agent's `bay land --wait` and a background `bay executor run` on the same machine share
//! the local change chain ref, and every reader of the remote chain (an executor's pass, a
//! claim's re-read) moves that ref to the remote's value. A write one process appended there,
//! and had not pushed yet, is then gone from the ref. A push of the ref would succeed without
//! it: "up to date". So each process pushes its own head, and a push is ownership only when it
//! carried that head.
//!
//! - A resubmission after `CHECK_FAILED` whose new request is rewound off the local ref just
//!   before its push still reaches the remote, and lands: no panic over an operation missing
//!   from the record, no `CHAIN_MOVED` (the second `land --wait` of a two-agent race).
//! - A lander whose claim is rewound off the local ref while the executor claims and checks the
//!   operation loses the claim's compare-and-swap: its claim never reaches the remote, and it
//!   never faults at `persist` (two `persist` fault receipts and an operation left `running`).
//! - A `land --wait` that loses its claim to a live executor never answers `ok` while nothing
//!   landed (the third `land --wait` of the race printed `ok`, and the executor was then stopped
//!   with the operation `running`): it waits for the operation's outcome, takes it over once
//!   the holder's lease passes, and the change lands once.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use support::{TestBox, check_files, passing_result, result_json};

/// The one check: a unit that fails while `src/new.rs` holds the word `broken`.
fn broken_fails() -> Vec<(String, String)> {
    let result = passing_result(1).replace("\\\"pass\\\"", "\\\"$verdict\\\"");
    check_files(
        "if grep -q broken src/new.rs; then verdict=product_failure; else verdict=pass; fi",
        &result,
        1,
        60,
    )
}

/// The remote's chain head of change `id`.
fn remote_chain(test_box: &TestBox, remote: &Path, id: &str) -> String {
    test_box.git_bare(remote, &["rev-parse", &format!("refs/bay/changes/{id}")])
}

/// Waits until a process paused at a publication boundary wrote `<release>.paused`.
fn wait_paused(release: &Path, what: &str) {
    let paused = PathBuf::from(format!("{}.paused", release.display()));
    // The gate box runs on two cores at nice 19: allow it time to reach the boundary.
    let deadline = Instant::now() + Duration::from_secs(120);
    while !paused.exists() {
        assert!(
            Instant::now() < deadline,
            "{what} never reached its boundary"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A `bay` process held at a publication boundary. A case that fails while it is held kills
/// it, so no process outlives its test waiting for a release file in a removed directory.
struct Held(Option<Child>);

impl Drop for Held {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The result document a held `bay` process printed once it finished, with its exit code.
fn finished(held: &mut Held, what: &str) -> (Option<i32>, serde_json::Value) {
    let out = held.0.take().unwrap().wait_with_output().unwrap();
    let result = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "{what} printed no result (exit {:?}): {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.code(), result)
}

/// B's change is refused `CHECK_FAILED`; B fixes it and lands again at once, and a reader of
/// the remote chain moves this box's chain ref back just before each request push. The
/// request is pushed as the lander wrote it, so the remote holds the new operation, the claim
/// finds it, and the change lands.
#[test]
fn a_resubmission_rewound_off_the_local_chain_still_lands() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &broken_fails());
    let (id, view) = test_box.start_and_edit(&repo);
    std::fs::write(view.join("src/new.rs"), "broken\n").unwrap();
    let (_, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(refused["decision"], "blocked", "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();

    // From now on, every push that moves the requests ref first lets another reader of the
    // remote chain run on this box: the local chain ref goes back to the remote's head.
    let chain = format!("refs/bay/changes/{id}");
    let rewinds = test_box.path().join("rewinds");
    test_box.wrap_git(&format!(
        "case \" $* \" in\n  *\" push \"*refs/bay/requests/*)\n    \"$real\" -C '{}' fetch -q --no-tags origin '+{chain}:{chain}'\n    echo rewound >> '{}'\n    ;;\nesac",
        repo.display(),
        rewinds.display()
    ));
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert!(rewinds.exists(), "the request push never ran");
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "landed", "{landed}");
    assert_eq!(landed["op_id"], format!("{id}.2"), "{landed}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}

/// The lander and the executor on one box take the same operation. The lander wrote its claim
/// and is held before the claim's push; a reader moves the local chain ref back, and the
/// executor claims, checks and persists the operation, then dies before it publishes. The
/// lander resumes: its claim's push carries its own claim, which the remote refuses, so the
/// executor's epoch stands and the lander never faults at `persist`. It waits for the outcome:
/// once the dead executor's lease passes it takes the operation over and lands it once.
#[test]
fn a_claim_rewound_off_the_local_chain_is_never_taken_for_ownership() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{queued}");
    let submitted = remote_chain(&test_box, &remote, &id);

    let release = test_box.path().join("release-lander");
    let child = test_box
        .bay_process(&view)
        .env("BAY_TEST_PAUSE_AT", "before-claim-push")
        .env("BAY_TEST_PAUSE_FILE", &release)
        .args(["--json", "land", "--wait"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lander = Held(Some(child));
    wait_paused(&release, "the lander");
    let chain = format!("refs/bay/changes/{id}");
    test_box.git_ok(
        &repo,
        &[
            "fetch",
            "-q",
            "--no-tags",
            "origin",
            &format!("+{chain}:{chain}"),
        ],
    );
    assert_eq!(test_box.git_ok(&repo, &["rev-parse", &chain]), submitted);

    let executor = test_box
        .bay(&repo)
        .env("BAY_TEST_KILL_AT", "before-publish")
        .args(["--json", "executor", "run", "--once"])
        .output()
        .unwrap();
    assert_eq!(executor.status.code(), Some(137), "{executor:?}");
    let persisted = remote_chain(&test_box, &remote, &id);
    assert_ne!(persisted, submitted, "the executor claimed nothing");

    std::fs::write(&release, "go").unwrap();
    let (code, answer) = finished(&mut lander, "the lander");
    assert_eq!(code, Some(0), "{answer}");
    assert_eq!(answer["decision"], "landed", "{answer}");
    assert_eq!(answer["op_id"], format!("{id}.1"), "{answer}");
    let claims = claims_on_remote(&test_box, &remote, &id);
    assert_eq!(
        claims
            .iter()
            .filter(|claim| claim.contains(" epoch 1 "))
            .count(),
        1,
        "the lander's lost claim reached the remote: {claims:?}"
    );
    assert!(
        claims.iter().any(|claim| claim.contains(" epoch 2 ")),
        "the lander never took the operation over: {claims:?}"
    );

    test_box.bay_ok(&repo, &["executor", "run", "--once"]);
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}

/// The claim commits on the remote chain of change `id`, newest first.
fn claims_on_remote(test_box: &TestBox, remote: &Path, id: &str) -> Vec<String> {
    test_box
        .git_bare(
            remote,
            &["log", "--format=%s", &format!("refs/bay/changes/{id}")],
        )
        .lines()
        .filter(|subject| subject.starts_with("claim "))
        .map(str::to_owned)
        .collect()
}

/// The resubmission race of two agents and a background executor on one box: B's
/// `land --wait` read its operation unclaimed and is about to claim it when the executor
/// claims, checks and persists it, and is held before it publishes. The lander's claim loses
/// the compare-and-swap on this box's chain ref. Nothing has landed, so the lander never
/// answers `ok` for it: it waits for the operation's outcome, takes the operation over once
/// the held executor's lease passes, and lands it. Its lost claim never reaches the remote;
/// the executor, released, finds the landing and publishes nothing.
///
/// Guard: `not_taken` in `wait_land` (`src/publish.rs`). Made to answer `false`, the lander
/// prints `ok` with `superseded` while trunk holds nothing of the change.
#[test]
fn a_land_wait_that_loses_its_claim_to_a_live_executor_answers_with_the_outcome() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{queued}");

    // The lander has read the operation unclaimed and is held just before its claim.
    let release_lander = test_box.path().join("release-lander");
    let child = test_box
        .bay_process(&view)
        .env("BAY_TEST_PAUSE_AT", "before-claim")
        .env("BAY_TEST_PAUSE_FILE", &release_lander)
        .args(["--json", "land", "--wait"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lander = Held(Some(child));
    wait_paused(&release_lander, "the lander");

    // The executor claims, checks and persists it, and is held before it publishes.
    let release_executor = test_box.path().join("release-executor");
    let child = test_box
        .bay_process(&repo)
        .env("BAY_TEST_PAUSE_AT", "after-persist")
        .env("BAY_TEST_PAUSE_FILE", &release_executor)
        .args(["--json", "executor", "run", "--once"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut executor = Held(Some(child));
    wait_paused(&release_executor, "the executor");
    assert_eq!(test_box.trunk_change_ids(&remote), Vec::<String>::new());

    std::fs::write(&release_lander, "go").unwrap();
    let (code, answer) = finished(&mut lander, "the lander");
    assert_eq!(
        answer["decision"], "landed",
        "land --wait answered without the operation's outcome: {answer}"
    );
    assert_eq!(code, Some(0), "{answer}");
    assert_eq!(answer["op_id"], format!("{id}.1"), "{answer}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id.clone()]);
    let claims = claims_on_remote(&test_box, &remote, &id);
    assert_eq!(
        claims
            .iter()
            .filter(|claim| claim.contains(" epoch 1 "))
            .count(),
        1,
        "the lander's lost claim reached the remote: {claims:?}"
    );

    std::fs::write(&release_executor, "go").unwrap();
    let (code, ran) = finished(&mut executor, "the executor");
    assert_eq!(code, Some(0), "{ran}");
    let late = &ran["detail"]["ran"][0];
    assert!(
        late["detail"]["existing"] == true || late["detail"]["superseded"] == true,
        "{ran}"
    );
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}
