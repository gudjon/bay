//! G293. One word per idea.
//!
//! Each idea a caller acts on has one word, and an old spelling keeps working beside it:
//!
//! - every reason's `allowed` is one of four actions, `read`, `refresh`, `repair` and `wait`,
//!   and the schema of `bay.result/1` lists exactly those four;
//! - a landed change closes its generation, so a plain `bay start <intent>` after the landing
//!   claims the next generation, whose change supersedes the landed one and lands once;
//!   `--new` is still accepted there and changes nothing;
//! - `bay merge --wait=SECONDS` watches the run that long, then returns `queued`/`running`
//!   while the run goes on, as `--wait --observe SECONDS` does; `bay undo` takes the same flag;
//! - `bay merge <change>` names the change positionally, as `status`, `undo` and `share` do;
//!   `--change <change>` still works.
//!
//! Guards to mutate:
//! - `src/result.rs`, the `reasons!` table: put back `["edit_locally"]` on the `QUEUED`
//!   row (or `["resume"]` on `VIEW_INCOMPLETE`): the first case fails.
//! - `src/start.rs` `open`: the arm `(true, _) => (generation + 1, Some(change.clone())),`
//!   (make it `(true, _) => (*generation, Some(change.clone())),`, or put back the 0.2.6 pair
//!   that refuses `(true, false)` with `CHANGE_ALREADY_MERGED`): the second case's plain start
//!   claims no next generation.
//! - `src/main.rs` `WaitArgs::wait`: `let seconds = self.wait.flatten().or(self.observe);`
//!   (make it `let seconds = self.observe;`): `--wait=2` waits for the whole run, and the third
//!   case's caller is not back within its bound.
//! - `src/main.rs` `main`, the `Command::Land` arm: `change.or(change_flag)` (make it
//!   `change_flag`): the fourth case's positional change is dropped, and a land outside a view
//!   names no change.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bay::result::Reason;
use support::{TestBox, check_files, passing_result, result_json};

/// The four actions a refusal may still allow.
const ACTIONS: [&str; 4] = ["read", "refresh", "repair", "wait"];

/// A repository whose one required check runs `before` first, then passes.
fn seeded(before: &str) -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files(before, &passing_result(1), 1, 60),
    );
    (test_box, repo, remote)
}

/// `bay start <args>` in `repo`, which must open or adopt a change; then `file` is written in
/// its view. The result and the view.
fn start_and_write(
    test_box: &TestBox,
    repo: &Path,
    args: &[&str],
    file: &str,
) -> (serde_json::Value, PathBuf) {
    let started = test_box.bay_ok(repo, &[&["start"][..], args].concat());
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(file), "fn x() {}\n").unwrap();
    (started, view)
}

