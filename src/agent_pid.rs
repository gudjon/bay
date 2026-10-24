//! The session of the agent that runs a bay command (BACKLOG 66 C, owner ruling 2026-10-24).
//!
//! Only Claude Code hands an agent's shell its session (`BAY_SESSION`, ruling R2). Under any
//! other product, an agent's own `bay save` knew no session, so its own hook's lock refused it
//! `PATH_HELD` until the turn ended. Both run under the agent's process: a product starts its
//! hooks and its shell commands through a shell, from its own process. So each hook names its
//! session against the agent's process, the nearest ancestor that is no shell, by its pid and
//! start time (a pid is reused; the pair is not), in `<git-common-dir>/bay/agents/<pid>`. A bay
//! command with no `BAY_SESSION` walks its own ancestors to the first one so named whose start
//! time still matches, and acts as that session. A person's terminal is no agent's process,
//! so it finds none, and acts as before. Unix only: elsewhere nothing is named or found.

use std::path::{Path, PathBuf};

/// The programs a product starts its hooks and tools through, never the agent itself.
const SHELLS: &[&str] = &["sh", "bash", "dash", "zsh", "ksh", "fish", "ash", "busybox"];

/// How far up a command looks for an agent's process.
const MAX_DEPTH: usize = 16;

/// A process: its parent's pid, its start time as the system reports it, and its name.
struct Process {
    parent: u32,
    start: String,
    name: String,
}

/// The folder of the agents' names in the repository whose git common directory is `common`.
fn dir(common: &Path) -> PathBuf {
    common.join("bay").join("agents")
}

/// Names `session` against the agent's process of this hook, in the repository whose git
/// common directory is `common`. A hook with no agent's process above it names nothing. Best
/// effort: a name that cannot be written leaves the command as it would be without one.
pub fn remember(common: &Path, session: &str) {
    let Some((pid, agent)) = agent_of(std::process::id()) else {
        return;
    };
    let path = dir(common).join(pid.to_string());
    let text = format!("{}\n{session}\n", agent.start);
    if std::fs::read_to_string(&path).is_ok_and(|now| now == text) {
        return;
    }
    if std::fs::create_dir_all(dir(common)).is_err() {
        return;
    }
    // Written beside its place, then renamed: a reader never sees half a name.
    let part = path.with_extension(format!("part-{}", std::process::id()));
    if std::fs::write(&part, &text).is_ok() && crate::durable::rename(&part, &path).is_err() {
        let _ = std::fs::remove_file(&part);
    }
}

/// The session a hook named against one of this process's ancestors, the nearest first, in
/// the repository whose git common directory is `common`; `None` when none is named, or its
/// process has ended and its pid was reused.
#[must_use]
pub fn session_of_caller(common: &Path) -> Option<String> {
    let mut pid = std::process::id();
    for _ in 0..MAX_DEPTH {
        let process = process(pid)?;
        if pid != std::process::id()
            && let Ok(text) = std::fs::read_to_string(dir(common).join(pid.to_string()))
        {
            let mut lines = text.lines();
            if lines.next() == Some(process.start.as_str())
                && let Some(session) = lines.next()
                && crate::session::SessionId::parse(session).is_some()
            {
                return Some(session.to_owned());
            }
        }
        if process.parent <= 1 {
            return None;
        }
        pid = process.parent;
    }
    None
}

/// The agent's process of `pid`: its nearest ancestor that is no shell, and that process.
fn agent_of(pid: u32) -> Option<(u32, Process)> {
    let mut pid = process(pid)?.parent;
    for _ in 0..MAX_DEPTH {
        if pid <= 1 {
            return None;
        }
        let process = process(pid)?;
        if !SHELLS.contains(&process.name.as_str()) {
            return Some((pid, process));
        }
        pid = process.parent;
    }
    None
}

#[cfg(target_os = "linux")]
fn process(pid: u32) -> Option<Process> {
    // `<pid> (<name>) <state> <ppid> ... <starttime> ...`: the name may hold spaces and
    // parentheses, so the fields are read after its last `)`.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let name = stat.get(open + 1..close)?.to_owned();
    let fields: Vec<&str> = stat.get(close + 1..)?.split_whitespace().collect();
    Some(Process {
        parent: fields.get(1)?.parse().ok()?,
        start: (*fields.get(19)?).to_owned(),
        name,
    })
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process(pid: u32) -> Option<Process> {
    let mut ps = std::process::Command::new("ps");
    ps.args(["-o", "ppid=,lstart=,ucomm=", "-p", &pid.to_string()]);
    let output = crate::git::run_bounded(ps, "ps").ok()?;
    if !output.status.success() {
        return None;
    }
    // `<ppid> <weekday> <month> <day> <time> <year> <name>`.
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let words: Vec<&str> = text.split_whitespace().collect();
    Some(Process {
        parent: words.first()?.parse().ok()?,
        start: words.get(1..6)?.join(" "),
        name: words.get(6..)?.join(" "),
    })
}

#[cfg(not(unix))]
fn process(_pid: u32) -> Option<Process> {
    None
}
