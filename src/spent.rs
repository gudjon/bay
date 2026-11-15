//! G738 (a measurement, 2026-11-15: where agents wait on bay could be read only from the agents'
//! own transcripts, call by call, and they never said where a command's time went). Each bay
//! command run in a repository appends one line to `bay/spent/<day>.jsonl` under the store's
//! common directory: the command, its decision, its wall time, its time in Git and on the
//! network, and its slowest Git commands. One `write` of one line, so commands that end
//! together never mix their lines. A day's file is removed after [`KEPT_DAYS`] days. Best
//! effort: a store that cannot be written records nothing, and nothing ever reads it to decide.

use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;

/// How many days of lines are kept.
const KEPT_DAYS: u64 = 7;

/// How many of the slowest Git commands a line names.
const SLOWEST: usize = 5;

/// The Git commands that ask the remote.
const NETWORK: [&str; 3] = ["fetch", "push", "ls-remote"];

/// In a test build, `BAY_TEST_SPENT=off` records nothing.
fn off() -> bool {
    crate::version::TEST_HOOKS && std::env::var("BAY_TEST_SPENT").is_ok_and(|value| value == "off")
}

/// Records that `command` run in `dir` ended with `decision`, `began` being when it began.
pub fn record(dir: &Path, command: &str, decision: &str, began: Instant) {
    if off() {
        return;
    }
    let Some(common) = common_of(dir) else {
        return;
    };
    let folder = common.join("bay").join("spent");
    let times = crate::git::process_times();
    let git: Duration = times.iter().map(|(_, spent, _)| *spent).sum();
    let network: Duration = times
        .iter()
        .filter(|(command, _, _)| NETWORK.contains(&command.as_str()))
        .map(|(_, spent, _)| *spent)
        .sum();
    let mut slowest = times.clone();
    slowest.sort_by_key(|(_, spent, _)| std::cmp::Reverse(*spent));
    let slowest: Vec<_> = slowest
        .iter()
        .take(SLOWEST)
        .map(|(command, spent, count)| json!([command, spent.as_millis(), count]))
        .collect();
    let now = crate::clock::now_rfc3339();
    let line = json!({
        "at": now,
        "command": command,
        "decision": decision,
        "ms": began.elapsed().as_millis(),
        "git_ms": git.as_millis(),
        "git_calls": times.iter().map(|(_, _, count)| u64::from(*count)).sum::<u64>(),
        "network_ms": network.as_millis(),
        "slowest": slowest,
    });
    let day = now.get(..10).unwrap_or("unknown");
    let file = folder.join(format!("{day}.jsonl"));
    let fresh = !file.exists();
    if std::fs::create_dir_all(&folder).is_err() {
        return;
    }
    if let Ok(mut out) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
    {
        let _ = out.write_all(format!("{line}\n").as_bytes());
    }
    if fresh {
        prune(&folder);
    }
}

/// The common directory of the store `dir` is in, found with no Git: the nearest `.git`
/// directory, or the `commondir` a linked worktree's `.git` file leads to.
fn common_of(dir: &Path) -> Option<std::path::PathBuf> {
    let dir = dir.canonicalize().ok()?;
    let root = dir.ancestors().find(|root| root.join(".git").exists())?;
    let git = root.join(".git");
    if git.is_dir() {
        return Some(git);
    }
    let link = std::fs::read_to_string(&git).ok()?;
    let own = root.join(link.trim().strip_prefix("gitdir:")?.trim());
    let common = std::fs::read_to_string(own.join("commondir")).ok()?;
    own.join(common.trim()).canonicalize().ok()
}

/// Removes the day files of `folder` older than [`KEPT_DAYS`] days.
fn prune(folder: &Path) {
    let Some(cutoff) =
        std::time::SystemTime::now().checked_sub(Duration::from_secs(KEPT_DAYS * 86_400))
    else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| modified < cutoff);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
