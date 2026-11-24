//! The box's check-start load limits (BACKLOG 280; an operator's ask, 2026-11-24: two
//! runners' checks started beside another job on a box at load1 30.5 and CPU some avg10 41 %, and
//! that job's own load watch stopped it). Git config `bay.checkStartLoad1` and
//! `bay.checkStartCpuSome`, global or for one clone, like `bay.minFreeMiB`: while the box's
//! one-minute load average is at or above the first, or the share of the last ten seconds in which
//! some task waited for a CPU (`/proc/pressure/cpu`, `some avg10`, in percent) is at or above the
//! second, a runner's pass claims no operation and starts no post piece. A runner on another box
//! takes the operation, or this one does at its first pass below both limits. Nothing is written
//! while it waits. Unset is no limit: a box sets its own. A box setting, never a repository one.
//!
//! Read on Linux; on any other system the limits are not judged.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::git::Git;
use crate::version::TEST_HOOKS;

/// The one-minute load average at or above which a runner starts no check.
pub const LOAD1_KEY: &str = "bay.checkStartLoad1";

/// The CPU pressure (`some avg10`, percent) at or above which a runner starts no check.
pub const CPU_SOME_KEY: &str = "bay.checkStartCpuSome";

/// What a box whose limits cannot be read, or whose load cannot be measured, does.
pub const RECOVERY: &str =
    "set git config bay.checkStartLoad1 and bay.checkStartCpuSome to numbers, or unset them";

/// How often a runner's journal names one wait for the box's load.
const SAY_EVERY: Duration = Duration::from_secs(60);

/// Whether the wait named `key` (an operation id, or `post`) may write its journal line now: at
/// most once a [`SAY_EVERY`] each, so a busy box's passes do not fill the journal.
pub fn say_now(key: &str) -> bool {
    static SAID: Mutex<BTreeMap<String, Instant>> = Mutex::new(BTreeMap::new());
    let mut said = SAID.lock().unwrap_or_else(PoisonError::into_inner);
    let now = Instant::now();
    said.retain(|_, at| now.duration_since(*at) < SAY_EVERY);
    if said.contains_key(key) {
        return false;
    }
    said.insert(key.to_owned(), now);
    true
}

/// The box's limits; `None` is a limit not set.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Limits {
    pub load1: Option<f64>,
    pub cpu_some: Option<f64>,
}

/// A box at or above a limit: what was measured, against which limits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Busy {
    pub load1: Option<f64>,
    pub cpu_some: Option<f64>,
    pub limits: Limits,
}

impl Busy {
    /// `BOX_BUSY`'s detail.
    #[must_use]
    pub fn detail(&self) -> Value {
        json!({
            "load1": self.load1,
            "cpu_some_avg10": self.cpu_some,
            "limits": {
                LOAD1_KEY: self.limits.load1,
                CPU_SOME_KEY: self.limits.cpu_some,
            },
            "recovery": "a runner on another box takes the operation, or this one once the box is below both limits",
        })
    }

    /// The wait in one line, for the runner's journal.
    #[must_use]
    pub fn line(&self) -> String {
        let mut over = Vec::new();
        if let (Some(load1), Some(limit)) = (self.load1, self.limits.load1)
            && load1 >= limit
        {
            over.push(format!("load1 {load1:.1} >= {limit} ({LOAD1_KEY})"));
        }
        if let (Some(some), Some(limit)) = (self.cpu_some, self.limits.cpu_some)
            && some >= limit
        {
            over.push(format!(
                "CPU some avg10 {some:.1} % >= {limit} ({CPU_SOME_KEY})"
            ));
        }
        format!("box busy: {}", over.join(", "))
    }
}

/// The box's limits as `git` reads them (global, or the clone's own config).
///
/// # Errors
///
/// A value that is not a number, or a `git config` that fails: a limit that cannot be read is
/// never no limit.
pub fn limits(git: &Git) -> Result<Limits, String> {
    Ok(Limits {
        load1: limit(git, LOAD1_KEY)?,
        cpu_some: limit(git, CPU_SOME_KEY)?,
    })
}

