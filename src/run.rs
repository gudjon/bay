//! The one way bay runs an adapter (`INTERFACES` § 3a, § 4, § 7; G291): the notify, wake,
//! runtime, hold and prune adapters, the executor's `submit`, an input provider and a check.
//!
//! - [`command`] builds an adapter named in the config, a program path relative to a work tree
//!   (this machine's, or a copy materialized from trunk), then its arguments, with only `PATH`
//!   and `HOME` of bay's environment.
//! - [`bounded`] runs one in a process group of its own and, at its bound, kills the whole
//!   group, so no child the adapter started outlives it.
//!
//! What a caller sees on top (its own arguments, a sandbox around a check, a check's own
//! environment) it adds to the command; each caller words its own messages from [`Fault`].

use std::io::{Read, Write as _};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// An adapter's command as a process: a program path relative to `work_root`, then its
/// arguments, run from `work_root` with only `PATH` and `HOME` from bay's environment. `None`
/// for an empty command; `Some(Err)` naming why for a `#!` script bay cannot start on Windows
/// (`crate::platform::script`).
#[must_use]
pub fn command(work_root: &Path, command: &str) -> Option<Result<Command, String>> {
    let mut words = command.split_whitespace();
    let program = work_root.join(words.next()?);
    Some(crate::platform::script(&program).map(|mut cmd| {
        cmd.args(words).current_dir(work_root);
        // bay's environment (an agent socket, key paths, `GIT_*`) is not the adapter's: it gets
        // only `PATH` and `HOME`.
        cmd.env_clear();
        for name in ["PATH", "HOME"] {
            if let Some(value) = std::env::var_os(name) {
                cmd.env(name, value);
            }
        }
        cmd
    }))
}

/// An adapter named in the config at trunk, as trunk holds it: its program is materialized
/// from the trunk tip into a scratch directory of its own, removed when this is dropped, and
/// the command runs there (a fleet finding, 2026-10-26: 0 of 102 wakes reached a seat, since
/// every runner looked the connector up in its own checkout, sparse and at an old commit, which
/// lacked it). A program named by an absolute path runs as named, with no copy.
pub struct AtTrunk {
    pub command: Command,
    _scratch: Option<Scratch>,
}

/// A scratch directory, removed when dropped.
/// A scratch directory removed when it goes out of scope, on every path out of its owner: a
/// return, an error, a panic.
pub(crate) struct Scratch(pub(crate) std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// [`AtTrunk`] for `command` of the adapter `what`, as the trunk tip this repository fetched
/// holds it, materialized under `<common-dir>/bay/<runs>/`; `None` for an empty command.
///
/// # Errors
///
/// The program is not at `trunk`, it could not be materialized by `deadline`, or it cannot be
/// started here.
pub fn at_trunk(
    repo: &crate::repo::Repo,
    what: &str,
    command: &str,
    runs: &str,
    deadline: Instant,
) -> Option<Result<AtTrunk, String>> {
    let program = command.split_whitespace().next()?.to_owned();
    let tip = repo
        .git
        .run(&[
            "rev-parse",
            "--verify",
            "-q",
            &format!("{}^{{commit}}", repo.trunk_tracking_ref()),
        ])
        .ok();
    // With no trunk to read (a repository that never fetched one), the work tree's copy runs.
    let Some(trunk) = tip.filter(|_| !Path::new(&program).is_absolute()) else {
        return self::command(&repo.work_root, command).map(|built| {
            built.map(|command| AtTrunk {
                command,
                _scratch: None,
            })
        });
    };
    let id = match crate::ids::new_change_id() {
        Ok(id) => id,
        Err(error) => return Some(Err(error.to_string())),
    };
    let scratch = repo
        .common_dir
        .join("bay")
        .join(runs)
        .join(format!("run-{}-{id}", std::process::id()));
    let guard = Scratch(scratch.clone());
    let paths = [program.clone()];
    if let Err(unmade) = crate::materialize::materialize(
        &repo.git,
        &trunk,
        Some(&paths[..]),
        &scratch,
        deadline,
        &mut || {},
    ) {
        return Some(Err(match unmade {
            crate::materialize::Unmade::TimedOut => {
                format!("materializing {program} from main ran past its bound")
            }
            crate::materialize::Unmade::Refused(why) | crate::materialize::Unmade::Failed(why) => {
                why
            }
            crate::materialize::Unmade::DiskLow(low) => low.message(),
        }));
    }
    if !scratch.join(&program).is_file() {
        return Some(Err(format!(
            "the {what} {program} is not in main ({trunk})"
        )));
    }
    Some(self::command(&scratch, command)?.map(|command| AtTrunk {
        command,
        _scratch: Some(guard),
    }))
}

/// What becomes of an adapter's stdout and stderr.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Streams {
    /// Read and returned.
    Captured,
    /// Sent nowhere, so a child the adapter leaves behind after exiting holds no pipe of ours.
    Discarded,
}

