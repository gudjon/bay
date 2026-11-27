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
//!
//! G898: a box also caps its runner's share of the work, with no landing. `[runner]
//! parallel_checks` and `ahead` are the repository's, so they reach every box at once and change
//! only by a landing; one box in a hot spell had no cap of its own. Git config, global or for one
//! clone, `bay.runnerParallelChecks` and `bay.runnerAhead`: each only lowers the repository's
//! value on this box and never raises it. Each is read where the runner uses it, so a change
//! holds from its next visit, with no restart. A value bay cannot take (not a whole number, or
//! outside the range of the repository's own key) leaves the repository's value in force, and
//! `bay doctor` names it: a cap never stops a runner.
//!
//! G946 (a fleet finding, 2026-11-27: a box's two runners waited 189 times in a day for CPU some
//! of 20 %, read at 17.6 % for the box while the job it protects stalled 0.7 %): the box's own
//! checks raised the pressure that held them. Git config `bay.checkStartCpuCgroup`, global or
//! for one clone, any number of values, names the cgroups the box protects, each a cgroup v2 path
//! under `/sys/fs/cgroup`; a `*` in a segment stands for any run of characters in it (a unit
//! named anew at each start). While one is set, `bay.checkStartCpuSome` is judged against the
//! highest `some avg10` of the cgroups they match (`cpu.pressure`), never the box's. One that
//! matches none runs nothing the box protects, so it is no wait; `bay doctor` names it.

use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::Config;
use crate::git::Git;
use crate::version::TEST_HOOKS;

/// The one-minute load average at or above which a runner starts no check.
pub const LOAD1_KEY: &str = "bay.checkStartLoad1";

/// The CPU pressure (`some avg10`, percent) at or above which a runner starts no check.
pub const CPU_SOME_KEY: &str = "bay.checkStartCpuSome";

/// G946: the cgroups whose CPU pressure `bay.checkStartCpuSome` is judged against, in place of
/// the box's.
pub const CPU_CGROUP_KEY: &str = "bay.checkStartCpuCgroup";

/// Where cgroup v2 is mounted.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// G898: the box's cap on `[runner] parallel_checks`, how many of a candidate's checks its
/// runner runs at once.
pub const PARALLEL_CHECKS_KEY: &str = "bay.runnerParallelChecks";

/// G898: the box's cap on `[runner] ahead`, how many queue entries its runner checks ahead of
/// their turn.
pub const AHEAD_KEY: &str = "bay.runnerAhead";

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
#[derive(Clone, Debug, PartialEq)]
pub struct Busy {
    pub load1: Option<f64>,
    pub cpu_some: Option<f64>,
    /// G946: the cgroup `cpu_some` was read from, when the box names any ([`CPU_CGROUP_KEY`]);
    /// `None` is the whole box.
    pub cpu_of: Option<String>,
    pub limits: Limits,
}

