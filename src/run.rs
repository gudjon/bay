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

/// S5: the kernel's `USER_HZ`, the unit of the CPU times in `/proc/<pid>/stat`: 100 a second on
/// every architecture bay is built for.
const USER_HZ: f64 = 100.0;

thread_local! {
    /// S5: the CPU seconds of every process [`bounded_until`] ran on this thread, summed.
    static CHILD_CPU: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
}

/// S5 (plan row S5, "CPU per landing"): the CPU seconds, user and system, of the processes
/// [`bounded_until`] ran on this thread so far. A caller reads it before and after a run on the
/// thread that runs it, and the difference is that run's. `None` off Linux.
#[must_use]
pub fn child_cpu() -> Option<f64> {
    cfg!(target_os = "linux").then(|| CHILD_CPU.with(std::cell::Cell::get))
}

/// S5: adds `cpu` seconds to this thread's [`child_cpu`].
fn spent(cpu: Option<f64>) {
    if let Some(cpu) = cpu {
        CHILD_CPU.with(|sum| sum.set(sum.get() + cpu));
    }
}

/// S5: process `pid`'s state (field 3) and the CPU seconds, user and system, that it and the
/// children it waited for used (fields 14 to 17), as Linux's `/proc/<pid>/stat` counts them. A
/// child's time joins its parent's when the parent reaps it, often just before the parent
/// exits; an exited process stays a zombie (`Z`) until it is reaped, and its entry then holds
/// the whole run. `None` off Linux or when the entry cannot be read.
fn proc_stat(pid: u32) -> Option<(char, f64)> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command's name (field 2) may hold spaces and parentheses; the fields after its last
    // `)` start at field 3.
    let fields: Vec<&str> = stat
        .get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .collect();
    let state = fields.first()?.chars().next()?;
    let mut ticks = 0_u32;
    for field in fields.get(11..15)? {
        ticks = ticks.checked_add(field.parse::<u32>().ok()?)?;
    }
    Some((state, f64::from(ticks) / USER_HZ))
}

/// S5: how often [`bounded_until`] reads the CPU of a running process's whole tree.
const TREE_EVERY: Duration = Duration::from_millis(100);

/// S5: the CPU seconds of process `pid` and of every descendant it has now, each read with
/// [`proc_stat`] (its own time and its reaped children's), the children from
/// `/proc/<pid>/task/<tid>/children`. A parent is read before its children, so a child it reaps
/// in between is missed for this read, never counted twice. A process can leave the tree
/// unreaped: `bwrap --unshare-pid` exits on the sandbox's exit code without waiting for the
/// sandbox's init, whose time then never reaches it. So the caller keeps the largest sum it
/// read. `None` when `pid`'s entry cannot be read.
fn tree_cpu(pid: u32) -> Option<f64> {
    let (_, mut total) = proc_stat(pid)?;
    let mut next = children(pid);
    let mut visits = 0_u32;
    while let Some(child) = next.pop() {
        visits += 1;
        if visits > 4096 {
            break;
        }
        if let Some((_, used)) = proc_stat(child) {
            total += used;
            next.extend(children(child));
        }
    }
    Some(total)
}

/// S5: the children of process `pid`, from each of its threads' `children` file.
fn children(pid: u32) -> Vec<u32> {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    tasks
        .flatten()
        .filter_map(|task| std::fs::read_to_string(task.path().join("children")).ok())
        .flat_map(|list| {
            list.split_whitespace()
                .filter_map(|child| child.parse::<u32>().ok())
                .collect::<Vec<_>>()
        })
        .collect()
}

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
        let _ = crate::check::remove_tree(&self.0);
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
    /// G796: its caller stopped it ([`bounded_until`]); its whole group was killed.
    Stopped,
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "it did not start: {error}"),
            Self::Wait(error) => write!(f, "{error}"),
            Self::Timeout(bound) => write!(f, "it timed out after {}s", bound.as_secs()),
            Self::LeftOpen => f.write_str("a child it left behind held its output open"),
            Self::Stopped => f.write_str("it was stopped"),
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
    cmd: Command,
    input: Option<&[u8]>,
    timeout: Duration,
    streams: Streams,
    beat: &mut dyn FnMut(),
    heartbeat: Option<Duration>,
) -> Result<Ran, Fault> {
    bounded_until(cmd, input, timeout, streams, (beat, heartbeat), &|| false)
}

