//! An executor's presence, `refs/bay/executor/<executor>` (G67; fleet-rollout-analysis F6).
//!
//! An executor that is down cannot otherwise be seen: agents on other boxes see no process, and
//! an idle executor writes nothing else to the remote. So each `bay runner run` pass publishes
//! one small record of itself: one parentless commit whose tree is `presence.json` alone (schema
//! `bay.executor/1`), replacing the ref's last one as a live summary does (`crate::live`). It
//! names the executor, its machine (`bay.machine`, never a pid), the producer it writes evidence
//! as (`bay.producerName`, when it names one: G807), its bay and Git versions, the
//! check environments the box serves (`bay.checkEnvironment`), the time the pass began, the
//! longest gap it leaves between two writes while idle (`cadence_s`), the operation the pass
//! drives first, if any (`holding`), and the live queue depth it saw. Under a key registry it is
//! signed by the executor key, and the host hook takes it only from the executor it is named for.
//!
//! A looping executor writes it at most once per heartbeat (`[runner] heartbeat_s`). A write
//! costs a fixed number of git spawns (one config read, four plumbing calls, one push) whatever
//! the queue or the history holds, and a pass within a heartbeat of the last write spawns none.
//! The write is best effort: a failure never stops the executor, and its outcome is kept in
//! `<git-common-dir>/bay/presence-push.json`, which `bay doctor --role runner` shows. A pass
//! writes it on a thread of its own and goes on at once (BACKLOG 277): one write at a time.
//!
//! A reader judges a presence by its age: the larger of its age on the reader's clock since the
//! fetch that first brought it, and its own time against the reader's clock. So a presence first
//! fetched long after it was written never reads as fresh; a skewed clock can only make it read
//! older, and a stale presence is a warning, never a refusal. It is stale past three of its own
//! cadences or the takeover window, whichever is longer, plus the progress deadline while it
//! names an operation it drives, since an executor inside an operation writes nothing.

