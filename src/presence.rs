//! An executor's presence, `refs/bay/executor/<executor>` (G67; fleet-rollout-analysis F6).
//!
//! An executor that is down cannot otherwise be seen: agents on other boxes see no process, and
//! an idle executor writes nothing else to the remote. So each `bay executor run` pass publishes
//! one small record of itself: one parentless commit whose tree is `presence.json` alone (schema
//! `bay.executor/1`), replacing the ref's last one as a live summary does (`crate::live`). It
//! names the executor, its machine (`bay.machine`, never a pid), its bay and Git versions, the
//! check environments the box serves (`bay.checkEnvironment`), the time the pass began, the
//! longest gap it leaves between two writes while idle (`cadence_s`), the operation the pass
//! drives first, if any (`holding`), and the live queue depth it saw. Under a key registry it is
//! signed by the executor key, and the host hook takes it only from the executor it is named for.
//!
//! A looping executor writes it at most once per heartbeat (`[executor] heartbeat_s`). A write
//! costs a fixed number of git spawns (one config read, four plumbing calls, one push) whatever
//! the queue or the history holds, and a pass within a heartbeat of the last write spawns none.
//! The write is best effort: a failure never stops the executor, and its outcome is kept in
//! `<git-common-dir>/bay/presence-push.json`, which `bay doctor --role executor` shows.
//!
//! A reader judges a presence by its age: the larger of its age on the reader's clock since the
//! fetch that first brought it, and its own time against the reader's clock. So a presence first
//! fetched long after it was written never reads as fresh; a skewed clock can only make it read
//! older, and a stale presence is a warning, never a refusal. It is stale past three of its own
//! cadences or the takeover window, whichever is longer, plus the progress deadline while it
//! names an operation it drives, since an executor inside an operation writes nothing.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::Config;
use crate::git::{Git, GitError};
use crate::registry::{KeyClass, Registry};
use crate::repo::Repo;
use crate::signing::Keys;

pub const PRESENCE_PREFIX: &str = crate::names::EXECUTOR;
pub const PRESENCE_SCHEMA: &str = "bay.executor/1";
pub const PRESENCE_FILE: &str = "presence.json";
/// The largest `presence.json` the host accepts and a reader reads.
pub const PRESENCE_MAX_BYTES: u64 = 4 * 1024;
/// How many of its own cadences a presence may miss before it is stale.
const STALE_CADENCES: u64 = 3;
/// The file the last write's outcome is kept in, under `<git-common-dir>/bay/`.
const OUTCOME_FILE: &str = "presence-push.json";

/// Test builds honour `BAY_TEST_PRESENCE_CLOCK_SKEW_S`; a release build never does.
const TEST_HOOKS: bool = option_env!("BAY_RELEASE_VERSION").is_none();

#[must_use]
pub fn presence_ref(executor: &str) -> String {
    format!("{PRESENCE_PREFIX}{executor}")
}

/// The reader's clock for presence ages: this machine's, or in a test build moved forward by
/// `BAY_TEST_PRESENCE_CLOCK_SKEW_S` seconds, so G67 reads a presence as a later reader would.
#[must_use]
pub fn reader_now() -> u64 {
    let skew = std::env::var("BAY_TEST_PRESENCE_CLOCK_SKEW_S")
        .ok()
        .filter(|_| TEST_HOOKS)
        .and_then(|text| text.parse::<u64>().ok())
        .unwrap_or(0);
    crate::live::unix_now().saturating_add(skew)
}

/// What a running executor keeps between passes for its presence: what does not change while
/// it runs, when it last wrote, and how that write went.
pub struct Beat {
    git: Option<String>,
    interval_s: u64,
    last: Option<Instant>,
    /// The last write's outcome, for `--once`'s detail.
    pub outcome: Option<Value>,
}