/// How an adapter that ran ended.
#[derive(Debug)]
pub struct Ran {
    pub status: ExitStatus,
    /// Empty when its streams were [`Streams::Discarded`].
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Why an adapter has no exit status to read.
#[derive(Debug)]
pub enum Fault {
    /// It did not start.
    Spawn(std::io::Error),
    /// Waiting on it failed; its group was killed.
    Wait(std::io::Error),
    /// It ran past its bound; its whole group was killed.
    Timeout(Duration),
    /// It exited, and a child it left behind held its output open even after its group was
    /// killed.
    LeftOpen,
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "it did not start: {error}"),
            Self::Wait(error) => write!(f, "{error}"),
            Self::Timeout(bound) => write!(f, "it timed out after {}s", bound.as_secs()),
            Self::LeftOpen => f.write_str("a child it left behind held its output open"),
        }
    }
}

/// [`bounded`] with its output captured and no heartbeat.
///
/// # Errors
///
/// As [`bounded`].
pub fn captured(cmd: Command, input: Option<&[u8]>, timeout: Duration) -> Result<Ran, Fault> {
    bounded(cmd, input, timeout, Streams::Captured, &mut || {}, None)
}

/// Runs `cmd` with `input` on stdin (none: stdin is empty), in a process group of its own, and
/// waits at most `timeout` on the monotonic clock: past it the whole group is killed, so no
/// child of the adapter outlives the bound. `beat` is called every `heartbeat` while it runs.
///
/// A child the adapter leaves running once it exits belongs to the adapter: when its captured
/// output is still open shortly after the exit, the group is killed, and the output is waited
/// for no longer than what is left of the bound (a second at least).
///
/// # Errors
///
/// The adapter did not start, waiting on it failed, it ran past `timeout`, or a child it left
/// behind held its output open ([`Fault`]).
pub fn bounded(
    mut cmd: Command,
    input: Option<&[u8]>,
    timeout: Duration,
    streams: Streams,
    beat: &mut dyn FnMut(),
    heartbeat: Option<Duration>,
) -> Result<Ran, Fault> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    let pipe = || match streams {
        Streams::Captured => Stdio::piped(),
        Streams::Discarded => Stdio::null(),
    };
    cmd.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(pipe())
    .stderr(pipe());
    let started = Instant::now();
    let mut child = cmd.spawn().map_err(Fault::Spawn)?;
    let group = child.id();
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        // An adapter that exits without reading closes the pipe; its exit status decides.
        let input = input.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let (sender, outputs) = mpsc::channel();
    let pipes: [(usize, Option<Box<dyn Read + Send>>); 2] = [
        (0, child.stdout.take().map(|pipe| Box::new(pipe) as _)),
        (1, child.stderr.take().map(|pipe| Box::new(pipe) as _)),
    ];
    let mut readers = 0;
    for (which, pipe) in pipes {
        let Some(mut pipe) = pipe else {
            continue;
        };
        readers += 1;
        let sender = sender.clone();
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer);
            let _ = sender.send((which, buffer));
        });
    }
    drop(sender);
    let mut last_beat = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                kill(group, &mut child);
                return Err(Fault::Wait(error));
            }
        }
        if heartbeat.is_some_and(|every| last_beat.elapsed() >= every) {
            beat();
            last_beat = Instant::now();
        }
        if started.elapsed() >= timeout {
            kill(group, &mut child);
            return Err(Fault::Timeout(timeout));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let [stdout, stderr] = collect(
        &outputs,
        readers,
        group,
        timeout.saturating_sub(started.elapsed()),
    )?;
    Ok(Ran {
        status,
        stdout,
        stderr,
    })
}