use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;
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
use crate::version::TEST_HOOKS;

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
    /// BACKLOG 277: the write [`Self::publish_beside`] started, until its outcome is taken.
    beside: Option<JoinHandle<Value>>,
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
            beside: None,
        }
    }

    /// Beats every heartbeat on a thread of its own while `work` runs, so a runner in one long
    /// check never reads as gone (the 0.5.1 acceptance finding): the thread beats first, as
    /// `work` begins, when a heartbeat passed since the last beat, and once a heartbeat after
    /// that until `work` ends. `beside` runs on that thread too ([`Self::meanwhile`]).
    ///
    /// G789 (a fleet measurement, 2026-11-18: a push is 3 to 8 s at load): the first beat goes
    /// beside `work`, never ahead of it, so a runner that takes the next head after a landing
    /// starts its visit at once.
    pub fn during<T>(
        &mut self,
        repo: &Repo,
        holding: &str,
        queue_depth: usize,
        beside: &mut (dyn FnMut() + Send),
        work: impl FnOnce() -> T,
    ) -> T {
        self.beating(repo, (holding, queue_depth), true, beside, work)
    }

    /// Beats once a heartbeat on a thread of its own while `work` runs, never before it, so work
    /// that ends within a heartbeat writes nothing: a pass's post work, which mostly finds
    /// nothing to run, never renames the operation the presence names (G423). The same thread
    /// calls `beside` after each beat, until `work` ends: S1, the runner places the changes
    /// submitted meanwhile (`crate::executor`), never waiting for its work to end.
    pub fn meanwhile<T>(
        &mut self,
        repo: &Repo,
        holding: &str,
        queue_depth: usize,
        beside: &mut (dyn FnMut() + Send),
        work: impl FnOnce() -> T,
    ) -> T {
        self.beating(repo, (holding, queue_depth), false, beside, work)
    }

    /// [`Self::during`] (`first`: the thread beats as `work` begins, when one is due) and
    /// [`Self::meanwhile`] (it beats only once a heartbeat has passed since `work` began).
    fn beating<T>(
        &mut self,
        repo: &Repo,
        (holding, queue_depth): (&str, usize),
        first: bool,
        beside: &mut (dyn FnMut() + Send),
        work: impl FnOnce() -> T,
    ) -> T {
        let mut ticker = Self {
            git: self.git.clone(),
            interval_s: self.interval_s,
            last: if first {
                self.last
            } else {
                Some(Instant::now())
            },
            outcome: None,
            beside: None,
        };
        // BACKLOG 277: a write the pass started beside it ends on this thread, never on the path
        // of `work`; the thread beats after it. 277b: one that outlasts `work` is left beside the
        // runner, so the pass ends with its work and the next beat or [`Self::settle`] takes it.
        let mut started = self.beside.take();
        let done = std::sync::atomic::AtomicBool::new(false);
        let out = std::thread::scope(|scope| {
            scope.spawn(|| {
                if !ended_before(&mut started, &mut ticker.outcome, &done) {
                    return;
                }
                if first {
                    ticker.publish(repo, Some(holding), queue_depth);
                }
                while !done.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(250));
                    ticker.publish(repo, Some(holding), queue_depth);
                    if !done.load(std::sync::atomic::Ordering::SeqCst) {
                        beside();
                    }
                }
            });
            let out = work();
            done.store(true, std::sync::atomic::Ordering::SeqCst);
            out
        });
        self.beside = started;
        // G779: a beat the thread made is this executor's last write, so the next operation's
        // beat waits its heartbeat from it rather than pushing again at once.
        if ticker.outcome.is_some() {
            self.last = ticker.last;
            self.outcome = ticker.outcome;
        }
        out
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
        self.outcome = Some(written(repo, &keys, &record));
    }

    /// BACKLOG 277 (a fleet measurement, 2026-11-24: an idle pass pushed its presence on its
    /// own path, 4.8 to 6.1 s of a pass of about 15 s): as [`Self::publish`], but the write and
    /// its push run on a thread of their own, so the pass goes on at once. While one runs, none
    /// starts. The next beat takes its outcome ([`Self::during`], [`Self::meanwhile`]), or
    /// [`Self::settle`] does.
    pub fn publish_beside(
        &mut self,
        shared: &Arc<Repo>,
        holding: Option<&str>,
        queue_depth: usize,
    ) {
        if self.beside.as_ref().is_some_and(JoinHandle::is_finished) {
            self.settle();
        }
        if self.beside.is_some()
            || self
                .last
                .is_some_and(|last| last.elapsed() < shared.config.heartbeat())
        {
            return;
        }
        self.last = Some(Instant::now());
        let keys = Keys::load(&shared.git);
        let record = self.record(shared, &keys, holding, queue_depth);
        let repo = Arc::clone(shared);
        self.beside = Some(std::thread::spawn(move || written(&repo, &keys, &record)));
    }

    /// Waits for the write [`Self::publish_beside`] started and takes its outcome: before
    /// `--once` reports it, and before the process ends or runs another binary.
    pub fn settle(&mut self) {
        if let Some(outcome) = self.beside.take().and_then(|write| write.join().ok()) {
            self.outcome = Some(outcome);
        }
    }

    fn record(&self, repo: &Repo, keys: &Keys, holding: Option<&str>, queue_depth: usize) -> Value {
        let served = crate::check::Served::read(&repo.git);
        json!({
            "schema": PRESENCE_SCHEMA,
            "executor": keys.executor_name,
            "machine": crate::live::machine(&repo.git).ok(),
            "producer": named_producer(keys),
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

/// BACKLOG 277b: waits for the write `started` while `done` is unset, and takes its outcome
/// into `outcome`. False when `done` came first: the write still runs and stays in `started`.
fn ended_before(
    started: &mut Option<JoinHandle<Value>>,
    outcome: &mut Option<Value>,
    done: &std::sync::atomic::AtomicBool,
) -> bool {
    while let Some(write) = started.as_ref() {
        if write.is_finished() {
            if let Some(kept) = started.take().and_then(|write| write.join().ok()) {
                *outcome = Some(kept);
            }
            return true;
        }
        if done.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

/// Writes `record` as this executor's presence and pushes it; keeps and gives the outcome.
fn written(repo: &Repo, keys: &Keys, record: &Value) -> Value {
    let outcome = write(repo, keys, record).and_then(|(name, commit)| push(repo, &name, &commit));
    let kept = outcome.unwrap_or_else(|why| json!({"pushed": false, "why": why}));
    keep(repo, &kept);
    kept
}

/// Writes `record` as this executor's presence commit and points the local presence ref at it:
/// its name and the commit.
fn write(repo: &Repo, keys: &Keys, record: &Value) -> Result<(String, String), String> {
    let executor = keys.executor_name.as_str();
    crate::names::operator_name(executor).map_err(|why| format!("bay.runnerName: {why}"))?;
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

/// The latest time, in Unix seconds on the reader's clock ([`reader_now`]), until which a
/// presence this store holds, other than `me` (a presence ref), stays live and serves every one
/// of `environments` (one that declares none serves every one); `None` when none is live now.
/// Live is strict: beaten within three of its cadences and half a minute, so a runner that
/// stopped never holds a head for the hour its presence takes to read stale (a fleet finding,
/// 2026-10-26).
#[must_use]
pub fn serving_until(git: &Git, me: &str, environments: &[String]) -> Option<u64> {
    let listed = git
        .run(&[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            PRESENCE_PREFIX,
        ])
        .ok()?;
    let now = reader_now();
    listed
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(name, _)| *name != me)
        .filter_map(|(_, commit)| record_at(git, commit).ok().flatten())
        .filter(|record| {
            environments
                .iter()
                .all(|environment| serves(record, environment))
        })
        .filter_map(|record| live_until(&record))
        .filter(|until| *until >= now)
        .max()
}

/// Whether presence `record` serves `environment`: it declares it, or declares none and so serves
/// every one.
#[must_use]
pub fn serves(record: &Value, environment: &str) -> bool {
    record["environments"]["source"] == "default"
        || record["environments"]["declared"]
            .as_array()
            .is_some_and(|all| all.iter().any(|it| it == environment))
}

/// G822: every presence this store holds that reads live now by [`serving_until`]'s strict rule,
/// read once, for a reader that judges several checks against them.
#[must_use]
pub fn live(git: &Git) -> Vec<Value> {
    let Ok(listed) = git.run(&["for-each-ref", "--format=%(objectname)", PRESENCE_PREFIX]) else {
        return Vec::new();
    };
    let now = reader_now();
    listed
        .lines()
        .filter_map(|commit| record_at(git, commit).ok().flatten())
        .filter(|record| live_until(record).is_some_and(|until| until >= now))
        .collect()
}

/// Until when, in Unix seconds, `record` stays live under [`serving_until`]'s strict rule.
fn live_until(record: &Value) -> Option<u64> {
    let cadence = record["cadence_s"].as_u64().unwrap_or(10);
    let at = record["at"].as_str().and_then(crate::clock::unix_of)?;
    Some(at.saturating_add(cadence * STALE_CADENCES + 30))
}

/// The producer `keys` write evidence as, when this clone names one (`bay.producerName`). A
/// clone that names none takes one of its own when it first writes evidence (G297), so it shares
/// none.
#[must_use]
pub fn named_producer(keys: &Keys) -> Option<&str> {
    Some(keys.producer_name.as_str()).filter(|name| *name != crate::signing::UNNAMED)
}

/// G807: each runner, other than `executor`, whose presence this store holds, live under
/// [`serving_until`]'s strict rule, and names `producer`: its name and machine, sorted by name.
#[must_use]
pub fn sharing(git: &Git, executor: &str, producer: &str) -> Vec<(String, Option<String>)> {
    let me = presence_ref(executor);
    let Ok(listed) = git.run(&[
        "for-each-ref",
        "--format=%(refname) %(objectname)",
        PRESENCE_PREFIX,
    ]) else {
        return Vec::new();
    };
    let commits: Vec<String> = listed
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(name, _)| *name != me)
        .map(|(_, commit)| commit.to_owned())
        .collect();
    if commits.is_empty() {
        return Vec::new();
    }
    // Two `cat-file` calls whatever the count, as [`rows`] reads them.
    let Ok(reads) = crate::live::files_at(git, &commits, PRESENCE_FILE, PRESENCE_MAX_BYTES) else {
        return Vec::new();
    };
    let now = reader_now();
    let mut found: Vec<(String, Option<String>)> = reads
        .into_iter()
        .filter_map(|read| read.ok().flatten())
        .filter(|record| record["producer"] == producer)
        .filter(|record| live_until(record).is_some_and(|until| until >= now))
        .filter_map(|record| {
            let runner = record["executor"].as_str()?.to_owned();
            (runner != executor).then(|| (runner, record["machine"].as_str().map(str::to_owned)))
        })
        .collect();
    found.sort();
    found
}

/// Brings every runner's presence from the remote, forced, as the runner's pass does
/// ([`Repo::fetch_chains`]): whether the fetch went.
pub fn fetch(repo: &Repo) -> bool {
    repo.git
        .run(&["fetch", "-q", "--no-tags", repo.remote(), &refspec()])
        .is_ok()
}

/// The refspec that brings every runner's presence, forced.
#[must_use]
pub fn refspec() -> String {
    format!("+{PRESENCE_PREFIX}*:{PRESENCE_PREFIX}*")
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
        Some(registry) => Some(registry.class_file(git, KeyClass::Runner)?),
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
                        "it holds no {PRESENCE_SCHEMA} record of this runner"
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
        Some(principal) => Err(format!("it is signed by runner {principal}")),
        None => Err("its signature does not verify against the runner keys".to_owned()),
    }
}

/// Whether some row of `rows` is a readable presence that is not stale.
#[must_use]
pub fn any_fresh(rows: &[Value]) -> bool {
    rows.iter()
        .any(|row| row["readable"] == true && row["stale"] == false)
}

/// The operations of `record` that wait for an executor ([`crate::change::ChangeRecord::waits`]).
#[must_use]
pub fn waiting(record: &crate::change::ChangeRecord) -> usize {
    record
        .operations
        .iter()
        .filter(|op| record.waits(op))
        .count()
}

/// G287: the operations of every change this repository holds that wait for an executor
/// ([`waiting`]), read by one `for-each-ref` and one `cat-file --batch`; none when the chains
/// cannot be read.
#[must_use]
pub fn queued(repo: &Repo) -> usize {
    crate::change::Chain::new(&repo.git)
        .read_all()
        .map_or(0, |chains| {
            chains.iter().map(|held| waiting(&held.record)).sum()
        })
}

/// G872 (a measurement, 2026-11-23: in a store of 11,200 changes, [`queued`] took about 1 s of
/// each prompt's hook, and 4 s on a loaded box): whether operations wait and no executor presence
/// is fresh, the rule of [`no_live_executor`], judged in the cheap order. The presences of the
/// last recorded fetch come first; the change records are read only when none is fresh.
#[must_use]
pub fn unserved_queue(repo: &Repo) -> bool {
    if let Ok(Some(rows)) = crate::fleet::executors(repo)
        && any_fresh(&rows)
    {
        return false;
    }
    queued(repo) > 0
}

/// G67: operations are queued and no executor presence on record is fresh. The warning says how
/// many operations wait, the presences the last fleet fetch recorded, and the age of the
/// freshest readable one (`seen_s`, `null` when none was seen). Never a refusal: the operations
/// stay queued and land when an executor comes. `None` with nothing queued or a fresh presence.
/// `bay status` names it in `detail.warnings`; the agent hook tells a session of it (G287).
#[must_use]
pub fn no_live_executor(repo: &Repo, queued: usize) -> Option<Value> {
    if queued == 0 {
        return None;
    }
    let (executors, seen_s) = match crate::fleet::executors(repo) {
        Ok(Some(rows)) => {
            if any_fresh(&rows) {
                return None;
            }
            let seen = rows
                .iter()
                .filter(|row| row["readable"] == true)
                .filter_map(|row| row["age_s"].as_u64())
                .min();
            (Value::from(rows), seen)
        }
        Ok(None) => (json!([]), None),
        Err(why) => (json!({"unread": why}), None),
    };
    let executor = seen_s.map_or_else(
        || "none seen".to_owned(),
        |seen| format!("none seen for {seen}s"),
    );
    Some(json!({
        "code": "no_live_executor",
        "queued": queued,
        "executor": executor,
        "seen_s": seen_s,
        "executors": executors,
        "note": "operations are queued and no runner has been seen lately: they stay queued and merge once a runner runs (bay runner run)",
    }))
}

/// A fleet finding (2026-10-25): an `unserved_environment` warning when operations are queued
/// and live runners answer, but no live runner serves an environment a check of the policy at
/// `trunk` runs in. Those checks answer `unavailable` until a box declares it
/// (`bay.checkEnvironment`). A live runner that declares nothing serves every environment.
#[must_use]
pub fn unserved_environments(repo: &Repo, queued: usize, trunk: &str) -> Option<Value> {
    if queued == 0 {
        return None;
    }
    let rows = crate::fleet::executors(repo).ok()??;
    let live: Vec<&Value> = rows
        .iter()
        .filter(|row| row["readable"] == true && row["stale"] == false)
        .collect();
    if live.is_empty()
        || live
            .iter()
            .any(|row| row["environments"]["source"] == "default")
    {
        return None;
    }
    let served: Vec<&str> = live
        .iter()
        .filter_map(|row| row["environments"]["declared"].as_array())
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let policy = crate::policy::load_at(&repo.git, trunk).ok()??;
    let mut missing: Vec<(&str, &str)> = policy
        .checks
        .iter()
        // G245: an approval runs on no runner; a person's record answers it.
        .filter(|entry| !entry.is_approval())
        .filter(|entry| !served.contains(&entry.environment.as_str()))
        .map(|entry| (entry.environment.as_str(), entry.id.as_str()))
        .collect();
    if missing.is_empty() {
        return None;
    }
    missing.sort_unstable();
    let mut environments: Vec<&str> = missing
        .iter()
        .map(|(environment, _)| *environment)
        .collect();
    environments.dedup();
    let checks: Vec<Value> = missing
        .iter()
        .map(|(environment, check)| json!({"check": check, "environment": environment}))
        .collect();
    Some(json!({
        "code": "unserved_environment",
        "environments": environments,
        "checks": checks,
        "served": served,
        "note": "no live runner serves these environments, so their checks answer unavailable: add each with git config --add bay.checkEnvironment <environment> in the runner's clone, then restart the runner",
    }))
}

/// The warnings `bay status` gives about the runners while `queued` operations wait: no live
/// runner, an environment no live runner serves, and one only one machine serves.
pub fn runner_warnings(repo: &Repo, queued: usize, trunk: &str) -> Vec<Value> {
    no_live_executor(repo, queued)
        .into_iter()
        .chain(unserved_environments(repo, queued, trunk))
        .chain(single_machine_environments(repo, queued, trunk))
        .collect()
}

/// G586 (a fleet finding, 2026-10-30: three environments were served by one box's runner, and
/// each stop of that box stopped their checks, with nothing said). A `single_machine_environment`
/// warning when operations are queued, live runners answer from two or more machines, and the
/// runners that serve an environment a check of the policy at `trunk` runs in are all on one
/// machine. A live runner that declares nothing serves every environment; an environment no live
/// runner serves is [`unserved_environments`]'s.
#[must_use]
pub fn single_machine_environments(repo: &Repo, queued: usize, trunk: &str) -> Option<Value> {
    if queued == 0 {
        return None;
    }
    let rows = crate::fleet::executors(repo).ok()??;
    let live: Vec<(&str, &str, &Value)> = rows
        .iter()
        .filter(|row| row["readable"] == true && row["stale"] == false)
        .filter_map(|row| {
            Some((
                row["executor"].as_str()?,
                row["machine"].as_str()?,
                &row["environments"],
            ))
        })
        .collect();
    let mut machines: Vec<&str> = live.iter().map(|(_, machine, _)| *machine).collect();
    machines.sort_unstable();
    machines.dedup();
    if machines.len() < 2 {
        return None;
    }
    let serves = |environments: &Value, environment: &str| {
        environments["source"] == "default"
            || environments["declared"]
                .as_array()
                .is_some_and(|declared| declared.iter().any(|served| served == environment))
    };
    let policy = crate::policy::load_at(&repo.git, trunk).ok()??;
    let mut checks: Vec<(&str, &str)> = policy
        .checks
        .iter()
        // G245: an approval runs on no runner; a person's record answers it.
        .filter(|entry| !entry.is_approval())
        .map(|entry| (entry.environment.as_str(), entry.id.as_str()))
        .collect();
    checks.sort_unstable();
    let mut single = Vec::new();
    for (at, (environment, _)) in checks.iter().enumerate() {
        if at > 0 && checks[at - 1].0 == *environment {
            continue;
        }
        let serving: Vec<(&str, &str)> = live
            .iter()
            .filter(|(_, _, environments)| serves(environments, environment))
            .map(|(executor, machine, _)| (*executor, *machine))
            .collect();
        let Some((_, machine)) = serving.first() else {
            continue;
        };
        if serving.iter().any(|(_, other)| other != machine) {
            continue;
        }
        let ids: Vec<&str> = checks
            .iter()
            .filter(|(other, _)| other == environment)
            .map(|(_, check)| *check)
            .collect();
        let runners: Vec<&str> = serving.iter().map(|(executor, _)| *executor).collect();
        single.push(json!({
            "environment": environment,
            "machine": machine,
            "runners": runners,
            "checks": ids,
        }));
    }
    if single.is_empty() {
        return None;
    }
    Some(json!({
        "code": "single_machine_environment",
        "environments": single,
        "machines": machines,
        "note": "only one machine serves these environments, so their checks wait while it is busy or stopped: add each with git config --add bay.checkEnvironment <environment> in a runner's clone on another machine, then restart that runner",
    }))
}

#[cfg(test)]
mod tests {
    use super::{age, ended_before};
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// BACKLOG 277b: work that ends during a slow write does not wait for it; the write stays
    /// to be taken later, and one that ended is taken at once.
    #[test]
    fn a_write_that_outlasts_the_work_is_left_running_and_taken_later() {
        let mut started = Some(std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(2));
            json!({"pushed": true})
        }));
        let mut outcome = None;
        let done = AtomicBool::new(false);
        let began = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                done.store(true, Ordering::SeqCst);
            });
            assert!(!ended_before(&mut started, &mut outcome, &done));
        });
        assert!(
            began.elapsed() < Duration::from_secs(1),
            "{:?}",
            began.elapsed()
        );
        assert!(outcome.is_none());
        let write = started.take().expect("the write was dropped");
        assert_eq!(write.join().unwrap()["pushed"], true);

        let mut started = Some(std::thread::spawn(|| json!({"pushed": true})));
        while !started
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ended_before(
            &mut started,
            &mut outcome,
            &AtomicBool::new(true)
        ));
        assert!(started.is_none());
        assert_eq!(outcome.unwrap()["pushed"], true);
    }

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