fn limit(git: &Git, key: &str) -> Result<Option<f64>, String> {
    let value = git
        .config_get(key)
        .map_err(|error| format!("{key} could not be read: {}", error.message()))?;
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    match value.parse::<f64>() {
        Ok(number) if number.is_finite() && number >= 0.0 => Ok(Some(number)),
        _ => Err(format!("{key} = \"{value}\" is not a number")),
    }
}

/// Whether this box is at or above a limit it sets: `None` when it sets none, or is below
/// each. Only what a limit is set for is measured.
///
/// # Errors
///
/// A limit cannot be read, or what a set limit names cannot be measured.
pub fn busy(git: &Git) -> Result<Option<Busy>, String> {
    let limits = limits(git)?;
    if (limits.load1.is_none() && limits.cpu_some.is_none()) || !judged() {
        return Ok(None);
    }
    let load1 = limits.load1.map(|_| load1()).transpose()?;
    let cpu_some = limits.cpu_some.map(|_| cpu_some()).transpose()?;
    let over = |measured: Option<f64>, limit: Option<f64>| {
        measured
            .zip(limit)
            .is_some_and(|(measured, limit)| measured >= limit)
    };
    Ok(
        (over(load1, limits.load1) || over(cpu_some, limits.cpu_some)).then_some(Busy {
            load1,
            cpu_some,
            limits,
        }),
    )
}

/// Whether this system's load is judged: Linux, or a test build.
fn judged() -> bool {
    cfg!(target_os = "linux") || TEST_HOOKS
}

/// A test build reads `name` in place of the box's own value: no test can load the machine.
fn test_value(name: &str) -> Option<f64> {
    TEST_HOOKS
        .then(|| std::env::var(name).ok())
        .flatten()
        .and_then(|value| value.parse().ok())
}

/// The one-minute load average, from `/proc/loadavg`.
fn load1() -> Result<f64, String> {
    if let Some(value) = test_value("BAY_TEST_LOAD1") {
        return Ok(value);
    }
    let text = std::fs::read_to_string("/proc/loadavg")
        .map_err(|error| format!("/proc/loadavg could not be read: {error}"))?;
    first_load(&text).ok_or_else(|| format!("/proc/loadavg could not be read: {}", text.trim()))
}

/// The CPU pressure `some avg10`, from `/proc/pressure/cpu`.
fn cpu_some() -> Result<f64, String> {
    if let Some(value) = test_value("BAY_TEST_CPU_SOME") {
        return Ok(value);
    }
    let text = std::fs::read_to_string("/proc/pressure/cpu")
        .map_err(|error| format!("/proc/pressure/cpu could not be read: {error}"))?;
    some_avg10(&text)
        .ok_or_else(|| format!("/proc/pressure/cpu could not be read: {}", text.trim()))
}

/// The first field of `/proc/loadavg`.
fn first_load(text: &str) -> Option<f64> {
    text.split_whitespace().next()?.parse().ok()
}

/// `avg10` of the `some` line of a `/proc/pressure/*` file.
fn some_avg10(text: &str) -> Option<f64> {
    let line = text.lines().find(|line| line.starts_with("some "))?;
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_load_is_the_first_field_of_loadavg() {
        assert_eq!(first_load("30.51 17.76 15.02 3/1234 5678\n"), Some(30.51));
        assert_eq!(first_load(""), None);
        assert_eq!(first_load("busy 1 2\n"), None);
    }

    #[test]
    fn the_pressure_is_avg10_of_the_some_line() {
        let cpu = "some avg10=41.00 avg60=30.21 avg300=10.00 total=123456\n\
                   full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        assert_eq!(some_avg10(cpu), Some(41.0));
        assert_eq!(
            some_avg10("full avg10=3.00 avg60=0 avg300=0 total=0\n"),
            None
        );
        assert_eq!(some_avg10("some avg60=1.00\n"), None);
        assert_eq!(some_avg10(""), None);
    }

    #[test]
    fn a_busy_line_names_each_limit_it_is_over() {
        let busy = Busy {
            load1: Some(30.51),
            cpu_some: Some(12.0),
            limits: Limits {
                load1: Some(24.0),
                cpu_some: Some(20.0),
            },
        };
        assert_eq!(
            busy.line(),
            "box busy: load1 30.5 >= 24 (bay.checkStartLoad1)"
        );
    }
}