/// [`bounded`], and stopped once `stop` says so: asked as often as the process is, its whole
/// group is then killed (G796: a check whose candidate main moved past).
///
/// # Errors
///
/// As [`bounded`], and [`Fault::Stopped`].
pub fn bounded_until(
    mut cmd: Command,
    input: Option<&[u8]>,
    timeout: Duration,
    streams: Streams,
    (beat, heartbeat): (&mut dyn FnMut(), Option<Duration>),
    stop: &dyn Fn() -> bool,
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
    // S5: its CPU time, the largest of its tree's sums read (`tree_cpu`). Where it can be
    // read, the process is waited for only once it reads as exited, and its tree is read then,
    // so the children it reaped at its end count.
    let (mut cpu, mut walked) = (None, None::<Instant>);
    let status = loop {
        let stat = proc_stat(group);
        let exited = stat.is_none_or(|(state, _)| matches!(state, 'Z' | 'X'));
        if stat.is_some() && (exited || walked.is_none_or(|at| at.elapsed() >= TREE_EVERY)) {
            walked = Some(Instant::now());
            cpu = match (tree_cpu(group), cpu) {
                (Some(read), Some(most)) => Some(f64::max(read, most)),
                (read, most) => read.or(most),
            };
        }
        if exited {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    kill(group, &mut child);
                    spent(cpu);
                    return Err(Fault::Wait(error));
                }
            }
        }
        if heartbeat.is_some_and(|every| last_beat.elapsed() >= every) {
            beat();
            last_beat = Instant::now();
        }
        if started.elapsed() >= timeout {
            kill(group, &mut child);
            spent(cpu);
            return Err(Fault::Timeout(timeout));
        }
        if stop() {
            kill(group, &mut child);
            spent(cpu);
            return Err(Fault::Stopped);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    spent(cpu);
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

    /// S5: a run's CPU time counts on the thread that ran it, its children's with it.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_run_counts_the_cpu_its_processes_used_on_its_thread() {
        let before = super::child_cpu().unwrap();
        // GIVEN an adapter whose child spins and is reaped as the adapter exits, then one that
        // sleeps
        let busy = "( i=0; while [ $i -lt 300000 ]; do i=$((i+1)); done ); exit 0";
        let ran = captured(sh(busy), None, Duration::from_secs(60));
        assert!(ran.is_ok(), "{ran:?}");
        let spun = super::child_cpu().unwrap() - before;
        let ran = captured(sh("sleep 0.3"), None, Duration::from_secs(10));
        assert!(ran.is_ok(), "{ran:?}");
        let slept = super::child_cpu().unwrap() - before - spun;
        // THEN the spin's CPU counts, and the sleep's next to none
        assert!(spun >= 0.1, "{spun}");
        assert!(slept < 0.1, "{slept}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_run_counts_the_cpu_of_a_child_its_adapter_left_unreaped() {
        let before = super::child_cpu().unwrap();
        // GIVEN an adapter that exits while the child that spun still lives, never reaped by
        // it, as `bwrap --unshare-pid` leaves the sandbox's init
        let busy = "( i=0; while [ $i -lt 300000 ]; do i=$((i+1)); done; sleep 0.5 ) \
                    >/dev/null 2>&1 & sleep 1";
        let ran = captured(sh(busy), None, Duration::from_secs(60));
        assert!(ran.is_ok(), "{ran:?}");
        // THEN the child's CPU counts
        let spun = super::child_cpu().unwrap() - before;
        assert!(spun >= 0.1, "{spun}");
    }
}