/// The `readers` outputs of an adapter that exited, stdout first. One still open shortly after
/// the exit is held by a child the adapter left behind: its group is killed, and the output is
/// waited for no longer than `left` (a second at least).
fn collect(
    outputs: &mpsc::Receiver<(usize, Vec<u8>)>,
    readers: usize,
    group: u32,
    left: Duration,
) -> Result<[Vec<u8>; 2], Fault> {
    let since = Instant::now();
    let mut output = [Vec::new(), Vec::new()];
    let mut received = 0;
    let mut killed = false;
    while received < readers {
        let wait = if killed {
            left.saturating_sub(since.elapsed())
                .max(Duration::from_secs(1))
        } else {
            Duration::from_millis(200)
        };
        match outputs.recv_timeout(wait) {
            Ok((which, buffer)) => {
                output[which] = buffer;
                received += 1;
            }
            Err(mpsc::RecvTimeoutError::Timeout) if !killed => {
                kill_group(group);
                killed = true;
            }
            Err(_) => return Err(Fault::LeftOpen),
        }
    }
    Ok(output)
}

/// Kills `child`'s whole group, then `child` itself, and reaps it.
fn kill(group: u32, child: &mut Child) {
    kill_group(group);
    let _ = child.kill();
    let _ = child.wait();
}

/// Kills process group `group`, with a bounded `kill`. On Windows, where a child leads no
/// group, the process `group` names and every process it started, with a bounded `taskkill`.
fn kill_group(group: u32) {
    let kill = if cfg!(windows) {
        let mut kill = Command::new("taskkill");
        kill.args(["/F", "/T", "/PID", &group.to_string()]);
        kill
    } else {
        let mut kill = Command::new("kill");
        kill.args(["-KILL", "--", &format!("-{group}")]);
        kill
    };
    let _ = crate::git::run_bounded_for(kill, "kill", None, Duration::from_secs(5));
}

#[cfg(all(test, unix))]
mod tests {
    use super::{Fault, Streams, bounded, captured};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }

    #[test]
    fn output_and_status_come_back() {
        let ran = captured(
            sh("cat; echo err >&2; exit 3"),
            Some(&b"in"[..]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(ran.status.code(), Some(3));
        assert_eq!(ran.stdout, b"in");
        assert_eq!(ran.stderr, b"err\n");
    }

    #[test]
    fn a_heartbeat_beats_while_the_adapter_runs() {
        let mut beats = 0;
        let ran = bounded(
            sh("sleep 1"),
            None,
            Duration::from_secs(5),
            Streams::Captured,
            &mut || beats += 1,
            Some(Duration::from_millis(100)),
        );
        assert!(ran.is_ok());
        assert!(beats >= 3, "{beats}");
    }

    #[test]
    fn a_child_of_a_hung_adapter_dies_with_it() {
        let dir = std::env::temp_dir().join(format!("bay-run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("child.pid");
        let script = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
        let started = Instant::now();
        let ran = captured(sh(&script), None, Duration::from_millis(500));
        assert!(matches!(ran, Err(Fault::Timeout(_))), "{ran:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
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
        assert!(!alive(), "the plugin's child {pid} outlived the bound");
    }
}