/// Waits until trunk on `remote` carries `id`, at most a minute.
fn until_landed(test_box: &TestBox, remote: &Path, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !test_box
        .trunk_change_ids(remote)
        .iter()
        .any(|landed| landed == id)
    {
        assert!(
            Instant::now() < deadline,
            "{id} did not land after the caller left"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
fn every_allowed_word_is_one_of_four() {
    for reason in Reason::ALL {
        for word in reason.allowed() {
            assert!(
                ACTIONS.contains(word),
                "{} allows {word}, which is not one of {ACTIONS:?}",
                reason.code()
            );
        }
    }
    // The schema the binary prints lists the four, and no other.
    let test_box = TestBox::new();
    let contract = test_box.bay_ok(test_box.path(), &["help"]);
    let listed = &contract["detail"]["schema"]["properties"]["allowed"]["items"]["enum"];
    assert_eq!(*listed, serde_json::json!(ACTIONS), "{listed}");
}

#[test]
fn a_plain_start_after_a_landing_opens_the_next_generation() {
    let (test_box, repo, remote) = seeded("true");
    let (first, first_view) = start_and_write(
        &test_box,
        &repo,
        &["CARD-293", "--paths", "src/"],
        "src/one.rs",
    );
    let first_id = first["change"].as_str().unwrap().to_owned();
    let (code, landed) = result_json(test_box.bay(&first_view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");

    // The same intent again, with no --new: the next generation, superseding the landing.
    let (second, second_view) = start_and_write(
        &test_box,
        &repo,
        &["CARD-293", "--paths", "src/"],
        "src/two.rs",
    );
    let second_id = second["change"].as_str().unwrap().to_owned();
    assert_ne!(second_id, first_id, "{second}");
    assert!(
        second["detail"]["intent"]
            .as_str()
            .is_some_and(|claim| claim.ends_with("/2")),
        "{second}"
    );
    assert_eq!(
        second["detail"]["supersedes"],
        first_id.as_str(),
        "{second}"
    );
    let (code, landed) = result_json(test_box.bay(&second_view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(
        test_box.trunk_change_ids(&remote),
        vec![second_id.clone(), first_id.clone()]
    );
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

    // Each generation lands once: the first change's view, merged and clean, was released when
    // the second started, so nothing can land from it again.
    assert!(!first_view.exists(), "{}", first_view.display());
    assert_eq!(test_box.trunk_change_ids(&remote).len(), 2);

    // --new is still accepted after a landing, and does what a plain start does.
    let (third, _) = start_and_write(
        &test_box,
        &repo,
        &["CARD-293", "--new", "--paths", "src/"],
        "src/three.rs",
    );
    assert!(
        third["detail"]["intent"]
            .as_str()
            .is_some_and(|claim| claim.ends_with("/3")),
        "{third}"
    );
    assert_eq!(third["detail"]["supersedes"], second_id.as_str(), "{third}");
}

#[test]
fn merge_wait_seconds_observes_and_detaches() {
    // The check outlasts the caller's bound by far, so a caller that waited for the run cannot
    // pass on a slow machine, and one that detached cannot fail on one.
    let (test_box, repo, remote) = seeded("sleep 15");
    let (started, view) = start_and_write(
        &test_box,
        &repo,
        &["CARD-293a", "--paths", "src/"],
        "src/one.rs",
    );
    let id = started["change"].as_str().unwrap().to_owned();
    let asked = Instant::now();
    let (code, detached) = result_json(test_box.bay(&view).args(["merge", "--wait=2"]));
    let waited = asked.elapsed();
    assert_eq!(code, 0, "{detached}");
    assert!(
        waited < Duration::from_secs(10),
        "the caller waited for the run: {waited:?}"
    );
    assert_eq!(detached["decision"], "queued", "{detached}");
    assert_eq!(detached["state"], "running", "{detached}");
    assert_eq!(detached["detail"]["observation_timeout_s"], 2, "{detached}");
    // The run goes on without its caller.
    until_landed(&test_box, &remote, &id);

    // `bay undo` takes the same flag: it parses, and the refusal is the undo's own.
    let (code, undo) = result_json(test_box.bay(&repo).args(["undo", "--wait=2", "abcd2345"]));
    assert_eq!(code, 10, "{undo}");
    assert_eq!(undo["reason"], "CHANGE_NOT_MERGED", "{undo}");
}

#[test]
fn land_takes_the_change_positionally() {
    let (test_box, repo, remote) = seeded("true");
    let (first, _) = start_and_write(
        &test_box,
        &repo,
        &["CARD-293p", "--paths", "src/"],
        "src/p.rs",
    );
    let first_id = first["change"].as_str().unwrap().to_owned();
    // Outside the change's view, the change is named positionally, before or after --wait.
    let (code, landed) = result_json(test_box.bay(&repo).args(["merge", &first_id, "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert_eq!(landed["change"], first_id.as_str(), "{landed}");

    let (second, _) = start_and_write(
        &test_box,
        &repo,
        &["CARD-293q", "--paths", "src/"],
        "src/q.rs",
    );
    let second_id = second["change"].as_str().unwrap().to_owned();
    let (code, landed) = result_json(test_box.bay(&repo).args(["merge", "--wait", &second_id]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["change"], second_id.as_str(), "{landed}");

    // --change still names it.
    let (third, _) = start_and_write(
        &test_box,
        &repo,
        &["CARD-293r", "--paths", "src/"],
        "src/r.rs",
    );
    let third_id = third["change"].as_str().unwrap().to_owned();
    let (code, landed) = result_json(test_box.bay(&repo).args(["merge", "--wait", &third_id]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["change"], third_id.as_str(), "{landed}");
    assert_eq!(
        test_box.trunk_change_ids(&remote),
        vec![third_id, second_id, first_id]
    );
}