impl Beat {
    /// `git` is the Git version the executor runs, read once at start; `interval` its wait
    /// between passes.
    #[must_use]
    pub const fn new(git: Option<String>, interval: Duration) -> Self {
        Self {
            git,
            interval_s: interval.as_secs(),
            last: None,
            outcome: None,
        }
    }

    /// Writes this executor's presence and pushes it, unless the last write is less than one
    /// heartbeat old. Best effort: the outcome goes to [`Self::outcome`] and the outcome file.
    pub fn publish(&mut self, repo: &Repo, holding: Option<&str>, queue_depth: usize) {
        if self
            .last
            .is_some_and(|last| last.elapsed() < repo.config.heartbeat())
        {
            return;
        }
        self.last = Some(Instant::now());
        let keys = Keys::load(&repo.git);
        let record = self.record(repo, &keys, holding, queue_depth);
        let outcome =
            write(repo, &keys, &record).and_then(|(name, commit)| push(repo, &name, &commit));
        let kept = outcome.unwrap_or_else(|why| json!({"pushed": false, "why": why}));
        keep(repo, &kept);
        self.outcome = Some(kept);
    }

    fn record(&self, repo: &Repo, keys: &Keys, holding: Option<&str>, queue_depth: usize) -> Value {
        let served = crate::check::Served::read(&repo.git);
        json!({
            "schema": PRESENCE_SCHEMA,
            "executor": keys.executor_name,
            "machine": crate::live::machine(&repo.git).ok(),
            "bay": crate::version::BAY,
            "git": self.git,
            "environments": {"source": served.source(), "declared": served.declared()},
            "at": crate::clock::now_rfc3339(),
            "cadence_s": self.interval_s.max(repo.config.heartbeat().as_secs()),
            "holding": holding,
            "queue_depth": queue_depth,
        })
    }
}