impl Busy {
    /// `BOX_BUSY`'s detail.
    #[must_use]
    pub fn detail(&self) -> Value {
        json!({
            "load1": self.load1,
            "cpu_some_avg10": self.cpu_some,
            "cpu_some_of": self.cpu_of,
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
            let of = self
                .cpu_of
                .as_ref()
                .map(|cgroup| format!(" of {cgroup}"))
                .unwrap_or_default();
            over.push(format!(
                "CPU some avg10 {some:.1} %{of} >= {limit} ({CPU_SOME_KEY})"
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
    let (cpu_some, cpu_of) = match limits.cpu_some {
        Some(_) => cpu_pressure(git)?,
        None => (None, None),
    };
    let over = |measured: Option<f64>, limit: Option<f64>| {
        measured
            .zip(limit)
            .is_some_and(|(measured, limit)| measured >= limit)
    };
    Ok(
        (over(load1, limits.load1) || over(cpu_some, limits.cpu_some)).then_some(Busy {
            load1,
            cpu_some,
            cpu_of,
            limits,
        }),
    )
}

/// G898: a cap the box sets on one of the repository's runner settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cap {
    pub key: &'static str,
    /// The repository's value.
    pub repository: usize,
    /// The box's value, or why bay cannot take it.
    pub set: Result<usize, String>,
}

impl Cap {
    /// What the runner uses: the repository's value, lowered by the box's. A cap never raises
    /// it, and one bay cannot take leaves it.
    #[must_use]
    pub fn in_force(&self) -> usize {
        self.set
            .as_ref()
            .map_or(self.repository, |&set| set.min(self.repository))
    }

    /// The cap as `bay doctor` names it.
    #[must_use]
    pub fn row(&self) -> Value {
        let mut row = json!({"key": self.key, "repository": self.repository,
            "in_force": self.in_force()});
        match &self.set {
            Ok(set) => row["set"] = json!(set),
            Err(why) => row["refused"] = json!(why),
        }
        row
    }
}

/// The box's cap on `[runner] parallel_checks`; `None` when it sets none. It lowers the
/// checks at once to one, never to none.
#[must_use]
pub fn parallel_checks_cap(git: &Git, config: &Config) -> Option<Cap> {
    cap(
        git,
        PARALLEL_CHECKS_KEY,
        config.parallel_checks(),
        &(1..=crate::check::MAX_PARALLEL_CHECKS),
    )
}

/// The box's cap on `[runner] ahead`; `None` when it sets none. At 0 the runner checks no
/// entry ahead of its turn.
#[must_use]
pub fn ahead_cap(git: &Git, config: &Config) -> Option<Cap> {
    cap(
        git,
        AHEAD_KEY,
        config.ahead(),
        &(0..=crate::throughput::MAX_AHEAD),
    )
}

/// The cap `key` sets, taken only in `range`, the range of the repository's own key.
fn cap(
    git: &Git,
    key: &'static str,
    repository: usize,
    range: &RangeInclusive<usize>,
) -> Option<Cap> {
    let set = match git.config_get(key) {
        Ok(None) => return None,
        Ok(Some(value)) => {
            let value = value.trim();
            value
                .parse::<usize>()
                .ok()
                .filter(|set| range.contains(set))
                .ok_or_else(|| {
                    format!(
                        "{key} = \"{value}\" is not a whole number from {} to {}, so the \
                         repository's {repository} holds",
                        range.start(),
                        range.end()
                    )
                })
        }
        Err(error) => Err(format!("{key} could not be read: {}", error.message())),
    };
    Some(Cap {
        key,
        repository,
        set,
    })
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

/// G946: the CPU pressure the box judges and where it was read: the highest of the cgroups it
/// names ([`CPU_CGROUP_KEY`]), with that cgroup, or the box's when it names none. `None` when
/// they match no cgroup: nothing the box protects runs.
fn cpu_pressure(git: &Git) -> Result<(Option<f64>, Option<String>), String> {
    let patterns = cgroups(git)?;
    if patterns.is_empty() {
        return Ok((Some(cpu_some()?), None));
    }
    let highest = pressures(&cgroup_root(), &patterns)?
        .into_iter()
        .max_by(|(_, a), (_, b)| a.total_cmp(b));
    Ok(highest.map_or((None, None), |(cgroup, some)| (Some(some), Some(cgroup))))
}

/// G946: every value of [`CPU_CGROUP_KEY`] as `git` reads it. Unset is none.
///
/// # Errors
///
/// A value with a `..` segment, or a `git config` that fails.
pub fn cgroups(git: &Git) -> Result<Vec<String>, String> {
    let values = match git.run_status(&["config", "--get-all", CPU_CGROUP_KEY]) {
        // `git config --get-all` exits 1 for a key that is not set.
        Ok((1, _, _)) => return Ok(Vec::new()),
        Ok((0, values, _)) => values,
        Ok((_, _, stderr)) => return Err(format!("{CPU_CGROUP_KEY} could not be read: {stderr}")),
        Err(error) => {
            return Err(format!(
                "{CPU_CGROUP_KEY} could not be read: {}",
                error.message()
            ));
        }
    };
    values
        .lines()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            if value.split('/').any(|segment| segment == "..") {
                Err(format!(
                    "{CPU_CGROUP_KEY} = \"{value}\" is not a cgroup path under {CGROUP_ROOT}"
                ))
            } else {
                Ok(value.trim_matches('/').to_owned())
            }
        })
        .collect()
}

/// G946: each cgroup the box names ([`cgroups`]) that exists now, with its CPU pressure
/// `some avg10`, for `bay doctor`.
///
/// # Errors
///
/// The key cannot be read, or a cgroup it matches has no readable `cpu.pressure`.
pub fn cgroup_pressures(git: &Git) -> Result<Vec<(String, f64)>, String> {
    pressures(&cgroup_root(), &cgroups(git)?)
}

/// Where cgroups are read: [`CGROUP_ROOT`], or `BAY_TEST_CGROUP_ROOT` in a test build.
fn cgroup_root() -> std::path::PathBuf {
    TEST_HOOKS
        .then(|| std::env::var("BAY_TEST_CGROUP_ROOT").ok())
        .flatten()
        .unwrap_or_else(|| CGROUP_ROOT.to_owned())
        .into()
}

/// The cgroups under `root` that `patterns` match, each once and in order, with `some avg10` of
/// its `cpu.pressure`.
fn pressures(root: &std::path::Path, patterns: &[String]) -> Result<Vec<(String, f64)>, String> {
    let mut found: Vec<String> = Vec::new();
    for pattern in patterns {
        for cgroup in matched(root, pattern) {
            if !found.contains(&cgroup) {
                found.push(cgroup);
            }
        }
    }
    found
        .into_iter()
        .map(|cgroup| {
            let file = root.join(&cgroup).join("cpu.pressure");
            let text = std::fs::read_to_string(&file)
                .map_err(|error| format!("{} could not be read: {error}", file.display()))?;
            let some = some_avg10(&text)
                .ok_or_else(|| format!("{} could not be read: {}", file.display(), text.trim()))?;
            Ok((cgroup, some))
        })
        .collect()
}

/// The directories under `root` that `pattern` names, relative to it, sorted: a segment with a
/// `*` matches each directory whose name it fits ([`fits`]), any other names one.
fn matched(root: &std::path::Path, pattern: &str) -> Vec<String> {
    let mut found = vec![String::new()];
    for segment in pattern.split('/').filter(|segment| !segment.is_empty()) {
        let mut next = Vec::new();
        for dir in &found {
            let at = root.join(dir);
            let join = |name: &str| {
                if dir.is_empty() {
                    name.to_owned()
                } else {
                    format!("{dir}/{name}")
                }
            };
            if !segment.contains('*') {
                if at.join(segment).is_dir() {
                    next.push(join(segment));
                }
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&at) else {
                continue;
            };
            let mut names: Vec<String> = entries
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|name| fits(segment, name))
                .collect();
            names.sort();
            next.extend(names.iter().map(|name| join(name)));
        }
        found = next;
    }
    found.retain(|cgroup| !cgroup.is_empty());
    found
}

/// Whether `name` fits `pattern`, where each `*` stands for any run of characters.
fn fits(pattern: &str, name: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(rest) = name.strip_prefix(parts.next().unwrap_or_default()) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else {
        return rest.is_empty();
    };
    let mut rest = rest;
    for part in middle {
        let Some(at) = rest.find(part) else {
            return false;
        };
        rest = &rest[at + part.len()..];
    }
    rest.len() >= last.len() && rest.ends_with(last)
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

    /// G898: a cap lowers the repository's value and never raises it; one bay cannot take
    /// leaves it.
    #[test]
    fn a_cap_only_lowers_the_repositorys_value() {
        let cap = |set| Cap {
            key: AHEAD_KEY,
            repository: 4,
            set,
        };
        assert_eq!(cap(Ok(2)).in_force(), 2);
        assert_eq!(cap(Ok(0)).in_force(), 0);
        assert_eq!(cap(Ok(8)).in_force(), 4);
        assert_eq!(cap(Err("bad".to_owned())).in_force(), 4);
        assert_eq!(cap(Ok(2)).row()["set"], 2);
        assert_eq!(cap(Err("bad".to_owned())).row()["refused"], "bad");
    }

    #[test]
    fn a_busy_line_names_each_limit_it_is_over() {
        let busy = Busy {
            load1: Some(30.51),
            cpu_some: Some(12.0),
            cpu_of: None,
            limits: Limits {
                load1: Some(24.0),
                cpu_some: Some(20.0),
            },
        };
        assert_eq!(
            busy.line(),
            "box busy: load1 30.5 >= 24 (bay.checkStartLoad1)"
        );
        let pressed = Busy {
            load1: None,
            cpu_some: Some(41.0),
            cpu_of: Some("system.slice/plant-7.service".to_owned()),
            limits: Limits {
                load1: None,
                cpu_some: Some(20.0),
            },
        };
        assert_eq!(
            pressed.line(),
            "box busy: CPU some avg10 41.0 % of system.slice/plant-7.service >= 20 (bay.checkStartCpuSome)"
        );
    }

    /// G946: a `*` stands for any run of characters, and only in the segment it is in.
    #[test]
    fn a_star_fits_any_run_of_characters() {
        assert!(fits(
            "plant-*.service",
            "plant-1805163-1795641087145.service"
        ));
        assert!(fits("plant-*.service", "plant-.service"));
        assert!(fits("*", "anything"));
        assert!(fits("a*b*c", "abc"));
        assert!(fits("exact", "exact"));
        assert!(!fits("exact", "exactly"));
        assert!(!fits("plant-*.service", "plant-1.scope"));
        assert!(!fits("ab*b", "ab"));
        assert!(!fits("bay-*", "plant-1"));
    }

    /// G946: the cgroups a box names are read under the root, each once, with their pressure;
    /// one that matches nothing adds nothing.
    #[test]
    fn the_named_cgroups_are_read_with_their_pressure() {
        let root = std::env::temp_dir().join(format!("bay-g946-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cgroup = |path: &str, some: &str| {
            let dir = root.join(path);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("cpu.pressure"),
                format!(
                    "some avg10={some} avg60=1.00 avg300=1.00 total=1
full avg10=0.00 avg60=0.00 avg300=0.00 total=0
"
                ),
            )
            .unwrap();
        };
        cgroup("user.slice/u/plant-2.service", "3.00");
        cgroup("user.slice/u/plant-1.service", "41.00");
        cgroup("user.slice/u/bay-r.service", "90.00");
        let read = |patterns: &[&str]| {
            pressures(
                &root,
                &patterns.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>(),
            )
        };
        assert_eq!(
            read(&[
                "user.slice/u/plant-*.service",
                "user.slice/u/plant-1.service"
            ])
            .unwrap(),
            vec![
                ("user.slice/u/plant-1.service".to_owned(), 41.0),
                ("user.slice/u/plant-2.service".to_owned(), 3.0),
            ]
        );
        assert_eq!(read(&["user.slice/*/none-*.service"]).unwrap(), vec![]);
        assert_eq!(read(&["system.slice"]).unwrap(), vec![]);
        std::fs::create_dir_all(root.join("user.slice/u/plant-3.service")).unwrap();
        assert!(
            read(&["user.slice/u/plant-3.service"])
                .unwrap_err()
                .contains("cpu.pressure could not be read")
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
