//! G619 (a fleet finding, 2026-11-01: at load 100 to 155 a sweep of a hosted remote took about
//! 34 s, and every `bay status` waited for it): a status read starts its sweep as a process of its
//! own (`bay sweep`, hidden) and waits for it only until its deadline. Past it, the read answers
//! from the store as the last sweep left it and names how old that is (`stale_read`); the sweep
//! runs on to its end, settles the store and records itself, so the reads after it are fresh.
//!
//! Why a process of its own (measured on a loaded server, 2026-11-01): a caller waits for the exit
//! of the process it started, not for its output. A process that closed its stdout and stderr and
//! then worked 2 s more held `x=$(...)` 2.007 s, a pipe 2.025 s and Python's `subprocess.run` 2.24
//! s; a detached child with no stdio held `$(...)` 0.014 s after its parent exited. A sweep cut at
//! the deadline would never end at that load, and the store would only grow older.
//!
//! The sweep holds the store's sweep lock ([`crate::sweep::lock`]) for its whole life, so a store
//! runs one at a time, and it ends by the git calls' own timeouts. A read that finds a sweep
//! running waits for that one ([`wait_ended`]) and starts none.

use std::io::BufRead as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// What the sweep a status read started answered (`bay sweep`, [`crate::status::sweep_child`]).
pub enum Answer {
    /// It swept the remote at `at` and settled the store; the changes the read lists (`ids`: the
    /// ones live before the sweep settled them, so a change it saw end is listed once more);
    /// `targets` when the remote answered the read of the promotion targets; what accepting this
    /// actor's talks did.
    Done {
        at: String,
        ids: Vec<String>,
        targets: bool,
        talk_accept: Option<Value>,
    },
    /// It moved the store's trunk at `at` to a tip whose config the store cannot open: the read
    /// opens the store again and answers with what that says.
    Moved(String),
    /// A sweep that ended before it took the lock left its window open.
    Open,
    /// Another sweep of the store runs: what it wrote in the lock.
    Busy(Value),
    /// No answer by the deadline: the sweep runs on as `pid`.
    Late(u32),
    /// It ended with no answer.
    Lost,
    /// A sweep cannot serve this read (offline, or a store a sweep cannot settle).
    Unserved,
    /// The read cannot be answered: what it names is not here or on the remote.
    Failed(String),
}

/// Starts `bay sweep` on the repository at `dir` and leaves it running, for a read served from
/// the store past its sweep's window (G710, G837): what the read says of the sweep.
#[must_use]
pub fn leave_running(dir: &Path) -> Value {
    match start(dir, None, Some(Instant::now())) {
        Answer::Late(pid) => json!({"state": "started", "by": {"pid": pid}}),
        Answer::Busy(running) => json!({"state": "running", "by": running}),
        _ => json!({"state": "started"}),
    }
}

/// Starts `bay sweep [change]` on the repository at `dir` and waits for its answer until
/// `until`, or for as long as it takes with `None`.
#[must_use]
pub fn start(dir: &Path, change: Option<&str>, until: Option<Instant>) -> Answer {
    let Ok(bay) = std::env::current_exe() else {
        return Answer::Unserved;
    };
    let mut cmd = Command::new(bay);
    cmd.arg("-C").arg(dir).arg("sweep");
    cmd.args(change);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::git::trace_child(&mut cmd);
    // Its own process group: a signal to the caller's group does not stop it mid-sweep.
    crate::platform::detach(&mut cmd);
    let Ok(mut child) = cmd.spawn() else {
        return Answer::Unserved;
    };
    let pid = child.id();
    let out = child.stdout.take();
    crate::platform::reap(child);
    let Some(out) = out else {
        return Answer::Lost;
    };
    let (send, answer) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::BufReader::new(out).read_line(&mut line);
        let _ = send.send(line);
    });
    let line = match until {
        Some(until) => answer
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .ok(),
        None => answer.recv().ok(),
    };
    line.map_or(Answer::Late(pid), |line| parse(&line))
}

fn parse(line: &str) -> Answer {
    let Ok(answer) = serde_json::from_str::<Value>(line) else {
        return Answer::Lost;
    };
    if let Some(done) = answer.get("done")
        && let Some(at) = done["at"].as_str()
    {
        return Answer::Done {
            at: at.to_owned(),
            ids: done["ids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .collect(),
            targets: done["targets"] == true,
            talk_accept: done
                .get("talk_accept")
                .filter(|accept| !accept.is_null())
                .cloned(),
        };
    }
    if let Some(at) = answer["moved"].as_str() {
        return Answer::Moved(at.to_owned());
    }
    if answer.get("open").is_some() {
        return Answer::Open;
    }
    if let Some(running) = answer.get("busy") {
        return Answer::Busy(running.clone());
    }
    if let Some(message) = answer["failed"].as_str() {
        return Answer::Failed(message.to_owned());
    }
    if answer.get("unserved").is_some() {
        return Answer::Unserved;
    }
    Answer::Lost
}

/// The longest a read with no deadline waits for another process's sweep: past the git calls'
/// own timeouts, so only a sweep that died unseen is waited for this long.
const NO_DEADLINE_WAIT: Duration = Duration::from_secs(300);

/// Waits until the sweep `running` names (what its lock said, [`crate::sweep::running`]) has
/// ended, or until `until`: `true` when it ended. A newer sweep in the lock means it ended.
#[must_use]
pub fn wait_ended(common: &Path, running: &Value, until: Option<Instant>) -> bool {
    let until = until.unwrap_or_else(|| Instant::now() + NO_DEADLINE_WAIT);
    let mut since = running["since"].as_u64();
    loop {
        let now = crate::sweep::running(common);
        match (since, now["since"].as_u64()) {
            (Some(waited), Some(seen)) if seen != waited => return true,
            (Some(_), Some(_)) if now["ended"].is_u64() => return true,
            (None, Some(seen)) => since = Some(seen),
            _ => {}
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
