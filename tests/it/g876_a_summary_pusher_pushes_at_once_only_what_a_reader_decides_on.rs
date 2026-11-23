//! G876 (a measurement, 2026-11-23): a machine's summary pusher pushes at once only what a
//! reader decides on.
//!
//! A machine of 14 sessions pushed its live summary about 190 times an hour, about 8 s each,
//! while which session held which change for which actor changed about 30 times an hour: each
//! hook event moved a row's time or state, and the pusher pushed again one heartbeat later. The
//! hook's pusher (`bay live push --paced`) now pushes at once when a session starts, ends or
//! holds another change, and a change only in a row's time or state at the keep-alive, a sixth
//! of the takeover window, so the summary out is never more than that behind its records.
//!
//! Guards to mutate:
//! - `src/live.rs` `due`: answer `Due::Now("keep_alive")` in place of `Due::Wait` (a time-only
//!   change is pushed before the keep-alive).
//! - `src/live.rs` `due`: drop the `holders` comparison (a new session waits for the keep-alive).

use crate::support;

use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::{TestBox, UNCHECKED_POLICY};

/// The takeover window of this case; its keep-alive is a sixth of it.
const TAKEOVER_S: u64 = 60;
const KEEP_ALIVE_S: u64 = TAKEOVER_S / 6;

/// `bay hook <event>` of the recorded Claude Code payload `name`, for `session` in `repo`.
fn hook(test_box: &TestBox, repo: &Path, event: &str, name: &str, session: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code-2.1.302")
        .join(name);
    let mut payload: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    payload["session_id"] = json!(session);
    payload["cwd"] = json!(repo);
    let mut child = test_box
        .bay_process(repo)
        .args(["hook", event])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}

/// Waits until the pusher the hook started has pushed another commit than `before` (an outcome
/// it kept, or `Null`) and ended, then returns the outcome it kept.
fn pushed(repo: &Path, before: &Value) -> Value {
    let lock = repo.join(".git/bay/live-push.lock");
    let kept = repo.join(".git/bay/live-push.json");
    let asked = Instant::now();
    loop {
        let outcome = std::fs::read_to_string(&kept)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        if let Some(outcome) = outcome
            && outcome["detail"]["commit"] != before["detail"]["commit"]
            && !lock.exists()
        {
            return outcome;
        }
        assert!(
            asked.elapsed() < Duration::from_secs(KEEP_ALIVE_S * 4),
            "the live pusher of {} pushed nothing new; it kept {:?}; bay dir {:?}",
            repo.display(),
            std::fs::read_to_string(&kept).ok(),
            std::fs::read_dir(repo.join(".git/bay"))
                .map(|entries| entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name())
                    .collect::<Vec<_>>())
                .ok()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The summary live commit `commit` carries, read in `repo`.
fn summary(test_box: &TestBox, repo: &Path, commit: &str) -> Value {
    serde_json::from_str(&test_box.git_ok(repo, &["show", &format!("{commit}:live.json")])).unwrap()
}

/// The Unix time of a summary's `at`.
fn seconds(at: &Value) -> u64 {
    bay::clock::unix_of(at.as_str().unwrap_or_else(|| panic!("no time: {at}"))).unwrap()
}

#[test]
fn a_new_holder_is_pushed_at_once_and_a_time_only_change_at_the_keep_alive() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[
            (".bay/policy.toml".to_owned(), UNCHECKED_POLICY.to_owned()),
            (
                ".bay/config.toml".to_owned(),
                format!("[runner]\nlease_s = 2\nheartbeat_s = 1\ntakeover_s = {TAKEOVER_S}\n"),
            ),
        ],
    );
    test_box.git_ok(&repo, &["config", "bay.machine", "box-a"]);
    // A first session: nothing is out yet, so it goes out at once.
    hook(
        &test_box,
        &repo,
        "session.start",
        "01-SessionStart.json",
        "s-a",
    );
    let first = pushed(&repo, &Value::Null);
    assert_eq!(first["ok"], true, "{first}");
    let first_at = summary(
        &test_box,
        &repo,
        first["detail"]["commit"].as_str().unwrap(),
    )["at"]
        .clone();

    // Its next step moves only its row's time and state: it goes out at the keep-alive.
    hook(&test_box, &repo, "step.end", "08-PostToolBatch.json", "s-a");
    let step = pushed(&repo, &first);
    assert_eq!(step["detail"]["why"], "keep_alive", "{step}");
    let step_summary = summary(&test_box, &repo, step["detail"]["commit"].as_str().unwrap());
    let waited = seconds(&step_summary["at"]).saturating_sub(seconds(&first_at));
    assert!(
        waited >= KEEP_ALIVE_S,
        "a time-only change went out {waited} s after the summary before it: {step_summary}"
    );
    let remote = test_box.git_ok(&repo, &["ls-remote", "origin", "refs/bay/live/box-a"]);
    assert!(
        remote.starts_with(step["detail"]["commit"].as_str().unwrap()),
        "{remote}"
    );

    // A second session starts: a reader decides on that, so it goes out at once.
    hook(
        &test_box,
        &repo,
        "session.start",
        "01-SessionStart.json",
        "s-b",
    );
    let started = pushed(&repo, &step);
    assert_eq!(started["detail"]["why"], "holders", "{started}");
    let rows = summary(
        &test_box,
        &repo,
        started["detail"]["commit"].as_str().unwrap(),
    );
    let sessions: Vec<&Value> = rows["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| &row["session"])
        .collect();
    assert_eq!(sessions, [&json!("s-a"), &json!("s-b")], "{rows}");
}
