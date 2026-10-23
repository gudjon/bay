//! N-8: a trunk hold. A team sometimes must hold its trunk (an incident, an infrastructure
//! migration, a lock outside the repository). While trunk is held the executor publishes only
//! a landing that carries `Fixes:` or `Reverts:`, the same rule a `[[hold]]` of a promotion
//! target keeps (`crate::target`); every other operation waits with `MAIN_HELD`, publishes
//! nothing and holds no queue place, so a fix behind it reaches the head (`crate::publish`).
//!
//! A hold comes from one of two places, both read from the config at the trunk's tip, so a
//! change never lifts a hold for itself:
//!
//! - a `[[hold]]` whose `target` is the trunk's own name, in force until its `end`;
//! - the hold adapter, `[plugin.hold] command` in `.bay/config.toml`: a program path relative to the
//!   work tree, then its arguments, as `[plugin.notify] command` is. Its program is materialized from
//!   the trunk's tip, never taken from this machine's work tree, and run as `<command> status`
//!   with only `PATH` and `HOME` of bay's environment, within `[plugin.hold] timeout_s` (10 s unless
//!   set). It answers `{"held": true|false, "reason": "..."}` on stdout and exits 0.
//!
//! Unknown is never clear: a config at the trunk's tip bay cannot read, an adapter that does not
//! start, exits non-zero, runs past its bound or prints anything but that answer each reads as
//! held, with a reason that names the fault.

use std::process::ExitStatus;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::Config;
use crate::repo::Repo;

/// How long the hold adapter may take when `[plugin.hold] timeout_s` is not set.
pub const DEFAULT_TIMEOUT_S: u64 = 10;

/// The argument the hold adapter is asked with.
pub const STATUS: &str = "status";

/// The release reason of a queue place given back because trunk is held. It counts toward
/// neither `QUEUE_DEADLINE_EXCEEDED` nor `MERGE_RETRY_EXHAUSTED` (`DESIGN` § 6).
pub const RELEASE: &str = "trunk_held";

/// A hold in force on trunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hold {
    /// `freeze` (a `[[hold]]` of the trunk), `adapter` (`[plugin.hold] command`), or `config` (the
    /// config at the trunk's tip could not be read).
    pub source: &'static str,
    pub reason: String,
    /// When it ends: a freeze's `end`, or the adapter's `end` when it names one.
    pub end: Option<String>,
}

impl Hold {
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({"source": self.source, "reason": self.reason, "end": self.end})
    }
}

/// Whether a landing with these trailers passes a hold: it carries `Fixes:` or `Reverts:`.
#[must_use]
pub fn passes(fixes: Option<&String>, reverts: Option<&String>) -> bool {
    [fixes, reverts]
        .into_iter()
        .flatten()
        .any(|named| !named.trim().is_empty())
}

/// The hold in force on trunk at `trunk` (a commit) now, or `None` when trunk is clear. A
/// freeze is read first: when one is in force the adapter is not asked.
#[must_use]
pub fn held(repo: &Repo, trunk: &str) -> Option<Hold> {
    let config = match Config::at(&repo.git, trunk) {
        Ok(config) => config,
        Err(why) => {
            return Some(Hold {
                source: "config",
                reason: format!(
                    "the {} on main ({trunk}) cannot be read, so whether main is held is unknown: {why}",
                    crate::config::CONFIG_PATH
                ),
                end: None,
            });
        }
    };
    if let Some(hold) = frozen(&config, repo.trunk(), crate::live::unix_now()) {
        return Some(hold);
    }
    let adapter = config.plugin.hold.as_ref()?;
    let command = adapter.command.as_deref()?;
    let timeout = adapter.timeout(Duration::from_secs(DEFAULT_TIMEOUT_S));
    match ask(repo, trunk, command, timeout) {
        Ok(answer) => answer,
        Err(fault) => Some(Hold {
            source: "plugin",
            reason: format!("the hold plugin {command:?} gave no answer bay can read: {fault}"),
            end: None,
        }),
    }
}

/// The `[[hold]]` of main, `trunk_name`, in force at `now`, the one that ends last.
#[must_use]
pub fn frozen(config: &Config, trunk_name: &str, now: u64) -> Option<Hold> {
    config
        .freezes
        .iter()
        .filter(|freeze| freeze.target == trunk_name && freeze.active(now))
        .max_by_key(|freeze| crate::clock::unix_of(&freeze.end).unwrap_or(u64::MAX))
        .map(|freeze| Hold {
            source: "window",
            reason: freeze.reason.clone(),
            end: Some(freeze.end.clone()),
        })
}

/// Asks the hold adapter `command` once: its program materialized from `trunk` into a scratch
/// of its own, run as `<command> status` within `timeout`.
///
/// # Errors
///
/// What kept the answer from being read: the program is not at the trunk's tip, it did not
/// start, it ran past `timeout`, it exited non-zero, or it printed no answer.
fn ask(repo: &Repo, trunk: &str, command: &str, timeout: Duration) -> Result<Option<Hold>, String> {
    let (status, stdout, stderr) = run_from_trunk(
        repo,
        trunk,
        "[plugin.hold] command",
        command,
        &[STATUS],
        timeout,
        "hold-run",
    )?;
    if !status.success() {
        return Err(format!(
            "it exited with {status}: {}",
            String::from_utf8_lossy(&stderr).trim()
        ));
    }
    read_answer(&stdout)
}

