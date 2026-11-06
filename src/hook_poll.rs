//! BACKLOG 218 (a fleet finding, 2026-11-02: "`UserPromptSubmit` hook [... `bay hook
//! turn.start`] timed out after 30s — output discarded", too often): a hook does its work with
//! the remote in a child it does not wait for. The hook polled its talks (G54), fetched the
//! fleet's presence (G427) and closed its question talks (dossier 63) itself, one call after
//! another, each with the network bound of its own (180 s, [`crate::result::GIT_NETWORK_TIMEOUT`]).
//! One slow remote held the hook past the product's timeout, and the product dropped what the
//! hook said. Now the hook starts `bay live poll` detached and waits for it at most [`JOIN`]: a
//! poll that ends in time is told on this hook, as before; a later one is told at a later hook
//! (G147). One poll runs at a time in a clone, so a slow remote never piles them up.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::repo::Repo;
use crate::result::{BayResult, GIT_NETWORK_TIMEOUT};

/// How long a hook waits for its poll to end.
pub const JOIN: Duration = Duration::from_secs(3);

/// How often a hook looks whether its poll ended.
const STEP: Duration = Duration::from_millis(25);

use crate::version::TEST_HOOKS;

/// How long a hook waits for its poll: [`JOIN`], or in a test build `BAY_TEST_POLL_JOIN_S`, else
/// the hook's whole budget, so a test reads what the poll brought on the hook that ran it.
fn join() -> Duration {
    if !TEST_HOOKS {
        return JOIN;
    }
    std::env::var("BAY_TEST_POLL_JOIN_S")
        .ok()
        .and_then(|text| text.parse().ok())
        .map_or(Duration::MAX, Duration::from_secs)
}

/// The lock of the one poll a clone runs, and the age after which a lock is stale: a poll makes
/// at most four calls to the remote, each with its own bound.
fn lock_of(repo: &Repo) -> (PathBuf, Duration) {
    let stale = GIT_NETWORK_TIMEOUT * 4 + Duration::from_secs(60);
    (repo.common_dir.join("bay").join("hook-poll.lock"), stale)
}

/// Whether the fleet's presence record is older than the presence window (G427), at the two
/// moments that tell whether a runner runs (`notice::no_executor`).
fn fleet_due(repo: &Repo, event: &str) -> bool {
    let window = repo.config.takeover().as_secs();
    matches!(event, "session.start" | "turn.start")
        && crate::fleet::fetched_age_s(repo).is_none_or(|age| age > window)
}

/// `bay hook <event>` in `dir`: starts the poll that is due, and waits for it at most [`join`]
/// within the hook's deadline. `questions` are the session's question talks to close as `ended`
/// (dossier 63). With nothing due, nothing starts.
pub fn start(dir: &Path, event: &str, questions: &[String]) {
    let Ok(repo) = Repo::open(dir) else {
        return;
    };
    let mut args = vec!["--json".to_owned(), "live".to_owned(), "poll".to_owned()];
    let (lock, stale) = lock_of(&repo);
    if !crate::live::lock_is_held(&lock, stale) {
        let now = crate::live::unix_now();
        if matches!(event, "turn.start" | "step.end") && crate::talk::poll::due(&repo, now) {
            args.push("--talks".to_owned());
        }
        if fleet_due(&repo, event) {
            args.push("--fleet".to_owned());
        }
    }
    for talk in questions {
        args.extend(["--end-question".to_owned(), talk.clone()]);
    }
    if args.len() == 3 {
        return;
    }
    let Some(bay) = crate::update::own_binary() else {
        return;
    };
    let mut cmd = Command::new(bay);
    // G653: the poll accepts the talks of this session's actor, whatever names it here.
    if crate::change::actor_source() == crate::change::ActorSource::Env {
        cmd.env("BAY_ACTOR", crate::change::actor_name());
    }
    cmd.arg("-C")
        .arg(dir)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::platform::detach(&mut cmd);
    let Ok(mut child) = cmd.spawn() else {
        return;
    };
    let wait = join().min(crate::agent_hook::BUDGET);
    let until = crate::deadline::bounded(wait).map(|wait| Instant::now() + wait);
    while until.is_some_and(|until| Instant::now() < until) {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(STEP);
    }
    crate::platform::reap(child);
}

/// `bay live poll`: the poll a hook started. `talks` accepts this actor's talks (G54), `fleet`
/// fetches the fleet's presence (G427) and `questions` are closed as `ended` (dossier 63). The
/// talks and the fleet wait for no other poll: while one runs, this one leaves them to it.
#[must_use]
pub fn run(dir: &Path, talks: bool, fleet: bool, questions: &[String]) -> BayResult {
    let repo = match Repo::open(dir) {
        Ok(repo) => repo,
        Err(why) => return BayResult::failed(why),
    };
    if !questions.is_empty() {
        crate::talk::end_questions(&repo, questions);
    }
    if !talks && !fleet {
        return BayResult::ok(json!({"polled": false, "ended": questions}));
    }
    let (lock, stale) = lock_of(&repo);
    let _ = std::fs::create_dir_all(repo.common_dir.join("bay"));
    if !crate::live::acquire(&lock, stale) {
        return BayResult::ok(
            json!({"polled": false, "ended": questions, "why": "another poll is running"}),
        );
    }
    let accepted = talks
        .then(|| crate::talk::poll::live(&repo, crate::live::unix_now()))
        .flatten();
    let fetched = fleet && fleet_due(&repo, "turn.start");
    if fetched {
        crate::fleet::fetch(&repo);
    }
    let _ = std::fs::remove_file(&lock);
    BayResult::ok(
        json!({"polled": true, "accepted": accepted, "fleet_fetched": fetched,
        "ended": questions}),
    )
}
