//! The notify adapter (`INTERFACES` § 4, `DESIGN` § 9; ruling R6): `[notify] command` in
//! `.bay/config.toml`, called once after each event is written, with the event's JSON on
//! stdin. Exit 0 means delivered. The event carries its id so the adapter can deduplicate;
//! bay cannot make another system exactly-once.
//!
//! A notice is a convenience, never the record: the event is already on the executor's log
//! when the adapter runs, its answer is never written back, and a failed or slow notice is
//! never retried. `bay watch` reads what a lost notice would have said (G9).

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::repo::Repo;

/// How long one notice may take before the adapter is killed and the notice counts as lost.
pub const NOTIFY_TIMEOUT: Duration = Duration::from_secs(5);

/// What became of one notice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// No `[notify] command` is configured.
    Unconfigured,
    /// The adapter exited 0.
    Delivered,
    /// The adapter could not start, exited non-zero, or ran past [`NOTIFY_TIMEOUT`].
    Lost,
}

/// Calls the configured notify adapter once with `event` on stdin. The command is a program
/// path relative to the main work tree, then its arguments, as `[executor] command` is.
#[must_use]
pub fn notify(repo: &Repo, event: &Value) -> Delivery {
    let Some(cmd) = repo
        .config
        .notify
        .as_ref()
        .and_then(|notify| notify.command.as_deref())
        .and_then(|command| command_at(&repo.work_root, command))
    else {
        return Delivery::Unconfigured;
    };
    send(cmd, notice(event).as_bytes(), NOTIFY_TIMEOUT).delivery(NOTIFY_TIMEOUT)
}

/// The bytes one notice of `event` puts on the adapter's stdin: its JSON and a newline.
#[must_use]
pub fn notice(event: &Value) -> String {
    let mut text = event.to_string();
    text.push('\n');
    text
}

/// `[notify] command` as a process: a program path relative to `work_root`, then its
/// arguments, run from `work_root` with only `PATH` and `HOME` from bay's environment, its
/// output going nowhere. `None` for an empty command.
#[must_use]
pub fn command_at(work_root: &Path, command: &str) -> Option<Command> {
    let mut words = command.split_whitespace();
    let program = words.next()?;
    // A `#!` script bay cannot start on Windows starts as it is, and so is lost as an adapter
    // that cannot start is (`crate::platform::script`).
    let program = work_root.join(program);
    let mut cmd = crate::platform::script(&program).unwrap_or_else(|_| Command::new(&program));
    cmd.args(words)
        .current_dir(work_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // The executor's environment (an agent socket, key paths, `GIT_*`) is not the adapter's:
    // it gets only `PATH` and `HOME`.
    cmd.env_clear();
    for name in ["PATH", "HOME"] {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    Some(cmd)
}

/// How one notice ended.
#[derive(Clone, Copy, Debug)]
pub struct Sent {
    /// The adapter's exit status; `None` when it did not start or ran past the bound.
    pub status: Option<ExitStatus>,
    pub elapsed: Duration,
}

impl Sent {
    /// What bay records for this notice when its bound is `limit`: delivered only on an exit
    /// of 0 within it.
    #[must_use]
    pub fn delivery(&self, limit: Duration) -> Delivery {
        if self.status.is_some_and(|status| status.success()) && self.elapsed <= limit {
            Delivery::Delivered
        } else {
            Delivery::Lost
        }
    }
}

/// Runs `cmd` with `input` on stdin, in a process group of its own, and waits at most
/// `timeout` on the monotonic clock; past it the whole group is killed, so no child of the
/// adapter outlives the bound. Its output goes nowhere, so a child it leaves behind after
/// exiting holds no pipe of ours open.
#[must_use]
pub fn send(mut cmd: Command, input: &[u8], timeout: Duration) -> Sent {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    let started = Instant::now();
    let Ok(mut child) = cmd.spawn() else {
        return Sent {
            status: None,
            elapsed: started.elapsed(),
        };
    };
    let group = child.id();
    if let Some(mut stdin) = child.stdin.take() {
        // An adapter that exits without reading closes the pipe; its exit status decides.
        let input = input.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Sent {
                    status: Some(status),
                    elapsed: started.elapsed(),
                };
            }
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                crate::inputs::kill_group(group);
                let _ = child.kill();
                let _ = child.wait();
                return Sent {
                    status: None,
                    elapsed: started.elapsed(),
                };
            }
        }
    }
}

/// [`send`], delivered or not, as the unit tests read it.
#[cfg(test)]
fn deliver(cmd: Command, input: &[u8], timeout: Duration) -> bool {
    send(cmd, input, timeout).delivery(timeout) == Delivery::Delivered
}

#[cfg(test)]
mod tests {
    use super::deliver;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    }

    #[test]
    fn exit_zero_is_delivered_and_anything_else_is_lost() {
        assert!(deliver(
            sh("cat >/dev/null"),
            b"{}\n",
            Duration::from_secs(5)
        ));
        assert!(!deliver(
            sh("cat >/dev/null; exit 1"),
            b"{}\n",
            Duration::from_secs(5)
        ));
        assert!(!deliver(
            Command::new("/nonexistent/notify"),
            b"{}\n",
            Duration::from_secs(5)
        ));
    }

    #[test]
    fn a_slow_adapter_is_killed_at_its_timeout() {
        let started = Instant::now();
        assert!(!deliver(
            sh("sleep 30"),
            b"{}\n",
            Duration::from_millis(300)
        ));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_child_of_a_slow_adapter_dies_with_it() {
        let dir = std::env::temp_dir().join(format!("bay-notify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("child.pid");
        let script = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
        assert!(!deliver(sh(&script), b"{}\n", Duration::from_millis(500)));
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let alive = || {
            Command::new("kill")
                .args(["-0", pid.trim()])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!alive(), "the adapter's child {pid} outlived the bound");
    }
}