/// Runs the adapter `command` (a program path relative to the work tree, then its arguments)
/// once, with `args` after its own: its program materialized from the commit `trunk` into a
/// scratch of its own under `<common dir>/bay/<runs>/`, never taken from this machine's work
/// tree, and run there with only `PATH` and `HOME` of bay's environment, materializing and
/// running together within `timeout`. The scratch is removed once it ends. `what` names the
/// setting in an error (`[plugin.hold] command`). The hold adapter and the prune adapter
/// (`crate::heads`) both run this way.
///
/// # Errors
///
/// The program is not at `trunk`, it could not be materialized or did not start, or it ran
/// past `timeout`. An exit status, zero or not, is the caller's to read.
pub(crate) fn run_from_trunk(
    repo: &Repo,
    trunk: &str,
    what: &str,
    command: &str,
    args: &[&str],
    timeout: Duration,
    runs: &str,
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), String> {
    let program = command
        .split_whitespace()
        .next()
        .ok_or_else(|| format!("{what} is empty"))?;
    let scratch = repo.common_dir.join("bay").join(runs).join(format!(
        "run-{}-{}",
        std::process::id(),
        crate::ids::new_change_id().map_err(|error| error.to_string())?
    ));
    let ran = run_in(repo, trunk, what, command, program, args, &scratch, timeout);
    let _ = std::fs::remove_dir_all(&scratch);
    ran
}

#[allow(clippy::too_many_arguments)]
fn run_in(
    repo: &Repo,
    trunk: &str,
    what: &str,
    command: &str,
    program: &str,
    args: &[&str],
    scratch: &std::path::Path,
    timeout: Duration,
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), String> {
    let deadline = Instant::now() + timeout;
    let paths = [program.to_owned()];
    crate::materialize::materialize(
        &repo.git,
        trunk,
        Some(&paths[..]),
        scratch,
        deadline,
        &mut || {},
    )
    .map_err(|unmade| match unmade {
        crate::materialize::Unmade::TimedOut => {
            format!(
                "materializing {program} from main took more than {}s",
                timeout.as_secs()
            )
        }
        crate::materialize::Unmade::Refused(why) | crate::materialize::Unmade::Failed(why) => why,
        crate::materialize::Unmade::DiskLow(low) => low.message(),
    })?;
    if !scratch.join(program).is_file() {
        return Err(format!("{program} is not in main ({trunk})"));
    }
    let mut cmd =
        crate::run::command(scratch, command).ok_or_else(|| format!("{what} is empty"))??;
    cmd.args(args);
    let left = deadline.saturating_duration_since(Instant::now());
    let ran = crate::run::captured(cmd, None, left.max(Duration::from_millis(1)))
        .map_err(|fault| fault.to_string())?;
    Ok((ran.status, ran.stdout, ran.stderr))
}

/// The adapter's answer on stdout: `{"held": true|false, "reason": "..."}`, and an `end` it
/// may name.
///
/// # Errors
///
/// The output is not JSON, or `held` is not a boolean.
pub fn read_answer(stdout: &[u8]) -> Result<Option<Hold>, String> {
    let text = String::from_utf8_lossy(stdout);
    let answer: Value = serde_json::from_str(text.trim()).map_err(|error| {
        format!(
            "it printed no JSON answer ({error}): {:?}",
            text.chars().take(200).collect::<String>()
        )
    })?;
    let held = answer["held"]
        .as_bool()
        .ok_or_else(|| format!("its answer has no boolean \"held\": {answer}"))?;
    if !held {
        return Ok(None);
    }
    let reason = answer["reason"]
        .as_str()
        .filter(|reason| !reason.trim().is_empty())
        .unwrap_or("the hold plugin answers held and names no reason")
        .to_owned();
    Ok(Some(Hold {
        source: "plugin",
        reason,
        end: answer["end"].as_str().map(str::to_owned),
    }))
}

#[cfg(test)]
mod tests {
    use super::{frozen, passes, read_answer};
    use crate::config::Config;

    #[test]
    fn an_answer_is_held_clear_or_a_fault() {
        assert_eq!(read_answer(br#"{"held": false}"#), Ok(None));
        let held = read_answer(br#"{"held": true, "reason": "incident 12"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(held.source, "plugin");
        assert_eq!(held.reason, "incident 12");
        assert!(read_answer(b"not json").is_err());
        assert!(read_answer(br#"{"held": "no"}"#).is_err());
        assert!(read_answer(b"").is_err());
    }

    #[test]
    fn a_freeze_of_the_trunk_holds_it_until_its_end() {
        let config = Config::parse(
            "main = \"main\"\n[[hold]]\ntarget = \"main\"\nend = \"2026-12-24T00:00:00Z\"\nreason = \"migration\"\n",
        )
        .unwrap();
        let end = crate::clock::unix_of("2026-12-24T00:00:00Z").unwrap();
        let hold = frozen(&config, "main", end - 1).unwrap();
        assert_eq!(hold.source, "window");
        assert_eq!(hold.reason, "migration");
        assert_eq!(hold.end.as_deref(), Some("2026-12-24T00:00:00Z"));
        assert!(frozen(&config, "main", end).is_none());
    }

    #[test]
    fn only_a_fix_or_a_revert_passes() {
        let named = "c7k2m4".to_owned();
        assert!(passes(Some(&named), None));
        assert!(passes(None, Some(&named)));
        assert!(!passes(None, None));
    }
}