/// Writes `record` as this executor's presence commit and points the local presence ref at it:
/// its name and the commit.
fn write(repo: &Repo, keys: &Keys, record: &Value) -> Result<(String, String), String> {
    let executor = keys.executor_name.as_str();
    crate::names::operator_name(executor).map_err(|why| format!("bay.executorName: {why}"))?;
    let git = &repo.git;
    let mut text = serde_json::to_string_pretty(record).map_err(|error| error.to_string())?;
    text.push('\n');
    let blob = git
        .run_with(
            &["hash-object", "-w", "--stdin"],
            &[],
            Some(text.as_bytes()),
        )
        .map_err(|error| error.message())?;
    let entry = format!("100644 blob {blob}\t{PRESENCE_FILE}\n");
    let tree = git
        .run_with(&["mktree"], &[], Some(entry.as_bytes()))
        .map_err(|error| error.message())?;
    let signer = keys.executor.as_ref();
    let mut args: Vec<String> = signer
        .map(crate::signing::Signer::config_args)
        .unwrap_or_default();
    args.extend(["commit-tree".to_owned(), tree]);
    args.push(
        if signer.is_some() {
            "-S"
        } else {
            "--no-gpg-sign"
        }
        .to_owned(),
    );
    args.extend(["-m".to_owned(), format!("presence {executor}")]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // Unsigned, the committer names the executor.
    let email = format!("{executor}@bay.invalid");
    let envs = [
        ("GIT_AUTHOR_NAME", executor),
        ("GIT_AUTHOR_EMAIL", email.as_str()),
        ("GIT_COMMITTER_NAME", executor),
        ("GIT_COMMITTER_EMAIL", email.as_str()),
    ];
    let commit = git
        .run_with(&args, &envs, None)
        .map_err(|error| error.message())?;
    let name = presence_ref(executor);
    git.run(&["update-ref", &name, &commit])
        .map_err(|error| error.message())?;
    Ok((name, commit))
}

fn push(repo: &Repo, name: &str, commit: &str) -> Result<Value, String> {
    // The commit just written, never the local ref's name, as the live push does.
    repo.git
        .run(&["push", "-q", repo.remote(), &format!("+{commit}:{name}")])
        .map_err(|error| format!("{name} could not be pushed: {}", error.message()))?;
    Ok(json!({"ref": name, "commit": commit, "pushed": true}))
}

fn keep(repo: &Repo, outcome: &Value) {
    let mut record = outcome.clone();
    record["at"] = json!(crate::clock::now_rfc3339());
    let dir = repo.common_dir.join("bay");
    let path = dir.join(OUTCOME_FILE);
    let tmp = path.with_extension("tmp");
    if std::fs::create_dir_all(&dir).is_ok() && std::fs::write(&tmp, record.to_string()).is_ok() {
        let _ = crate::durable::rename(&tmp, &path);
    }
}

/// The outcome of this repository's last presence write, as the executor kept it.
#[must_use]
pub fn last_push(common_dir: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(common_dir.join("bay").join(OUTCOME_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The `presence.json` a presence commit carries; `None` where its tree holds none or one that
/// is not UTF-8 JSON.
///
/// # Errors
///
/// The tree names `presence.json` and its blob cannot be read, or a git fault.
pub fn record_at(git: &Git, commit: &str) -> Result<Option<Value>, String> {
    match git.file_at(commit, PRESENCE_FILE) {
        Ok(text) => Ok(text.and_then(|text| serde_json::from_str(&text).ok())),
        Err(GitError::NotUtf8 { .. }) => Ok(None),
        Err(error) => Err(error.message()),
    }
}

/// How a reader judges one presence at `now`: its age, the age past which it is stale, and
/// whether it is. `since` is when this reader first fetched the commit, on its own clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Age {
    pub age_s: u64,
    pub stale_after_s: u64,
    pub stale: bool,
}

/// The [`Age`] of `record`, first fetched at `since`, read at `now` under a takeover window of
/// `window` and a progress deadline of `deadline` seconds. A record without its own time cannot
/// be judged fresh.
#[must_use]
pub fn age(record: &Value, since: u64, now: u64, window: u64, deadline: u64) -> Age {
    let fetched = now.saturating_sub(since);
    let own = crate::live::unix_at(record).map(|at| now.saturating_sub(at));
    let age_s = own.map_or(fetched, |own| own.max(fetched));
    let cadence = record["cadence_s"].as_u64().unwrap_or(0);
    let mut stale_after_s = STALE_CADENCES.saturating_mul(cadence).max(window);
    if record["holding"].is_string() {
        stale_after_s = stale_after_s.saturating_add(deadline);
    }
    Age {
        age_s,
        stale_after_s,
        stale: own.is_none() || age_s >= stale_after_s,
    }
}

/// One presence a reader holds: the executor it is named for, its commit, and when this reader
/// first fetched that commit (Unix seconds, its own clock).
pub struct Held {
    pub executor: String,
    pub commit: String,
    pub since: u64,
}

/// The rows a reader shows for the presences it holds, read at `now`: each one's record fields,
/// its age, `stale`, and `readable`. Two `cat-file` calls whatever the count, and under a key
/// registry one signature check each. A presence that cannot be read, holds no
/// `bay.executor/1` record of its executor, or (under a registry) is not signed by that
/// executor's key is listed `readable: false` with why, and is stale: it proves no executor.
///
/// # Errors
///
/// A git fault, or a registry that does not load.
pub fn rows(git: &Git, config: &Config, held: &[Held], now: u64) -> Result<Vec<Value>, String> {
    if held.is_empty() {
        return Ok(Vec::new());
    }
    let commits: Vec<String> = held.iter().map(|held| held.commit.clone()).collect();
    let reads = crate::live::files_at(git, &commits, PRESENCE_FILE, PRESENCE_MAX_BYTES)?;
    let executors = match Registry::current(git)?.filter(Registry::signs_all) {
        Some(registry) => Some(registry.class_file(git, KeyClass::Executor)?),
        None => None,
    };
    let window = config.takeover().as_secs();
    let deadline = config.progress_deadline().as_secs();
    let mut rows = Vec::with_capacity(held.len());
    for (held, read) in held.iter().zip(reads) {
        let checked = read
            .and_then(|record| {
                record.ok_or_else(|| "its record is missing, too large or not JSON".to_owned())
            })
            .and_then(|record| {
                if record["schema"] == PRESENCE_SCHEMA && record["executor"] == held.executor {
                    Ok(record)
                } else {
                    Err(format!(
                        "it holds no {PRESENCE_SCHEMA} record of this executor"
                    ))
                }
            })
            .and_then(|record| {
                verified(git, &held.commit, &held.executor, executors.as_deref()).map(|()| record)
            });
        rows.push(match checked {
            Ok(record) => {
                let judged = age(&record, held.since, now, window, deadline);
                json!({
                    "executor": held.executor,
                    "machine": record["machine"],
                    "bay": record["bay"],
                    "git": record["git"],
                    "environments": record["environments"],
                    "last_pass": record["at"],
                    "holding": record["holding"],
                    "queue_depth": record["queue_depth"],
                    "commit": held.commit,
                    "readable": true,
                    "age_s": judged.age_s,
                    "stale_after_s": judged.stale_after_s,
                    "stale": judged.stale,
                })
            }
            Err(why) => json!({
                "executor": held.executor,
                "commit": held.commit,
                "readable": false,
                "why": why,
                "age_s": now.saturating_sub(held.since),
                "stale": true,
            }),
        });
    }
    Ok(rows)
}

/// Under a registry, whether presence commit `commit` is signed by the key of `executor`.
fn verified(
    git: &Git,
    commit: &str,
    executor: &str,
    executors: Option<&Path>,
) -> Result<(), String> {
    let Some(executors) = executors else {
        return Ok(());
    };
    match crate::signing::verify_commit(git, commit, executors)
        .map_err(|error| format!("its signature cannot be checked: {}", error.message()))?
    {
        Some(principal) if principal == executor => Ok(()),
        Some(principal) => Err(format!("it is signed by executor {principal}")),
        None => Err("its signature does not verify against the executor keys".to_owned()),
    }
}

/// Whether some row of `rows` is a readable presence that is not stale.
#[must_use]
pub fn any_fresh(rows: &[Value]) -> bool {
    rows.iter()
        .any(|row| row["readable"] == true && row["stale"] == false)
}

#[cfg(test)]
mod tests {
    use super::age;
    use serde_json::json;

    #[test]
    fn a_presence_is_as_old_as_the_older_of_its_fetch_and_its_own_time() {
        let at = crate::clock::unix_of("2026-10-21T10:00:00Z").unwrap();
        let record = json!({"at": "2026-10-21T10:00:00Z", "cadence_s": 10, "holding": null});
        // Fetched a moment ago, written long ago: it is old (a first fetch is not a heartbeat).
        let judged = age(&record, at + 3590, at + 3600, 120, 1860);
        assert_eq!(judged.age_s, 3600);
        assert!(judged.stale);
        // Fresh by both clocks.
        let judged = age(&record, at, at + 5, 120, 1860);
        assert_eq!(
            (judged.age_s, judged.stale_after_s, judged.stale),
            (5, 120, false)
        );
        // Three cadences past the window.
        let slow = json!({"at": "2026-10-21T10:00:00Z", "cadence_s": 60, "holding": null});
        assert_eq!(age(&slow, at, at + 150, 120, 1860).stale_after_s, 180);
        // An executor inside an operation writes nothing until it ends.
        let busy = json!({"at": "2026-10-21T10:00:00Z", "cadence_s": 10, "holding": "abcd2345.1"});
        assert!(!age(&busy, at, at + 600, 120, 1860).stale);
        // No time of its own: never fresh.
        assert!(age(&json!({"cadence_s": 10}), at, at, 120, 1860).stale);
    }
}
