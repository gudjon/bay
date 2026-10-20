//! Publication: freeze → merge-tree → commit-tree → check → persist → push → derive
//! (`DESIGN` § 6).
//!
//! Only the trunk push makes a change landed. Before it, everything is retained in the
//! change chain and the evidence notes; after it, "landed" is derived from trunk. Every
//! attempt first scans `base_first..B` for an earlier publication of the change.
//!
//! `bay land` does the agent's part and returns at once; `bay executor run` (or `land --wait`)
//! does the rest. Not yet: signatures (item 11), so the scan matches trailers only; the
//! publication queue (item 19d), so a third trunk move returns `TRUNK_MOVED`; the
//! landed-intent marker (item 19e).

use std::cell::Cell;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::canonical::digest;
use crate::change::{
    Attempt, Chain, ChangeRecord, ChangeState, OpState, Operation, Unavailable, change_ref,
    freeze_draft,
};
use crate::check::{self, Candidate};
use crate::evidence::{self, Combined};
use crate::git::{Git, GitError};
use crate::inputs;
use crate::merge::{Merge, merge_onto};
use crate::policy;
use crate::receipts;
use crate::registry::{KeyClass, REGISTRY_REF, Registry};
use crate::repo::Repo;
use crate::result::{BayResult, Decision, Reason};
use crate::signing::{self, Keys, Signer};
use crate::view;

/// The producer name of the built-in local executor's evidence.
pub const LOCAL_PRODUCER: &str = "local";
/// After this many `TRUNK_MOVED` refusals an operation stops rebuilding.
const TRUNK_MOVED_LIMIT: u32 = 2;

/// The identity bay commits candidates under until executor keys exist (item 11).
const EXECUTOR_IDENTITY: &[(&str, &str)] = &[
    ("GIT_AUTHOR_NAME", "bay executor"),
    ("GIT_AUTHOR_EMAIL", "executor@bay.invalid"),
    ("GIT_COMMITTER_NAME", "bay executor"),
    ("GIT_COMMITTER_EMAIL", "executor@bay.invalid"),
];

/// Whether this build honours the publication test hooks: a build `scripts/release.sh` made
/// (`BAY_RELEASE_VERSION` set at compile time) never does, so a release never pauses or ends
/// itself because of its environment.
const TEST_HOOKS: bool = option_env!("BAY_RELEASE_VERSION").is_none();

/// How long a paused process waits for its release when `BAY_TEST_PAUSE_TIMEOUT_S` does not
/// say: far beyond any test, short enough that a forgotten pause ends.
const PAUSE_TIMEOUT_S: u64 = 600;

/// Ends the process at a named publication boundary when `BAY_TEST_KILL_AT` names it.
/// Used only by the crash tests (G4, G5); a real kill leaves the same state.
fn kill_point(name: &str) {
    if !TEST_HOOKS {
        return;
    }
    if std::env::var("BAY_TEST_KILL_AT").is_ok_and(|at| at == name) {
        std::process::exit(137);
    }
    // `BAY_TEST_PAUSE_AT` holds the process at the boundary until `BAY_TEST_PAUSE_FILE` exists
    // (it writes `<file>.paused` first), for the ownership tests (G24). A pause nobody releases
    // ends the process as a kill would: after `BAY_TEST_PAUSE_TIMEOUT_S`, or once the process
    // that started it is gone, so a test that failed leaves no process behind.
    if std::env::var("BAY_TEST_PAUSE_AT").is_ok_and(|at| at == name)
        && let Ok(file) = std::env::var("BAY_TEST_PAUSE_FILE")
    {
        // Bounded, so a test that dies first never leaves a process held for good.
        let _ = std::fs::write(format!("{file}.paused"), name);
        let limit = std::env::var("BAY_TEST_PAUSE_TIMEOUT_S")
            .ok()
            .and_then(|text| text.parse().ok())
            .unwrap_or(PAUSE_TIMEOUT_S);
        if !released(Path::new(&file), Duration::from_secs(limit), parent_alive()) {
            std::process::exit(137);
        }
    }
}

/// Whether the process that started this one is still there. On Unix an orphan is adopted by
/// another process, so its parent id changes; elsewhere there is no such sign.
fn parent_alive() -> impl FnMut() -> bool {
    #[cfg(unix)]
    {
        let parent = std::os::unix::process::parent_id();
        move || std::os::unix::process::parent_id() == parent
    }
    #[cfg(not(unix))]
    {
        || true
    }
}

/// Waits until `release` exists: true then, false once `limit` passes or `alive` says the
/// waiting has no one to release it.
fn released(release: &Path, limit: Duration, mut alive: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while !release.exists() {
        if Instant::now() >= deadline || !alive() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

#[must_use]
pub fn land(
    dir: &Path,
    change: Option<&str>,
    wait: bool,
    observe: Option<Duration>,
    abandon: bool,
) -> BayResult {
    let repo = match Repo::open_fetched(dir) {
        Ok(repo) => repo,
        Err(error) => return error.into_result(),
    };
    let _ = receipts::sync(&repo);
    let Some(id) = change.map(str::to_owned).or_else(|| repo.view_change(dir)) else {
        return BayResult::failed("run bay land in a change's view, or name the change");
    };
    let step = Cell::new("read");
    let mut result = Err(CHAIN_MOVED.to_owned());
    for _ in 0..3 {
        adopt_remote_chain(&repo, &id);
        if abandon {
            result = abandon_change(&repo, &id, &step);
            break;
        }
        result = match observe.filter(|_| wait) {
            Some(timeout) => observe_land(&repo, dir, &id, &step, timeout),
            None => submit_land(&repo, &id, &step, wait),
        };
        if result.as_ref().is_err_and(|why| why == CHAIN_MOVED) {
            continue;
        }
        break;
    }
    let result = result.unwrap_or_else(BayResult::failed);
    let mut result = with_receipt(&repo, &id, step.get(), result);
    // G80: every result says which trunk it reflects, and when that trunk was fetched.
    result.freshness = result.freshness.or_else(|| repo.freshness());
    result
}

/// `land --wait` with an observation timeout (§ 9a): the run happens in a detached process,
/// and the caller watches the change chain for at most `timeout`. When the timeout expires the
/// caller returns `queued`/`running` and the run continues on its own.
fn observe_land(
    repo: &Repo,
    dir: &Path,
    id: &str,
    step: &Cell<&'static str>,
    timeout: Duration,
) -> Result<BayResult, String> {
    let submitted = submit_land(repo, id, step, false)?;
    if submitted.decision != Decision::Queued
        || submitted.waiting_for.as_deref() != Some("executor")
    {
        return Ok(submitted);
    }
    let op_id = submitted.op_id.clone().unwrap_or_default();
    let bay = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut run = std::process::Command::new(bay);
    run.arg("-C")
        .arg(dir)
        .args(["land", "--wait", "--change", id])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // On Windows the run is a detached process, so it outlives a caller that closes its console.
    crate::platform::detach_windows(&mut run);
    run.spawn()
        .map_err(|error| format!("the run could not be started: {error}"))?;
    let started = Instant::now();
    let chain = Chain::new(&repo.git);
    while started.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(200));
        let Ok((_, record)) = chain.read(id) else {
            continue;
        };
        let done = record
            .operations
            .iter()
            .find(|op| op.op_id == op_id)
            .is_some_and(|op| op.state.is_terminal() || op.state == OpState::Blocked);
        if done {
            // The outcome is in the chain; reading it again returns the same result the run did.
            return submit_land(repo, id, step, false);
        }
    }
    let mut result = BayResult::new(Decision::Queued)
        .with_change(id)
        .with_op(op_id);
    result.state = Some("running".to_owned());
    result.published = Some(false);
    result.waiting_for = Some("executor".to_owned());
    Ok(result.with_detail(json!({
        "observation_timeout_s": timeout.as_secs(),
        "the run continues": true,
    })))
}

/// Writes the receipt for a failed or blocked result before it is returned (§ 9): outside
/// the view, then synchronized when the remote takes it.
#[must_use]
pub fn with_receipt(repo: &Repo, id: &str, step: &str, mut result: BayResult) -> BayResult {
    if result.change.is_none() {
        result = result.with_change(id);
    }
    if matches!(result.decision, Decision::Failed | Decision::Blocked) {
        let actor = Chain::new(&repo.git)
            .read(id)
            .ok()
            .map(|(_, record)| record.actor);
        let receipt = receipts::from_result(id, actor, step, &result);
        match receipts::write_pending(repo, &receipt) {
            Ok(_) => {
                result.receipt = Some(receipt.id);
                let _ = receipts::sync(repo);
            }
            Err(error) => {
                result.receipt = Some(format!("not written: {error}"));
            }
        }
    }
    result
}

/// A commit on trunk that carries the change's `Change-Id`.
pub struct Found {
    pub commit: String,
    pub revision: String,
}

/// What a scan found: verified publications, and commits that only carry the trailer.
#[derive(Default)]
pub struct Scan {
    pub publications: Vec<Found>,
    /// Unsigned, unverifiable or wrongly signed commits that quote the `Change-Id`: reported,
    /// never counted as a landing (§ 6).
    pub anomalies: Vec<Found>,
}

/// Scans `base_first..trunk` for publication commits of `id` (§ 6). With a registry, a
/// commit counts only when its signature verifies against the executor file of the registry
/// version its `Registry-Id` names, under the executor that committed it; without one, bay
/// runs unsigned and trailers decide.
///
/// # Errors
///
/// A git fault.
pub fn scan(git: &Git, base_first: &str, trunk: &str, id: &str) -> Result<Scan, String> {
    let range = format!("{base_first}..{trunk}");
    let log = git
        .run(&[
            "log",
            "--format=%H%x1f%(trailers:key=Change-Id,valueonly,separator=%x2c)%x1f%(trailers:key=Revision-Id,valueonly,separator=%x2c)%x1f%(trailers:key=Registry-Id,valueonly,separator=%x2c)%x1f%cn%x1e",
            &range,
        ])
        .map_err(|error| error.message())?;
    let signatures = git
        .ref_target(REGISTRY_REF)
        .map_err(|error| error.message())?
        .is_some();
    let mut scan = Scan::default();
    for entry in log.split('\u{1e}') {
        let fields: Vec<&str> = entry.trim().split('\u{1f}').map(str::trim).collect();
        let [commit, change, revision, registry_id, committer] = fields.as_slice() else {
            continue;
        };
        if commit.is_empty() || *change != id {
            continue;
        }
        let found = Found {
            commit: (*commit).to_owned(),
            revision: (*revision).to_owned(),
        };
        if !signatures || verify_publication(git, commit, registry_id, committer) {
            scan.publications.push(found);
        } else {
            scan.anomalies.push(found);
        }
    }
    Ok(scan)
}

/// Whether `commit` verifies against the executor file of the registry version `registry_id`
/// names, under the executor that committed it.
#[must_use]
pub fn verify_publication(git: &Git, commit: &str, registry_id: &str, committer: &str) -> bool {
    if registry_id.is_empty() || registry_id == "none" {
        return false;
    }
    Registry::load_at(git, registry_id)
        .is_ok_and(|registry| verifies_under(git, commit, &registry, committer))
}

/// Whether `commit` verifies against `registry`'s executor file, under the executor that
/// committed it.
#[must_use]
pub fn verifies_under(git: &Git, commit: &str, registry: &Registry, committer: &str) -> bool {
    let Ok(executors) = registry.class_file(git, KeyClass::Executor) else {
        return false;
    };
    matches!(signing::verify_commit(git, commit, &executors), Ok(Some(principal)) if principal == committer)
}

struct Ctx<'a> {
    repo: &'a Repo,
    /// The publication step in progress, named in a receipt if it fails.
    step: &'a Cell<&'static str>,
    chain: Chain<'a>,
    id: String,
    head: String,
    record: ChangeRecord,
    /// The current registry after the last fetch; `None` means bay runs unsigned.
    registry: Option<Registry>,
    keys: Keys,
    /// The chain head this process last pushed: later pushes are leased on it, so a chain
    /// another executor moved is never overwritten.
    pushed_head: Option<String>,
    /// The epoch this process claimed, when it acts as the executor.
    my_epoch: u64,
    /// The registry principal that signed the request, when it is a policy repair (§ 7).
    repair: Option<String>,
    /// Whether the remote answered this run's fetch.
    online: bool,
    /// The chain head the last heartbeat pushed, while a check runs (§ 9a).
    beat_pushed: std::cell::RefCell<Option<String>>,
    /// Events this run wrote mid-operation, told to the notify adapter only once the operation
    /// reached its outcome, so a slow adapter never holds an operation past its lease.
    deferred: std::cell::RefCell<Vec<serde_json::Value>>,
    /// Uncounted retries this process made, whatever the chain on the remote says: a remote
    /// that takes no write (a held reftable lock) never records them, and a retry it never
    /// records must still exhaust (G128).
    retries_here: u32,
}

impl<'a> Ctx<'a> {
    fn load(repo: &'a Repo, step: &'a Cell<&'static str>, id: &str) -> Result<Self, String> {
        let keys = Keys::load(&repo.git);
        keys.validate()?;
        let chain = Chain::new(&repo.git);
        let (head, record) = chain.read(id).map_err(|error| error.message())?;
        Ok(Self {
            repo,
            step,
            chain,
            id: id.to_owned(),
            head,
            record,
            registry: None,
            keys,
            pushed_head: None,
            my_epoch: 0,
            repair: None,
            online: true,
            beat_pushed: std::cell::RefCell::new(None),
            deferred: std::cell::RefCell::new(Vec::new()),
            retries_here: 0,
        })
    }

    /// Tells the notify adapter, once each, of the events this run deferred (ruling R6).
    fn tell_deferred(&self) {
        for event in self.deferred.take() {
            let _ = crate::notify::notify(self.repo, &event);
        }
    }

    /// The agent's signer: its key under the change's actor name. Required with a registry.
    fn agent_signer(&self) -> Result<Option<Signer>, String> {
        if self.registry.is_none() {
            return Ok(None);
        }
        let key = self.keys.agent.clone().ok_or(
            "the repository has a key registry and this machine has no agent key (git config bay.agentKey); requests must be signed",
        )?;
        Ok(Some(Signer {
            key,
            name: self.record.actor.name.clone(),
        }))
    }

    /// The executor's signer. Required with a registry.
    fn executor_signer(&self) -> Result<Option<Signer>, String> {
        if self.registry.is_none() {
            return Ok(None);
        }
        self.keys.executor.clone().map(Some).ok_or_else(|| {
            "the repository has a key registry and this machine has no executor key (git config bay.executorKey)".to_owned()
        })
    }

    /// The producer name evidence is recorded under, and its signer when signing is on.
    fn producer(&self) -> Result<(String, Option<Signer>), String> {
        let name = self.keys.producer_name.clone();
        if self.registry.is_none() {
            return Ok((name, None));
        }
        let signer = self.keys.producer.clone().ok_or(
            "the repository has a key registry and this machine has no producer key (git config bay.producerKey)",
        )?;
        Ok((name, Some(signer)))
    }

    /// Fetches trunk and refuses a remote rewound behind what this change has seen.
    fn fetch_trunk(&mut self) -> Result<Result<String, BayResult>, String> {
        self.step.set("fetch");
        let trunk = self.repo.fetch_trunk().map_err(|error| error.message())?;
        self.repo.fetch_registry();
        self.registry = Registry::current(&self.repo.git)?;
        if !is_ancestor(&self.repo.git, &self.record.observed_remote, &trunk)? {
            return Ok(Err(BayResult::failed(format!(
                "trunk was rewound: {} is no longer an ancestor of the remote trunk {trunk}; bay never forces",
                self.record.observed_remote
            ))
            .with_change(self.id.clone())));
        }
        self.record.observed_remote.clone_from(&trunk);
        Ok(Ok(trunk))
    }

    fn op_mut(&mut self, op_id: &str) -> &mut Operation {
        self.record
            .operations
            .iter_mut()
            .find(|op| op.op_id == op_id)
            .unwrap_or_else(|| unreachable!("operation {op_id} is in the record"))
    }

    fn op(&self, op_id: &str) -> &Operation {
        self.record
            .operations
            .iter()
            .find(|op| op.op_id == op_id)
            .unwrap_or_else(|| unreachable!("operation {op_id} is in the record"))
    }

    fn save(&mut self, draft: &str, candidate: Option<&str>, message: &str) -> Result<(), String> {
        self.head = self
            .chain
            .append_linking(&self.head, &self.record, draft, candidate, message)
            .map_err(|error| error.message())?;
        Ok(())
    }

    /// Pushes the chain. It only appends, so this is a fast-forward and never forced; once this
    /// process has pushed, the push is leased on that head, so it fails when another executor
    /// moved the chain since.
    fn push_chain(&mut self) -> bool {
        let name = change_ref(&self.id);
        let lease = self
            .pushed_head
            .as_ref()
            .map(|expected| format!("--force-with-lease={name}:{expected}"));
        let mut args = vec!["push", "-q"];
        if let Some(lease) = &lease {
            args.push(lease);
        }
        let refspec = format!("{name}:{name}");
        args.extend([self.repo.remote(), refspec.as_str()]);
        let pushed = self.repo.git.run(&args).is_ok();
        if pushed {
            self.pushed_head = Some(self.head.clone());
        }
        pushed
    }

    /// Pushes the chain and moves `refs/bay/requests/<id>` to `request` in one atomic push
    /// (§ 10a). The requests ref always names the latest land request.
    fn push_chain_and_request(&mut self, request: &str) -> bool {
        let name = change_ref(&self.id);
        let requests = format!("{}{}", crate::names::REQUESTS, self.id);
        let _ = self.repo.git.run(&["update-ref", &requests, request]);
        let lease = self
            .pushed_head
            .as_ref()
            .map(|expected| format!("--force-with-lease={name}:{expected}"));
        let mut args = vec!["push", "-q", "--atomic"];
        if let Some(lease) = &lease {
            args.push(lease);
        }
        let chain = format!("{name}:{name}");
        let request_spec = format!("+{requests}:{requests}");
        args.extend([self.repo.remote(), chain.as_str(), request_spec.as_str()]);
        let pushed = self.repo.git.run(&args).is_ok();
        if pushed {
            self.pushed_head = Some(self.head.clone());
        }
        pushed
    }

    /// Replaces the local chain with the remote's and reloads it.
    fn refetch(&mut self) -> Result<(), String> {
        let name = change_ref(&self.id);
        self.repo
            .git
            .run(&[
                "fetch",
                "-q",
                "--no-tags",
                self.repo.remote(),
                &format!("+{name}:{name}"),
            ])
            .map_err(|error| error.message())?;
        let (head, record) = self.chain.read(&self.id).map_err(|error| error.message())?;
        self.head = head;
        self.record = record;
        self.pushed_head = Some(self.head.clone());
        Ok(())
    }

    fn executor_name(&self) -> String {
        self.keys.executor_name.clone()
    }
}

/// The agent's part of `land` (§ 9a): freeze, scan, and append the request. Returns at once
/// with the operation id; with `wait` it also runs the executor's part inline.
fn submit_land(
    repo: &Repo,
    id: &str,
    step: &Cell<&'static str>,
    wait: bool,
) -> Result<BayResult, String> {
    let mut ctx = Ctx::load(repo, step, id)?;
    // § 10a offline work: with the remote unreachable, the request is committed locally and
    // pushed by a later bay command when the remote returns.
    let (trunk, online) = match ctx.fetch_trunk() {
        Ok(Ok(trunk)) => (trunk, true),
        Ok(Err(refusal)) => return Ok(refusal),
        Err(why) => match repo.git.ref_target(&repo.trunk_tracking_ref()) {
            Ok(Some(trunk)) => {
                ctx.registry = Registry::current(&repo.git)?;
                ctx.online = false;
                (trunk, false)
            }
            _ => return Err(why),
        },
    };
    // An actor whose change was adopted away learns it here (§ 4, the `adopted` event).
    let me = crate::change::current_actor()?;
    if me.name != ctx.record.actor.name
        && ctx
            .record
            .adopted_from
            .iter()
            .any(|actor| actor.name == me.name)
    {
        return Ok(BayResult::blocked(Reason::ChangeSuperseded)
            .with_change(id)
            .with_detail(json!({"event": "adopted", "adopted_by": ctx.record.actor})));
    }
    let agent = ctx.agent_signer()?;
    ctx.chain.sign_with(agent);

    step.set("freeze");
    let revision = match freeze(&ctx)? {
        Ok(revision) => revision,
        Err(refusal) => return Ok(refusal),
    };
    observe_inputs(&mut ctx)?;

    if let Some(result) = scanned(&mut ctx, &trunk, &revision)? {
        return Ok(result);
    }

    let op_id = match ctx
        .record
        .operations
        .iter()
        .find(|op| op.revision == revision && !op.state.is_terminal())
    {
        Some(op) => op.op_id.clone(),
        None => submit(&mut ctx, &revision)?,
    };
    if ctx.op(&op_id).state == OpState::Blocked
        && let Some(refusal) = retry_blocked(&mut ctx, &op_id, &revision, &trunk)?
    {
        return Ok(refusal);
    }
    // § 8 at admit: a change that has not observed what trunk requires is refused now, with
    // the outcome on its operation, never queued for a refusal no one is there to read. An
    // operation an executor already took is the executor's to judge.
    if ctx.op(&op_id).state == OpState::Submitted
        && let Err(refusal) = input_gate(&mut ctx, &op_id, &revision, &trunk)?
    {
        return Ok(refusal);
    }
    kill_point("after-freeze");

    if wait && online {
        let result = execute_op(&mut ctx, &op_id, trunk);
        ctx.tell_deferred();
        return result;
    }
    let op = ctx.op(&op_id);
    let mut result = BayResult::new(Decision::Queued)
        .with_change(id)
        .with_op(op_id.clone());
    result.state = serde_json::to_value(op.state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned));
    result.published = Some(false);
    result.waiting_for = Some(if online { "executor" } else { "remote" }.to_owned());
    Ok(with_progress(&ctx.repo.git, ctx.op(&op_id), result))
}

/// How far operation `op` has come (G80, `INTERFACES` § 1): seconds since its request was
/// committed, the evidence records on its candidates so far, and the state it moves to next.
/// The time is the request commit's own; `None` when that commit cannot be read.
#[must_use]
pub fn progress(git: &Git, op: &Operation) -> (Option<u64>, Vec<String>, Option<&'static str>) {
    let elapsed = op
        .request_id
        .as_ref()
        .and_then(|request| git.run(&["log", "-1", "--format=%ct", request]).ok())
        .and_then(|at| at.trim().parse::<u64>().ok())
        .and_then(|at| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|now| now.as_secs().saturating_sub(at))
        });
    let receipts = op
        .attempts
        .iter()
        .filter_map(|attempt| {
            evidence::records_for(git, &attempt.tree)
                .ok()
                .map(|records| (attempt, records))
        })
        .flat_map(|(attempt, records)| {
            records
                .into_iter()
                .filter(|record| record.subject.commit == attempt.commit)
                .map(|record| record.id)
                .collect::<Vec<_>>()
        })
        .collect();
    (elapsed, receipts, op.state.next())
}

/// Sets `result`'s progress fields from operation `op` (G80).
fn with_progress(git: &Git, op: &Operation, mut result: BayResult) -> BayResult {
    let (elapsed, receipts, next) = progress(git, op);
    result.elapsed_s = elapsed;
    result.receipts = Some(receipts);
    result.next = next;
    result
}

/// The executor's part: runs operation `op_id` of change `id` to a stated outcome.
///
/// # Errors
///
/// A fault of bay's own; refusals are results.
pub fn execute(
    repo: &Repo,
    id: &str,
    op_id: &str,
    step: &Cell<&'static str>,
) -> Result<BayResult, String> {
    let mut ctx = Ctx::load(repo, step, id)?;
    if !ctx.record.operations.iter().any(|op| op.op_id == op_id) {
        if ctx.record.retention_cut {
            // § 9a: a retry of an op id whose record expired is ambiguous, never new work.
            return Ok(BayResult::failed(format!(
                "operation {op_id} expired with change {id}'s retention; a retry of it is refused as ambiguous"
            ))
            .with_change(id)
            .with_op(op_id));
        }
        return Err(format!("change {id} has no operation {op_id}"));
    }
    let trunk = match ctx.fetch_trunk()? {
        Ok(trunk) => trunk,
        Err(refusal) => return Ok(refusal),
    };
    let revision = ctx.op(op_id).revision.clone();
    if let Some(result) = scanned(&mut ctx, &trunk, &revision)? {
        return Ok(existing(result));
    }
    let result = execute_op(&mut ctx, op_id, trunk);
    ctx.tell_deferred();
    let result = result?;
    kill_point("before-outcome-event");
    // A log that cannot take the event now leaves the blocked operation on the chain, and the
    // next executor pass writes the event the logs lack (`recover_events`).
    if let Ok(Some(event)) = outcome_event(&ctx, op_id, &result) {
        // A lost notice is never retried: `bay watch` reads the event from the log.
        let _ = crate::notify::notify(ctx.repo, &event);
    }
    Ok(result)
}

/// `INTERFACES` § 2, outcomes that arrive after the agent left: a blocked outcome the executor
/// reached, which waits for the actor, is an event for the change's actor on this executor's
/// log. Returns the event when it was new, for the caller to tell the notify adapter.
fn outcome_event(
    ctx: &Ctx,
    op_id: &str,
    result: &BayResult,
) -> Result<Option<serde_json::Value>, String> {
    let Some(reason) = result.reason else {
        return Ok(None);
    };
    let waits = ctx
        .record
        .operations
        .iter()
        .find(|op| op.op_id == op_id)
        .is_some_and(|op| op.state == OpState::Blocked || op.state.is_terminal());
    if result.decision != Decision::Blocked || !waits {
        return Ok(None);
    }
    let detail = result.detail.clone().unwrap_or(serde_json::Value::Null);
    let event = blocked_event(&ctx.record, op_id, reason, &detail)?;
    let executor = ctx.executor_name();
    let signer = ctx.executor_signer().ok().flatten();
    let new = crate::log::append_event(ctx.repo, &executor, signer.as_ref(), &event)?;
    Ok(new.then_some(event))
}

/// The event of operation `op_id` of `record` blocked on `reason`: kind `check_failed`,
/// `check_unknown`, else `refused`. Its id is a digest of the operation, the reason and the
/// candidate, so one outcome is one event however often, and by whom, it is written.
fn blocked_event(
    record: &ChangeRecord,
    op_id: &str,
    reason: Reason,
    detail: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let kind = match reason {
        Reason::CheckFailed => "check_failed",
        Reason::CheckUnknown => "check_unknown",
        _ => "refused",
    };
    let candidate = record
        .operations
        .iter()
        .find(|op| op.op_id == op_id)
        .and_then(|op| op.attempts.last())
        .map(|attempt| attempt.candidate_id.clone());
    // The cause a reader counts recurrences by: the input, else the first failed check.
    let cause = detail["input"]
        .as_str()
        .or_else(|| detail["check"].as_str())
        .or_else(|| detail["checks"][0]["check"].as_str());
    let recurrence_key = match cause {
        Some(cause) => format!("{}/{}/{cause}", record.id, reason.code()),
        None => format!("{}/{}", record.id, reason.code()),
    };
    let id = digest(&json!({"op_id": op_id, "reason": reason.code(), "candidate": candidate}))?;
    let inputs: Vec<String> = detail["inputs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|input| {
            Some(format!(
                "{}@{}",
                input["input"].as_str()?,
                input["required"].as_str()?
            ))
        })
        .collect();
    Ok(json!({
        "schema": crate::log::EVENT_SCHEMA,
        "id": format!("evt_{}", &id.trim_start_matches("sha256:")[..20]),
        "at": crate::clock::now_rfc3339(),
        "kind": kind,
        "change": record.id,
        "op_id": op_id,
        "candidate": candidate,
        "reason": reason.code(),
        "actor": record.actor,
        "inputs": inputs,
        "detail": detail,
        "recurrence_key": recurrence_key,
    }))
}

/// The executor's repair of a lost event (`INTERFACES` § 2): the chain's move to blocked and
/// the log append are two writes, so an executor that dies or loses its log between them
/// leaves a blocked operation with no event. For every blocked operation of `records` whose
/// event no log holds, the event is written now from the chain (its `detail` is then null),
/// and the notify adapter is told once. Returns how many were written. The cost is one
/// `git log` over the executor logs, and one append per event written.
///
/// # Errors
///
/// A git fault, or a key registry with no executor key on this machine.
pub fn recover_events(repo: &Repo, records: &[ChangeRecord]) -> Result<usize, String> {
    let blocked: Vec<(&ChangeRecord, &str, Reason)> = records
        .iter()
        .flat_map(|record| {
            record
                .operations
                .iter()
                .filter(|op| op.state == OpState::Blocked)
                .filter_map(move |op| {
                    let reason = Reason::from_code(op.outcome.as_deref()?)?;
                    Some((record, op.op_id.as_str(), reason))
                })
        })
        .collect();
    if blocked.is_empty() {
        return Ok(0);
    }
    crate::log::fetch(repo);
    let held = crate::log::held_events(&repo.git)?;
    let keys = Keys::load(&repo.git);
    let signer = match Registry::current(&repo.git)? {
        None => None,
        Some(_) => Some(keys.executor.clone().ok_or(
            "the repository has a key registry and this machine has no executor key (git config bay.executorKey)",
        )?),
    };
    let mut written = 0;
    for (record, op_id, reason) in blocked {
        let event = blocked_event(record, op_id, reason, &serde_json::Value::Null)?;
        if held.contains(event["id"].as_str().unwrap_or_default()) {
            continue;
        }
        crate::log::append(repo, &keys.executor_name, signer.as_ref(), &event)?;
        let _ = crate::notify::notify(repo, &event);
        written += 1;
    }
    Ok(written)
}

/// Steps 2–7 for one operation, rebuilding when trunk moves.
fn execute_op(ctx: &mut Ctx, op_id: &str, trunk: String) -> Result<BayResult, String> {
    let revision = ctx.op(op_id).revision.clone();
    let executor = ctx.executor_signer()?;
    ctx.chain.sign_with(executor);
    if let Some(refusal) = verify_request(ctx, op_id)? {
        return Ok(refusal);
    }
    if let Some(waiting) = waits_for_worker(ctx, op_id) {
        return Ok(waiting);
    }
    // § 6: while a queue is live only its head runs; anyone else holds a place and no check.
    let mut seq = match queue_gate(ctx, op_id)? {
        Gate::Wait(result) => return Ok(*result),
        Gate::Head(seq) => Some(seq),
        Gate::Free => None,
    };
    if let Some(report) = claim(ctx, op_id, &revision, &trunk)? {
        return Ok(report);
    }
    // G173: whatever the outcome, the operation's anchor goes with it.
    let repo = ctx.repo;
    let _anchored = Anchored::new(&repo.git, op_id)?;
    let outcome = run_attempts(ctx, op_id, &revision, trunk, &mut seq);
    // The head always releases: an outcome other than its own publication (which released in
    // the publishing push) passes the head on. Not an operation given up on `REGISTRY_CHANGED`:
    // its key can sign no release, and the place stays with the operation, which the executor
    // that takes it over publishes as the head (§ 10a).
    let given_up = outcome
        .as_ref()
        .is_ok_and(|result| result.reason == Some(Reason::RegistryChanged));
    if let Some(at) = seq
        && !given_up
        && !crate::queue::Queue::read(ctx.repo)?.released.contains(&at)
    {
        let reason = match &outcome {
            Ok(result) => result.reason.map_or_else(
                || {
                    if result.decision == Decision::Landed {
                        "published".to_owned()
                    } else if result.state.as_deref() == Some(NO_EFFECT) {
                        NO_EFFECT.to_owned()
                    } else {
                        "failed".to_owned()
                    }
                },
                |reason| {
                    // § 6: a head that could not start its check is released as `unavailable`,
                    // which counts toward no limit.
                    if reason == Reason::CheckUnavailable {
                        crate::queue::UNAVAILABLE.to_owned()
                    } else {
                        reason.code().to_lowercase()
                    }
                },
            ),
            Err(_) => "failed".to_owned(),
        };
        let signer = ctx.executor_signer().ok().flatten();
        let _ = crate::queue::release(ctx.repo, signer.as_ref(), at, op_id, &reason);
    }
    outcome
}

/// Steps 2–7 for one operation, rebuilding when trunk moves; joins the queue after two
/// `TRUNK_MOVED` refusals. `seq` is this operation's place when it is the queue's head.
fn run_attempts(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    mut trunk: String,
    seq: &mut Option<u64>,
) -> Result<BayResult, String> {
    let revision = revision.to_owned();
    let id = ctx.id.clone();
    loop {
        // A candidate checked and persisted on this very trunk is published without a rebuild.
        let ready = ctx.op(op_id).attempts.last().filter(|attempt| {
            ctx.op(op_id).state == OpState::Checked
                && attempt.base == trunk
                && attempt.epoch == ctx.my_epoch
        });
        let attempt = match ready {
            Some(attempt) => attempt.clone(),
            None => match build_and_check(ctx, op_id, &revision, &trunk)? {
                Ok(attempt) => attempt,
                Err(refusal) => return Ok(refusal),
            },
        };

        ctx.step.set("publish");
        if let Some(report) = still_owner(ctx, op_id, &revision)? {
            return Ok(report);
        }
        kill_point("before-publish");
        let release = match *seq {
            Some(at) => {
                let signer = ctx.executor_signer().ok().flatten();
                Some((
                    at,
                    crate::queue::release_commit(
                        &ctx.repo.git,
                        signer.as_ref(),
                        at,
                        op_id,
                        "published",
                    )?,
                ))
            }
            None => None,
        };
        let claim = ctx.record.claim.clone();
        match publish_push(
            ctx.repo,
            &id,
            &attempt.commit,
            release.as_ref(),
            claim.as_ref(),
        ) {
            Ok(()) => {
                kill_point("after-publish");
                return derive_landed(ctx, op_id, &revision, &attempt.commit);
            }
            Err(error) => {
                if let Some(result) =
                    after_refused_push(ctx, op_id, &revision, &mut trunk, seq, &attempt, &error)?
                {
                    return Ok(result);
                }
            }
        }
    }
}

/// A publishing push was refused (§ 6): reports the existing landing, retries the same
/// candidate after a hook timeout, rejoins the queue, or moves to the new trunk (`None`: build
/// and check again).
fn after_refused_push(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    trunk: &mut String,
    seq: &mut Option<u64>,
    attempt: &Attempt,
    error: &str,
) -> Result<Option<BayResult>, String> {
    let now = match ctx.repo.fetch_trunk() {
        Ok(now) => now,
        Err(fetch) => {
            return outcome_unknown(ctx, op_id, revision, attempt, error, &fetch.message());
        }
    };
    if let Some(result) = scanned(ctx, &now, revision)? {
        return Ok(Some(existing(result)));
    }
    // Another change landed this generation of the intent: never a retry (§ 6).
    // The ref or its shard index record (§ 10a); an unread remote is an error, never "absent".
    if let Some(claim) = &ctx.record.claim
        && crate::compact::remote_marker(ctx.repo, &claim.landed_ref())?.is_some()
    {
        return Ok(Some(
            BayResult::blocked(Reason::ChangeAlreadyLanded)
                .with_change(ctx.id.clone())
                .with_op(op_id)
                .with_detail(json!({"landed_intent": claim.landed_ref()})),
        ));
    }
    // A cancel or a newer request that the hook's own transaction saw: the refusal is the
    // stated answer, never `failed` (G33).
    if let Some(report) = still_owner(ctx, op_id, revision)? {
        return Ok(Some(report));
    }
    if error.contains(Reason::RegistryChanged.code()) {
        let registry = match registry_after_change(ctx, op_id, attempt)? {
            Ok(registry) => registry,
            Err(given_up) => return Ok(Some(given_up)),
        };
        if now == *trunk {
            return registry_changed(ctx, op_id, revision, attempt, &registry);
        }
        // Trunk moved as well: C can no longer be re-signed on its parent. The move is the
        // refusal that counts (`TRUNK_MOVED`, § 6), and the rebuild signs under the current
        // registry.
    }
    if error.contains(Reason::HookTimeout.code()) {
        return hook_timed_out(ctx, op_id, revision);
    }
    // A release created while this head was publishing (its progress deadline), whether or not
    // trunk moved: rejoin, uncounted toward `QUEUE_DEADLINE_EXCEEDED`, one publish retry (§ 6).
    // A release for any other reason is no publish retry and no reason to rejoin: a newer
    // revision, a cancel or another executor's outcome ended this head's turn, and the chain
    // says which on the next pass.
    let reason = match *seq {
        Some(at) => crate::queue::Queue::read(ctx.repo)?
            .release_reasons
            .get(&at)
            .cloned(),
        None => None,
    };
    if let Some(reason) = reason.as_ref().filter(|reason| *reason != "deadline") {
        *seq = None;
        return Ok(Some(
            BayResult::failed(format!(
                "the publishing push was refused: this head's queue place was released ({reason}) while it published; nothing was published"
            ))
            .with_change(ctx.id.clone())
            .with_op(op_id),
        ));
    }
    if let Some(at) = *seq
        && reason.is_some()
    {
        *seq = None;
        // Saved in the chain with the retry it counts, so a later pass never counts this place
        // toward `QUEUE_DEADLINE_EXCEEDED`.
        ctx.op_mut(op_id).mid_publish_releases.push(at);
        if let Some(blocked) =
            uncounted_retry(ctx, op_id, revision, "deadline release mid-publish")?
        {
            return Ok(Some(blocked));
        }
        return enqueue(ctx, op_id, revision, Reason::PublicationQueued).map(Some);
    }
    // G128: another process on the remote holds a ref lock. Trunk did not move, so the same
    // candidate is pushed again after a short wait, uncounted; bay never unlinks the lock.
    if now == *trunk && held_lock(error) {
        return lock_held(ctx, op_id, revision);
    }
    if now == *trunk {
        return Ok(Some(
            BayResult::failed(format!("the publishing push was refused: {error}"))
                .with_change(ctx.id.clone())
                .with_op(op_id),
        ));
    }
    let held = error.contains(Reason::QueueHeld.code());
    *trunk = now;
    ctx.record.observed_remote.clone_from(trunk);
    let moved = Moved {
        base: &attempt.base,
        trunk,
        held,
    };
    after_trunk_moved(ctx, op_id, revision, seq, &moved)
}

/// The hook refused C with `REGISTRY_CHANGED` (`DESIGN` § 10a): a key behind C (its executor
/// key, or a producer key behind its evidence) was revoked after C was signed. Re-reads the
/// registry and this box's keys. Returns the current registry when this executor's key is valid
/// in it, and its chain is signed with that key from now on. Otherwise returns the answer that
/// gives the operation up (queued `REGISTRY_CHANGED`), never `failed`:
/// - this box's executor key is the revoked one: it writes nothing more, since the hook takes
///   no transition it signs. The revocation itself releases the operation: an executor with a
///   valid key takes it over at once, without waiting out the lease ([`holder_revoked`], § 9a);
/// - the registry read back is still the one C names (the fetch failed): no newer registry is
///   in sight to act on, so nothing is re-signed (never a loop of re-signs), and the operation
///   waits for the registry. A later pass reads it again.
fn registry_after_change(
    ctx: &mut Ctx,
    op_id: &str,
    attempt: &Attempt,
) -> Result<Result<Registry, BayResult>, String> {
    ctx.repo.fetch_registry();
    ctx.registry = Registry::current(&ctx.repo.git)?;
    let signed_under = ctx
        .repo
        .git
        .run(&[
            "log",
            "-1",
            "--format=%(trailers:key=Registry-Id,valueonly,separator=%x2c)",
            &attempt.commit,
        ])
        .map_err(|error| error.message())?;
    let given_up = |registry: &str, released: bool, waiting_for: &str| {
        let mut result = BayResult::refused(Decision::Queued, Reason::RegistryChanged)
            .with_change(ctx.id.clone())
            .with_op(op_id)
            .with_detail(json!({
                "candidate": attempt.candidate_id,
                "signed_under": signed_under,
                "registry": registry,
                "released": released,
            }));
        result.published = Some(false);
        result.waiting_for = Some(waiting_for.to_owned());
        result
    };
    let registry = match &ctx.registry {
        Some(registry) if registry.commit != signed_under => registry.clone(),
        _ => return Ok(Err(given_up(&signed_under, false, "registry"))),
    };
    // The box may hold a new key for the same principal by now.
    ctx.keys = Keys::load(&ctx.repo.git);
    if !executor_key_valid(ctx, &registry)? {
        return Ok(Err(given_up(&registry.commit, true, "executor_capacity")));
    }
    let signer = ctx.executor_signer()?;
    ctx.chain.sign_with(signer);
    Ok(Ok(registry))
}

/// `REGISTRY_CHANGED` on an unmoved trunk, with a valid key: re-sign a C with the same tree and
/// parent and the current `Registry-Id` (`None`: build again, where tree-keyed evidence is
/// reused, commit-keyed checks run again, and evidence whose producer key the registry revoked
/// no longer counts, so it is rechecked first). Not counted toward `PUBLISH_RETRY_EXHAUSTED`.
fn registry_changed(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    attempt: &Attempt,
    registry: &Registry,
) -> Result<Option<BayResult>, String> {
    ctx.op_mut(op_id).state = OpState::Running;
    ctx.save(
        revision,
        None,
        &format!(
            "{op_id}: REGISTRY_CHANGED, re-sign {} under registry {}",
            attempt.candidate_id, registry.commit
        ),
    )?;
    if !ctx.push_chain() {
        return Err("the change chain could not be pushed; nothing was published".to_owned());
    }
    Ok(None)
}

/// § 10a: whether the operation's holder signed its last chain commit with an executor key the
/// current registry no longer holds. That key can write nothing more (no heartbeat, no
/// release), so its lease is not waited out: the revocation released the operation.
fn holder_revoked(ctx: &Ctx, op_id: &str) -> Result<bool, String> {
    let (Some(registry), Some(owner)) = (&ctx.registry, &ctx.op(op_id).owner) else {
        return Ok(false);
    };
    let committer = ctx
        .repo
        .git
        .run(&["log", "-1", "--format=%cn", &ctx.head])
        .map_err(|error| error.message())?;
    Ok(committer == *owner && !verifies_under(&ctx.repo.git, &ctx.head, registry, owner))
}

/// Whether this machine's executor key is enrolled for its executor name in `registry`.
fn executor_key_valid(ctx: &Ctx, registry: &Registry) -> Result<bool, String> {
    let Some(signer) = &ctx.keys.executor else {
        return Ok(false);
    };
    let (key_type, blob) = signing::public_key(&signer.key)?;
    Ok(registry.holds(KeyClass::Executor, &signer.name, &key_type, &blob))
}

/// After this many uncounted publish retries an operation is `PUBLISH_RETRY_EXHAUSTED` (§ 6).
const PUBLISH_RETRY_LIMIT: u32 = 5;
/// The short wait before a publishing push the hook timed out is tried again.
const HOOK_RETRY_WAIT: Duration = Duration::from_secs(1);

/// The trunk hook timed out on the publishing push: retry the same candidate after a short wait
/// (`None`), uncounted toward `TRUNK_MOVED` and the queue's deadline (`INTERFACES` § 2).
fn hook_timed_out(ctx: &mut Ctx, op_id: &str, revision: &str) -> Result<Option<BayResult>, String> {
    if let Some(blocked) = uncounted_retry(ctx, op_id, revision, "hook timeout")? {
        return Ok(Some(blocked));
    }
    std::thread::sleep(HOOK_RETRY_WAIT);
    Ok(None)
}

/// G81: the publishing push failed and trunk could not then be read, so whether C landed is
/// unknown: the reply may have been lost after the remote moved trunk. bay acts on nothing and
/// says so; the change's next land reads trunk again and finds the landing or builds anew,
/// never a second publication (`DESIGN` § 9).
fn outcome_unknown(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    attempt: &Attempt,
    push: &str,
    fetch: &str,
) -> Result<Option<BayResult>, String> {
    let op = ctx.op_mut(op_id);
    op.state = OpState::Blocked;
    op.outcome = Some(Reason::PublishOutcomeUnknown.code().to_owned());
    // The chain may not reach the remote either; it is saved here, and the next pass reconciles.
    ctx.save(
        revision,
        None,
        &format!("blocked {op_id}: publish outcome unknown"),
    )?;
    ctx.push_chain();
    Ok(Some(
        BayResult::blocked(Reason::PublishOutcomeUnknown)
            .with_change(ctx.id.clone())
            .with_op(op_id)
            .with_detail(json!({"candidate": attempt.commit, "push": push, "trunk": fetch})),
    ))
}

/// Whether a refused push says a ref lock on the remote is held by another process: the files
/// backend's `cannot lock ref '<name>': Unable to create '<name>.lock': File exists`, and the
/// reftable backend's `cannot lock references`.
fn held_lock(error: &str) -> bool {
    error.contains("cannot lock ref")
}

/// G128: the remote's ref lock was held: retry the same candidate after a short wait (`None`),
/// uncounted toward `TRUNK_MOVED` and the queue's deadline, as a hook timeout is.
fn lock_held(ctx: &mut Ctx, op_id: &str, revision: &str) -> Result<Option<BayResult>, String> {
    if let Some(blocked) = uncounted_retry(ctx, op_id, revision, "remote ref lock held")? {
        return Ok(Some(blocked));
    }
    std::thread::sleep(HOOK_RETRY_WAIT);
    Ok(None)
}

/// Counts one uncounted retry (a `HOOK_TIMEOUT`, or a deadline release during a publish): the
/// fifth blocks the operation with `PUBLISH_RETRY_EXHAUSTED` (`DESIGN` § 6). The count lives in
/// the chain, since the next pass re-reads the record from the remote.
fn uncounted_retry(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    why: &str,
) -> Result<Option<BayResult>, String> {
    ctx.retries_here += 1;
    let here = ctx.retries_here;
    let op = ctx.op_mut(op_id);
    op.publish_retries = op.publish_retries.max(here - 1) + 1;
    let retries = op.publish_retries;
    if retries < PUBLISH_RETRY_LIMIT {
        ctx.save(revision, None, &format!("{op_id}: {why}, retry {retries}"))?;
        ctx.push_chain();
        return Ok(None);
    }
    let op = ctx.op_mut(op_id);
    op.state = OpState::Blocked;
    op.outcome = Some(Reason::PublishRetryExhausted.code().to_owned());
    ctx.save(
        revision,
        None,
        &format!("blocked {op_id}: publish retries exhausted"),
    )?;
    ctx.push_chain();
    Ok(Some(
        BayResult::blocked(Reason::PublishRetryExhausted)
            .with_change(ctx.id.clone())
            .with_op(op_id),
    ))
}

/// A publishing push refused because trunk moved from `base` to `trunk`.
struct Moved<'a> {
    base: &'a str,
    trunk: &'a str,
    /// The hook refused the push because the publication queue is held.
    held: bool,
}

/// What a refused publishing push leads to: a place in the queue, or `None` to rebuild on the
/// moved trunk and check again.
fn after_trunk_moved(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    seq: &mut Option<u64>,
    moved: &Moved,
) -> Result<Option<BayResult>, String> {
    if moved.held {
        return enqueue(ctx, op_id, revision, Reason::QueueHeld).map(Some);
    }
    if policy_identity(&ctx.repo.git, moved.base)? != policy_identity(&ctx.repo.git, moved.trunk)? {
        return recheck_under_new_policy(ctx, op_id, revision, seq);
    }
    let op = ctx.op_mut(op_id);
    op.trunk_moved += 1;
    // § 6: bay retries once more, then the operation joins the publication queue.
    if op.trunk_moved >= TRUNK_MOVED_LIMIT && seq.is_none() {
        return enqueue(ctx, op_id, revision, Reason::PublicationQueued).map(Some);
    }
    Ok(None)
}

/// § 7: a candidate checked under an older policy is `POLICY_CHANGED` and rechecked under the new
/// one. That is no `TRUNK_MOVED` refusal. A queue head is released with `policy_changed`, which
/// counts toward no limit (§ 6), and rejoins; any other operation rechecks at once (`None`).
fn recheck_under_new_policy(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    seq: &mut Option<u64>,
) -> Result<Option<BayResult>, String> {
    ctx.op_mut(op_id).policy_changed += 1;
    let Some(at) = seq.take() else {
        return Ok(None);
    };
    let signer = ctx.executor_signer().ok().flatten();
    crate::queue::release(ctx.repo, signer.as_ref(), at, op_id, "policy_changed")?;
    enqueue(ctx, op_id, revision, Reason::PolicyChanged).map(Some)
}

/// The acceptance policy at `commit`, by the two things that make a policy older (§ 7): its
/// `revision` and its manifest digest. `None` when the commit has no policy.
fn policy_identity(git: &Git, commit: &str) -> Result<Option<(u64, String)>, String> {
    Ok(policy::load_at(git, commit)?.map(|policy| (policy.revision, policy.manifest_digest)))
}

enum Gate {
    /// No live queue: publish directly.
    Free,
    /// This operation is the head, at this place.
    Head(u64),
    /// This operation holds a place behind the head, or is blocked by its deadline releases.
    Wait(Box<BayResult>),
}

/// The queue gate (§ 6). A newer revision replaces its change's entry in place, or releases it
/// when it is the head; an operation that is not the head reserves a place and runs no check.
fn queue_gate(ctx: &mut Ctx, op_id: &str) -> Result<Gate, String> {
    let signer = ctx.executor_signer().ok().flatten();
    let mut queue = crate::queue::Queue::read(ctx.repo)?;
    if queue.deadline_releases(op_id, &ctx.op(op_id).mid_publish_releases) >= 3 {
        let op = ctx.op_mut(op_id);
        op.state = OpState::Blocked;
        op.outcome = Some(Reason::QueueDeadlineExceeded.code().to_owned());
        let revision = op.revision.clone();
        ctx.save(
            &revision,
            None,
            &format!("blocked {op_id}: queue deadline exceeded"),
        )?;
        ctx.push_chain();
        return Ok(Gate::Wait(Box::new(
            BayResult::blocked(Reason::QueueDeadlineExceeded)
                .with_change(ctx.id.clone())
                .with_op(op_id),
        )));
    }
    if queue.of(op_id).is_none()
        && let Some(older) = queue.of_change(&ctx.id).cloned()
    {
        if queue.head().is_some_and(|head| head.seq == older.seq) {
            // A newer revision of the head releases it and joins at the back.
            crate::queue::release(
                ctx.repo,
                signer.as_ref(),
                older.seq,
                &older.op_id,
                "superseded",
            )?;
        } else {
            // A newer revision of a queued entry takes its place.
            crate::queue::replace(ctx.repo, signer.as_ref(), &older, op_id)?;
        }
        queue = crate::queue::Queue::read(ctx.repo)?;
    }
    // § 6: a head released because no worker answered rejoins at the back once it is served
    // again, live queue or not, behind whatever queued meanwhile.
    if ctx.op(op_id).unavailable.is_some()
        && queue.of(op_id).is_none()
        && queue.released_as(op_id, crate::queue::UNAVAILABLE)
    {
        crate::queue::join(ctx.repo, signer.as_ref(), op_id, &ctx.id)?;
        queue = crate::queue::Queue::read(ctx.repo)?;
    }
    let Some(head) = queue.head().cloned() else {
        return Ok(Gate::Free);
    };
    if head.op_id == op_id {
        return Ok(Gate::Head(head.seq));
    }
    let revision = ctx.op(op_id).revision.clone();
    let reason = if queue.of(op_id).is_some() {
        Reason::PublicationQueued
    } else {
        Reason::QueueHeld
    };
    enqueue(ctx, op_id, &revision, reason).map(|result| Gate::Wait(Box::new(result)))
}

/// Holds a place in the queue: reserves one if the operation has none, gives up ownership so
/// no lease must expire before the head reaches it, and reports the position.
fn enqueue(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    reason: Reason,
) -> Result<BayResult, String> {
    let signer = ctx.executor_signer().ok().flatten();
    let mut queue = crate::queue::Queue::read(ctx.repo)?;
    if queue.of(op_id).is_none() {
        crate::queue::join(ctx.repo, signer.as_ref(), op_id, &ctx.id)?;
        queue = crate::queue::Queue::read(ctx.repo)?;
    }
    let entry = queue
        .of(op_id)
        .cloned()
        .ok_or("the queue reservation vanished")?;
    let op = ctx.op_mut(op_id);
    if op.state != OpState::Admitted || op.owner.is_some() {
        op.state = OpState::Admitted;
        op.owner = None;
        op.outcome = Some(Reason::PublicationQueued.code().to_owned());
        ctx.save(revision, None, &format!("queued {op_id} at {}", entry.seq))?;
        ctx.push_chain();
    }
    let mut result = BayResult::refused(Decision::Queued, reason)
        .with_change(ctx.id.clone())
        .with_op(op_id)
        .with_detail(json!({
            "position": queue.position(entry.seq),
            "seq": entry.seq,
            "head": queue.head().map(|head| head.op_id.clone()),
        }));
    result.published = Some(false);
    result.waiting_for = Some("queue".to_owned());
    Ok(result)
}

/// The ownership claim (§ 9a): a compare-and-swap on the remote chain that raises the epoch.
/// Returns a report when this executor does not get to run the operation: it already
/// published, it is not runnable, or another executor's claim won the race.
fn claim(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    trunk: &str,
) -> Result<Option<BayResult>, String> {
    if ctx.refetch().is_err() {
        // Offline or unpushed: the local chain is the newest this executor can see.
        ctx.pushed_head = None;
    }
    if let Some(result) = scanned(ctx, trunk, revision)? {
        return Ok(Some(existing(result)));
    }
    let name = ctx.executor_name();
    let id = ctx.id.clone();
    let held = {
        let op = ctx.op(op_id);
        // An operation this executor left waiting for a worker holds no lease against it: it
        // takes it back at once once its box serves it (§ 7).
        let own_wait = op.unavailable.is_some() && op.owner.as_deref() == Some(name.as_str());
        op.owner.is_some() && matches!(op.state, OpState::Running | OpState::Checked) && !own_wait
    };
    // Another process holds the operation: take it over only once its lease expired, on this
    // process's own clock. A restarted supervisor watches a full lease again. A holder whose
    // executor key the registry revoked holds no lease (§ 10a).
    if held && !holder_revoked(ctx, op_id)? && !lease_expired(ctx) {
        let op = ctx.op(op_id);
        let mut result = BayResult::new(Decision::Ok).with_change(&id).with_op(op_id);
        result.state = Some("running".to_owned());
        return Ok(Some(result.with_detail(
            json!({"lease": "held", "owner": op.owner, "epoch": op.epoch}),
        )));
    }
    kill_point("before-claim");
    let op = ctx.op_mut(op_id);
    if op.state.is_terminal() {
        return Ok(Some(superseded(&id, op_id, op.epoch, op.owner.as_ref())));
    }
    op.epoch += 1;
    op.owner = Some(name.clone());
    if op.state != OpState::Checked {
        op.state = OpState::Running;
    }
    let epoch = op.epoch;
    ctx.save(
        revision,
        None,
        &format!("claim {op_id} epoch {epoch} by {name}"),
    )?;
    if !ctx.push_chain() {
        ctx.refetch()?;
        let op = ctx.op(op_id);
        return Ok(Some(superseded(
            &ctx.id,
            op_id,
            op.epoch,
            op.owner.as_ref(),
        )));
    }
    ctx.my_epoch = epoch;
    Ok(None)
}

/// Before the publishing push: the executor reads the chain from the remote, and if its epoch
/// is no longer current it never pushes (§ 9a, G24). It reports the existing publication, or
/// that it was superseded.
fn still_owner(ctx: &mut Ctx, op_id: &str, revision: &str) -> Result<Option<BayResult>, String> {
    if ctx.refetch().is_err() {
        return Ok(None);
    }
    let op = ctx.op(op_id);
    if op.epoch == ctx.my_epoch && op.state != OpState::Superseded {
        return Ok(None);
    }
    let (epoch, owner) = (op.epoch, op.owner.clone());
    let trunk = ctx.repo.fetch_trunk().map_err(|error| error.message())?;
    if let Some(result) = scanned(ctx, &trunk, revision)? {
        return Ok(Some(existing(result)));
    }
    Ok(Some(superseded(&ctx.id, op_id, epoch, owner.as_ref())))
}

/// Marks a landed result as a publication this executor found, not one it made.
fn existing(mut result: BayResult) -> BayResult {
    if let Some(detail) = result
        .detail
        .as_mut()
        .and_then(serde_json::Value::as_object_mut)
    {
        detail.insert("existing".to_owned(), json!(true));
    }
    result
}

fn superseded(id: &str, op_id: &str, epoch: u64, owner: Option<&String>) -> BayResult {
    let mut result = BayResult::new(Decision::Ok).with_change(id).with_op(op_id);
    result.state = Some("superseded".to_owned());
    result.with_detail(json!({"superseded": true, "epoch": epoch, "owner": owner}))
}

/// The condition an operation was blocked on: the revision, the trunk, the evidence on the
/// candidate tree, and the versions the change observed of its named inputs (§ 8: reading the
/// required version is what retries an `INPUT_UNOBSERVED`). A retry under the same condition
/// can only reach the same outcome.
fn condition(
    git: &Git,
    revision: &str,
    trunk: &str,
    tree: Option<&str>,
    observed: &std::collections::BTreeMap<String, String>,
) -> Result<String, String> {
    let mut evidence: Vec<String> = match tree {
        Some(tree) => evidence::records_for(git, tree)?
            .into_iter()
            .map(|record| record.id)
            .collect(),
        None => Vec::new(),
    };
    evidence.sort();
    let mut condition = json!({"revision": revision, "trunk": trunk, "evidence": evidence});
    // A change with no named input keeps the condition it always had.
    if !observed.is_empty() {
        condition["observed_versions"] = json!(observed);
    }
    digest(&condition)
}

/// A blocked operation retried (§ 2 of the interfaces): an unchanged condition is refused with
/// the same code and no work, unless the code is transient; a changed one re-arms the operation.
fn retry_blocked(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    trunk: &str,
) -> Result<Option<BayResult>, String> {
    let op = ctx.op(op_id);
    let reason = op.outcome.as_deref().and_then(Reason::from_code);
    let tree = op.attempts.last().map(|attempt| attempt.tree.clone());
    let then = op.blocked_on.clone();
    let now = condition(
        &ctx.repo.git,
        revision,
        trunk,
        tree.as_deref(),
        &ctx.record.observed_versions,
    )?;
    if let Some(reason) = reason
        && !reason.transient()
        && then.as_deref() == Some(now.as_str())
    {
        return Ok(Some(
            BayResult::blocked(reason)
                .with_change(ctx.id.clone())
                .with_op(op_id)
                .with_detail(json!({"unchanged": true, "condition": now})),
        ));
    }
    let op = ctx.op_mut(op_id);
    op.state = OpState::Submitted;
    op.outcome = None;
    op.blocked_on = None;
    op.unavailable = None;
    op.trunk_moved = 0;
    ctx.save(revision, None, &format!("rearmed {op_id}"))?;
    ctx.push_chain();
    Ok(None)
}

/// G28: the request commit must name this op id, revision and request digest, and the digest
/// must be the one the request's content gives. An op id reused with a different request is
/// refused before any work.
fn verify_request_digest(ctx: &Ctx, op_id: &str) -> Result<Option<BayResult>, String> {
    let op = ctx.op(op_id);
    let refuse = |why: &str| {
        Ok(Some(
            BayResult::failed(format!("operation {op_id} is refused, no work done: {why}"))
                .with_change(ctx.id.clone())
                .with_op(op_id),
        ))
    };
    let Some(request_id) = &op.request_id else {
        return refuse("it names no request");
    };
    let Ok(text) = ctx
        .repo
        .git
        .run(&["cat-file", "blob", &format!("{request_id}:request.json")])
    else {
        return refuse("its request is not a request commit");
    };
    let request: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| error.to_string())?;
    // G86: a request of a schema major this executor does not know is refused, never guessed at.
    if request["schema"] != REQUEST_SCHEMA {
        return refuse(&format!(
            "its request is {}; this bay reads {REQUEST_SCHEMA} (update bay)",
            request["schema"]
        ));
    }
    let expected = digest(&json!({
        "change": ctx.id,
        "revision": op.revision,
        "intent": {"reference": ctx.record.intent.reference, "revision": ctx.record.intent.revision},
    }))?;
    if request["op_id"] != op_id
        || request["revision"] != op.revision.as_str()
        || request["request_digest"] != op.request_digest.as_str()
        || op.request_digest != expected
    {
        return refuse("the op id is reused with a different request digest");
    }
    Ok(None)
}

/// The executor verifies the agent-signed request before it builds anything (§ 9a): signed by
/// a key in the agent file, under the actor the change records (G25).
fn verify_request(ctx: &mut Ctx, op_id: &str) -> Result<Option<BayResult>, String> {
    if let Some(refusal) = verify_request_digest(ctx, op_id)? {
        return Ok(Some(refusal));
    }
    let Some(registry) = &ctx.registry else {
        return Ok(None);
    };
    let request = ctx.op(op_id).request_id.clone().unwrap_or_default();
    // A request is judged by the registry version it names, so revoking a departed agent's key
    // later does not strand its queued request (§ 10a). That version is on the registry chain,
    // as the hook requires; a request naming any other commit is judged under none.
    let named = ctx
        .repo
        .git
        .run(&["cat-file", "blob", &format!("{request}:request.json")])
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|json| json["registry"].as_str().map(str::to_owned))
        .unwrap_or_default();
    let Some(named) = registry.version_on_chain(&ctx.repo.git, &named)? else {
        return Ok(Some(
            BayResult::failed(format!(
                "the request {request} of {op_id} names registry {named:?}, which is not on the registry chain"
            ))
            .with_change(ctx.id.clone())
            .with_op(op_id),
        ));
    };
    let registry = &named;
    let agents = registry.class_file(&ctx.repo.git, KeyClass::Agent)?;
    let principal = signing::verify_commit(&ctx.repo.git, &request, &agents)
        .map_err(|error| error.message())?;
    if principal.as_deref() == Some(ctx.record.actor.name.as_str()) {
        return Ok(None);
    }
    // The last resort (§ 7): a request signed by a registry key is a policy repair.
    let registrars = registry.class_file(&ctx.repo.git, KeyClass::Registry)?;
    if let Some(registrar) = signing::verify_commit(&ctx.repo.git, &request, &registrars)
        .map_err(|error| error.message())?
    {
        ctx.repair = Some(registrar);
        return Ok(None);
    }
    Ok(Some(
        BayResult::failed(format!(
            "the request {request} of {op_id} is not signed by an agent key of the change's actor {}",
            ctx.record.actor.name
        ))
        .with_change(ctx.id.clone())
        .with_op(op_id),
    ))
}

/// Step 1: freezes the view into the revision. An unchanged view keeps the latest draft, so a
/// retry names the same revision. A view that is not ready is refused (§ 10).
fn freeze(ctx: &Ctx) -> Result<Result<String, BayResult>, String> {
    let (repo, id) = (ctx.repo, ctx.id.as_str());
    if !view::journal(repo, id).is_some_and(view::Journal::is_complete) {
        return Ok(Err(BayResult::blocked(Reason::WorkspaceIncomplete)
            .with_change(id)
            .with_detail(
                json!({"view": repo.view_path(id).display().to_string()}),
            )));
    }
    let view = Git::new(repo.view_path(id));
    let latest = ctx.chain.drafts(id).map_err(|error| error.message())?[0].clone();
    let frozen = freeze_draft(&view, &ctx.record.base, "draft").map_err(|error| error.message())?;
    let revision = if tree_of(&repo.git, &frozen)? == tree_of(&repo.git, &latest)?
        && parent_of(&repo.git, &latest)?.as_deref() == Some(ctx.record.base.as_str())
    {
        latest
    } else {
        frozen
    };
    if view::journal(repo, id) == Some(view::Journal::Ready) {
        view::set_journal(repo, id, view::Journal::Checkpointed)?;
    }
    Ok(Ok(revision))
}

/// Records the view's undeclared paths as observed inputs before the request is written.
fn observe_inputs(ctx: &mut Ctx) -> Result<(), String> {
    let path = ctx.repo.view_path(&ctx.id);
    let declared = inputs::paths(&ctx.record.declared_inputs);
    view::record_observed(ctx.repo, &path, &declared, &mut ctx.record.observed_inputs)
        .map(|_| ())
        .map_err(|error| error.message())
}

/// Appends the request for a new operation; the compare-and-swap allocates its number.
fn submit(ctx: &mut Ctx, revision: &str) -> Result<String, String> {
    let request_digest = digest(&json!({
        "change": ctx.id,
        "revision": revision,
        "intent": {"reference": ctx.record.intent.reference, "revision": ctx.record.intent.revision},
    }))?;
    let op_id = format!("{}.{}", ctx.id, ctx.record.operations.len() + 1);
    ctx.record.state = ChangeState::Ready;
    // § 10a: a newer revision supersedes every unpublished operation of the change by itself;
    // their executors find it before they publish.
    for earlier in &mut ctx.record.operations {
        if !earlier.state.is_terminal() {
            earlier.state = OpState::Superseded;
            earlier.outcome = Some(format!("superseded by {op_id}"));
        }
    }
    ctx.record.operations.push(Operation {
        op_id: op_id.clone(),
        revision: revision.to_owned(),
        request_digest,
        request_id: None,
        state: OpState::Submitted,
        attempts: Vec::new(),
        outcome: None,
        job: None,
        blocked_on: None,
        epoch: 0,
        owner: None,
        trunk_moved: 0,
        policy_changed: 0,
        publish_retries: 0,
        mid_publish_releases: Vec::new(),
        unavailable: None,
    });
    // The request: an agent-signed commit on the revision, linked into the chain. Appending it
    // allocates the op id by compare-and-swap; `refs/bay/requests/<id>` moves with it.
    let request = json!({
        "schema": REQUEST_SCHEMA,
        "op_id": op_id,
        "change": ctx.id,
        "revision": revision,
        "request_digest": ctx.op(&op_id).request_digest,
        "actor": ctx.record.actor,
        "registry": ctx.registry.as_ref().map(|registry| registry.commit.clone()),
    });
    let request_id = ctx
        .chain
        .request_commit(&request, revision, &format!("request {op_id}"))
        .map_err(|error| error.message())?;
    ctx.op_mut(&op_id).request_id = Some(request_id.clone());
    ctx.save(revision, Some(&request_id), &format!("request {op_id}"))?;
    if !ctx.push_chain_and_request(&request_id) && ctx.online {
        // The remote chain moved while this request was written (an executor appended): the
        // caller re-reads the chain and writes the request again.
        return Err(CHAIN_MOVED.to_owned());
    }
    admit(ctx, &op_id, revision)?;
    Ok(op_id)
}

/// `submit` lost a race with another writer of the chain.
const CHAIN_MOVED: &str = "the change chain moved on the remote while the request was written";

/// The abandon request (§ 4, G40): an agent-signed request of kind `abandon` on the change
/// chain, by the actor the chain records as current. The change becomes `abandoned` and any
/// actor may then adopt its intent. Its pending operations are cancelled; trunk, the requests
/// ref and the view stay as they are.
fn abandon_change(repo: &Repo, id: &str, step: &Cell<&'static str>) -> Result<BayResult, String> {
    let mut ctx = Ctx::load(repo, step, id)?;
    let trunk = match ctx.fetch_trunk()? {
        Ok(trunk) => trunk,
        Err(refusal) => return Ok(refusal),
    };
    let me = crate::change::current_actor()?;
    if me.name != ctx.record.actor.name {
        return Ok(BayResult::blocked(Reason::AbandonRefused)
            .with_change(id)
            .with_detail(json!({"current_actor": ctx.record.actor,
                "current_actor_source": ctx.record.actor_source})));
    }
    let landed = !scan(&repo.git, &ctx.record.base_first, &trunk, id)?
        .publications
        .is_empty()
        || crate::compact::remote_marker(repo, &format!("{}{id}", crate::names::LANDED))?.is_some();
    if landed {
        return Ok(BayResult::blocked(Reason::ChangeAlreadyLanded).with_change(id));
    }
    let agent = ctx.agent_signer()?;
    ctx.chain.sign_with(agent);
    let draft = ctx.chain.drafts(id).map_err(|error| error.message())?[0].clone();
    let request = json!({
        "schema": REQUEST_SCHEMA,
        "kind": "abandon",
        "change": id,
        "actor": ctx.record.actor,
        "registry": ctx.registry.as_ref().map(|registry| registry.commit.clone()),
    });
    let request_id = ctx
        .chain
        .request_commit(&request, &draft, "abandon")
        .map_err(|error| error.message())?;
    ctx.record.state = ChangeState::Abandoned;
    for op in &mut ctx.record.operations {
        if !op.state.is_terminal() {
            op.state = OpState::Superseded;
            op.outcome = Some("abandoned".to_owned());
        }
    }
    ctx.save(&draft, Some(&request_id), &format!("abandon {id}"))?;
    if !ctx.push_chain() {
        return Err(CHAIN_MOVED.to_owned());
    }
    Ok(
        BayResult::ok(json!({"state": "abandoned", "event": "abandoned", "request": request_id}))
            .with_change(id),
    )
}

/// Makes the remote's chain the local one (§ 9a): the remote holds what executors recorded.
/// Local-only work was pushed by the sync before this, or is re-made by landing again.
fn adopt_remote_chain(repo: &Repo, id: &str) {
    let name = change_ref(id);
    let _ = repo.git.run(&[
        "fetch",
        "-q",
        "--no-tags",
        repo.remote(),
        &format!("+{name}:{name}"),
    ]);
}

/// § 3a submission order: the request is persisted; then the configured executor's `submit`
/// returns a durable job handle, which is persisted before admission is acknowledged. With no
/// external executor configured the request in the chain is the submission.
fn admit(ctx: &mut Ctx, op_id: &str, revision: &str) -> Result<(), String> {
    let Some(command) = ctx
        .repo
        .config
        .executor
        .as_ref()
        .and_then(|executor| executor.command.clone())
    else {
        return Ok(());
    };
    let op = ctx.op(op_id);
    let request = json!({
        "action": "submit",
        "op_id": op_id,
        "attempt_id": null,
        "request_digest": op.request_digest,
        "candidate": revision,
    });
    let mut words = command.split_whitespace();
    let program = ctx.repo.work_root.join(words.next().unwrap_or_default());
    let mut cmd = std::process::Command::new(program);
    cmd.args(words)
        .arg("submit")
        .current_dir(&ctx.repo.work_root);
    let output = match crate::git::run_bounded_for(
        cmd,
        "executor submit",
        Some(request.to_string().as_bytes()),
        ctx.repo.config.submission_timeout(),
    ) {
        Ok(output) => output,
        Err(crate::git::GitError::Timeout { secs, .. }) => {
            // The submission timeout ends the operation with a stated outcome (§ 9a).
            let op = ctx.op_mut(op_id);
            op.state = OpState::Failed;
            op.outcome = Some("submission_timeout".to_owned());
            ctx.save(
                revision,
                None,
                &format!("failed {op_id}: submission timeout"),
            )?;
            ctx.push_chain();
            return Err(format!(
                "the executor did not admit {op_id} within the submission timeout of {secs}s"
            ));
        }
        Err(error) => return Err(error.message()),
    };
    if !output.status.success() {
        return Err(format!(
            "the executor refused the submission: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("the executor's submit reply is not JSON: {error}"))?;
    let job = reply["job"]
        .as_str()
        .ok_or("the executor's submit reply has no job")?
        .to_owned();
    let op = ctx.op_mut(op_id);
    op.job = Some(job);
    op.state = OpState::Admitted;
    ctx.save(revision, None, &format!("admitted {op_id}"))?;
    ctx.push_chain();
    Ok(())
}

/// G244: trunk is red on paths this change touches, and the change does not name the landing
/// that made it red (by `--fixes`, an undo, or a supersede): `TRUNK_RED`, naming the owner and
/// the ways out. The operation is blocked on trunk's state, never failed.
fn red_gate(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    trunk: &str,
) -> Result<Option<BayResult>, String> {
    let git = &ctx.repo.git;
    let reds = crate::red::red(git, trunk, ctx.registry.as_ref())?;
    if reds.is_empty() {
        return Ok(None);
    }
    let base = git
        .run(&["merge-base", trunk, revision])
        .map_err(|error| error.message())?;
    let changed = check::changed_paths(git, &base, revision)?;
    let names: Vec<&String> = [
        &ctx.record.fixes,
        &ctx.record.reverts,
        &ctx.record.supersedes,
    ]
    .into_iter()
    .flatten()
    .collect();
    let blocking: Vec<&crate::red::Red> = reds
        .iter()
        .filter(|red| red.covers(&changed))
        .filter(|red| {
            red.change
                .as_ref()
                .is_none_or(|owner| !names.contains(&owner))
        })
        .collect();
    if blocking.is_empty() {
        return Ok(None);
    }
    let op = ctx.op_mut(op_id);
    op.state = OpState::Blocked;
    op.outcome = Some(Reason::TrunkRed.code().to_owned());
    ctx.save(revision, None, &format!("blocked {op_id}: trunk is red"))?;
    ctx.push_chain();
    let owners: Vec<Option<&String>> = blocking.iter().map(|red| red.change.as_ref()).collect();
    Ok(Some(
        BayResult::blocked(Reason::TrunkRed)
            .with_change(ctx.id.clone())
            .with_op(op_id)
            .with_detail(json!({
                "red": blocking.iter().map(|red| red.to_json()).collect::<Vec<_>>(),
                "ways_out": [
                    "land a change that names the owner: bay start --fixes <owner>, or bay undo <owner>",
                    "a later run of the post check passes on trunk",
                    "quarantine the check in the policy",
                ],
                "owners": owners,
            })),
    ))
}

/// Step 2: the merged tree of `revision` onto `trunk`, or the `MERGE_CONFLICT` refusal, with the
/// operation blocked on the condition that lifts it.
fn merged(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    trunk: &str,
) -> Result<Result<String, BayResult>, String> {
    let git = &ctx.repo.git;
    ctx.step.set("merge");
    match merge_onto(git, trunk, revision).map_err(|error| error.message())? {
        Merge::Clean(tree) => Ok(Ok(tree)),
        Merge::Conflict(paths, _) => {
            let condition = condition(git, revision, trunk, None, &ctx.record.observed_versions)?;
            let op = ctx.op_mut(op_id);
            op.state = OpState::Blocked;
            op.outcome = Some(Reason::MergeConflict.code().to_owned());
            op.blocked_on = Some(condition);
            ctx.save(revision, None, "blocked: merge conflict")?;
            ctx.push_chain();
            Ok(Err(BayResult::blocked(Reason::MergeConflict)
                .with_change(ctx.id.clone())
                .with_op(op_id)
                .with_detail(json!({"conflicts": paths, "trunk": trunk}))))
        }
    }
}

/// G213: a merged `tree` equal to `trunk`'s own tree is work already on trunk, whether another
/// change landed the same bytes or the change's edits cancel out. Nothing is published and no
/// check runs; the operation ends `no_effect`, naming the trunk commit that holds the work.
fn no_effect(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    trunk: &str,
    tree: &str,
) -> Result<Option<BayResult>, String> {
    let trunk_tree = ctx
        .repo
        .git
        .run(&["rev-parse", &format!("{trunk}^{{tree}}")])
        .map_err(|error| error.message())?;
    if trunk_tree != tree {
        return Ok(None);
    }
    let op = ctx.op_mut(op_id);
    op.state = OpState::NoEffect;
    op.outcome = Some(NO_EFFECT.to_owned());
    ctx.record.state = ChangeState::NoEffect;
    ctx.save(
        revision,
        None,
        &format!("{NO_EFFECT} {op_id}: already on trunk at {trunk}"),
    )?;
    ctx.push_chain();
    let mut result = BayResult::new(Decision::Ok)
        .with_change(ctx.id.clone())
        .with_op(op_id)
        .with_detail(json!({"trunk_commit": trunk, "tree": tree}));
    result.state = Some(NO_EFFECT.to_owned());
    Ok(Some(result))
}

/// The schema of a land or abandon request.
const REQUEST_SCHEMA: &str = "bay.request/1";

/// The terminal outcome of an operation whose work is already on trunk (G213).
pub const NO_EFFECT: &str = "no_effect";

/// Steps 2–5: merge onto `trunk`, commit the candidate, check it, persist it.
fn build_and_check(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    trunk: &str,
) -> Result<Result<Attempt, BayResult>, String> {
    let judged = match input_gate(ctx, op_id, revision, trunk)? {
        Ok(judged) => judged,
        Err(refusal) => return Ok(Err(refusal)),
    };
    if let Some(red) = red_gate(ctx, op_id, revision, trunk)? {
        return Ok(Err(red));
    }
    let tree = match merged(ctx, op_id, revision, trunk)? {
        Ok(tree) => tree,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let git = &ctx.repo.git;
    if let Some(no_effect) = no_effect(ctx, op_id, revision, trunk, &tree)? {
        return Ok(Err(no_effect));
    }
    anchor(git, op_id, &tree)?;

    let candidate_id = format!("{op_id}.{}", ctx.op(op_id).attempts.len() + 1);
    let request_id = ctx.op(op_id).request_id.clone().unwrap_or_default();
    let registry_id = ctx
        .registry
        .as_ref()
        .map_or_else(|| "none".to_owned(), |registry| registry.commit.clone());
    let message = candidate_message(ctx, revision, &request_id, &candidate_id, &registry_id);
    let commit = commit_candidate(ctx, &tree, trunk, &message)?;
    anchor(&ctx.repo.git, op_id, &commit)?;
    kill_point("after-commit");

    // Step 4: check the exact candidate under the policy at its base.
    ctx.step.set("check");
    let (has_policy, verdict, refusal) = check_candidate(
        ctx,
        op_id,
        &candidate_id,
        &commit,
        &tree,
        trunk,
        &inputs::request_inputs(&judged),
    )?;
    let refusal = judged_outcome(ctx, op_id, &candidate_id, refusal, &judged);
    // Heartbeats moved the chain while the check ran; continue from the last one pushed.
    if let Some(beat) = ctx.beat_pushed.borrow_mut().take() {
        ctx.head.clone_from(&beat);
        ctx.pushed_head = Some(beat);
    }
    let refusal = match refusal {
        Some(waiting) if waiting.reason == Some(Reason::CheckUnavailable) => {
            return wait_for_worker(ctx, op_id, revision, waiting).map(Err);
        }
        refusal => refusal,
    };

    ctx.step.set("persist");
    // Step 5: persist before publishing. The candidate is linked into the chain; the evidence
    // is pushed and read back from the remote.
    let attempt = Attempt {
        candidate_id: candidate_id.clone(),
        base: trunk.to_owned(),
        commit: commit.clone(),
        tree,
        verdict: verdict.to_owned(),
        epoch: ctx.my_epoch,
    };
    let blocked_on = if refusal.is_some() {
        Some(condition(
            &ctx.repo.git,
            revision,
            trunk,
            Some(&attempt.tree),
            &ctx.record.observed_versions,
        )?)
    } else {
        None
    };
    let op = ctx.op_mut(op_id);
    op.attempts.push(attempt.clone());
    op.unavailable = None;
    if let Some(refusal) = refusal {
        op.state = OpState::Blocked;
        op.outcome = refusal.reason.map(|reason| reason.code().to_owned());
        op.blocked_on = blocked_on;
        ctx.save(revision, Some(&commit), &format!("blocked {candidate_id}"))?;
        ctx.push_chain();
        return Ok(Err(refusal));
    }
    op.state = OpState::Checked;
    op.outcome = None;
    ctx.save(revision, Some(&commit), &format!("checked {candidate_id}"))?;
    if has_policy {
        push_and_read_back_evidence(ctx.repo, &ctx.producer()?.0)?;
    }
    if !ctx.push_chain() {
        // Another executor moved the chain: a newer claim means this one was superseded.
        if ctx.refetch().is_ok() && ctx.op(op_id).epoch != ctx.my_epoch {
            let op = ctx.op(op_id);
            return Ok(Err(superseded(&ctx.id, op_id, op.epoch, op.owner.as_ref())));
        }
        return Err("the change chain could not be pushed; nothing was published".to_owned());
    }
    kill_point("after-persist");
    Ok(Ok(attempt))
}

/// § 8: a failing candidate of a change whose input moved is a repair still owed on that
/// input (`INPUT_CHANGED`); a passing one is the candidate validated under the required
/// versions. Returns the refusal the attempt ends with.
fn judged_outcome(
    ctx: &mut Ctx,
    op_id: &str,
    candidate_id: &str,
    refusal: Option<BayResult>,
    judged: &[inputs::Judgement],
) -> Option<BayResult> {
    match refusal {
        Some(refusal) if refusal.reason == Some(Reason::CheckFailed) => {
            match judged
                .iter()
                .find(|judgement| judgement.state == inputs::InputState::Changed)
            {
                Some(changed) => Some(input_refusal(ctx, op_id, changed, judged, Some(&refusal))),
                None => Some(refusal),
            }
        }
        None if !judged.is_empty() => {
            ctx.record.validated_candidate = Some(inputs::Validated {
                candidate: candidate_id.to_owned(),
                versions: inputs::required_versions(judged),
            });
            None
        }
        refusal => refusal,
    }
}

/// § 8: a change that has not observed what trunk now requires of its inputs is refused
/// before any candidate is built. Returns every named input judged at `trunk`.
fn input_gate(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    trunk: &str,
) -> Result<Result<Vec<inputs::Judgement>, BayResult>, String> {
    ctx.step.set("inputs");
    // The gate runs every provider fresh: a cached answer is a plain file, never a derivation.
    let judged = inputs::judge(ctx.repo, &ctx.record, trunk, inputs::Probe::Fresh)?;
    if let Some(stale) = judged
        .iter()
        .find(|judgement| judgement.state == inputs::InputState::Unobserved)
    {
        let condition = condition(
            &ctx.repo.git,
            revision,
            trunk,
            None,
            &ctx.record.observed_versions,
        )?;
        let op = ctx.op_mut(op_id);
        op.state = OpState::Blocked;
        op.outcome = Some(Reason::InputUnobserved.code().to_owned());
        op.blocked_on = Some(condition);
        ctx.save(
            revision,
            None,
            &format!("blocked: input {} unobserved", stale.input),
        )?;
        ctx.push_chain();
        return Ok(Err(input_refusal(ctx, op_id, stale, &judged, None)));
    }
    // G8 fails closed: trunk names a provider for the input and it gave no version, so nothing
    // shows the change observed what trunk requires. The block records no condition: the
    // provider's answer is outside it, so a retry always asks the provider again.
    if let Some(unknown) = judged
        .iter()
        .find(|judgement| judgement.named && judgement.state == inputs::InputState::Unknown)
    {
        let op = ctx.op_mut(op_id);
        op.state = OpState::Blocked;
        op.outcome = Some(Reason::CoverageUnknown.code().to_owned());
        op.blocked_on = None;
        ctx.save(
            revision,
            None,
            &format!("blocked: input {} has no known version", unknown.input),
        )?;
        ctx.push_chain();
        return Ok(Err(BayResult::blocked(Reason::CoverageUnknown)
            .with_change(ctx.id.clone())
            .with_op(op_id)
            .with_detail(json!({
                "input": unknown.input,
                "why": unknown.why,
                "repair": format!("make trunk's provider for {} answer", unknown.input),
                "inputs": inputs::to_json(judged.iter()),
            }))));
    }
    Ok(Ok(judged))
}

/// The refusal on a stale input (§ 8): what changed, which assumption it invalidated, and
/// which repair remains, with every named input's coverage. `check` is the failed check a
/// changed input's candidate was refused by.
fn input_refusal(
    ctx: &Ctx,
    op_id: &str,
    stale: &inputs::Judgement,
    judged: &[inputs::Judgement],
    check: Option<&BayResult>,
) -> BayResult {
    let reason = stale.reason().unwrap_or(Reason::InputChanged);
    let repair = if reason == Reason::InputUnobserved {
        format!("read the required version: bay start --resume {}", ctx.id)
    } else {
        format!(
            "repair the change for {} {}",
            stale.input,
            stale.required.as_deref().unwrap_or_default()
        )
    };
    let mut detail = json!({
        "input": stale.input,
        "observed": stale.observed,
        "required": stale.required,
        "phase": stale.phase,
        "consumer": stale.consumer,
        "repair": repair,
        "inputs": inputs::to_json(judged.iter()),
    });
    if let Some(check) = check {
        detail["check"] = json!(check.reason);
        detail["candidate"] = check
            .detail
            .as_ref()
            .map_or(serde_json::Value::Null, |detail| {
                detail["candidate"].clone()
            });
        detail["checks"] = check
            .detail
            .as_ref()
            .map_or(serde_json::Value::Null, |detail| detail["checks"].clone());
    }
    BayResult::blocked(reason)
        .with_change(ctx.id.clone())
        .with_op(op_id)
        .with_detail(detail)
}

/// Step 4: runs the base policy's checks on the candidate and combines the evidence on its
/// tree. Returns whether a judging policy ran checks (so there is evidence to push), the
/// attempt's verdict word ([`UNCHECKED`] when the policy that judges it is unchecked), and the
/// refusal when any required check is not `pass` or the base has no policy.
fn check_candidate(
    ctx: &Ctx,
    op_id: &str,
    candidate_id: &str,
    commit: &str,
    tree: &str,
    trunk: &str,
    inputs: &[String],
) -> Result<(bool, &'static str, Option<BayResult>), String> {
    let git = &ctx.repo.git;
    let refuse = |why: String| {
        Ok((
            true,
            "unknown",
            Some(
                BayResult::failed(why)
                    .with_change(ctx.id.clone())
                    .with_op(op_id),
            ),
        ))
    };
    let base_policy = match policy::load_at(git, trunk) {
        Ok(policy) => policy,
        // § 7: a registry-signed repair is judged by its own new policy alone, so an invalid
        // base policy stays repairable.
        Err(policy::LoadError::Invalid(_)) if ctx.repair.is_some() => None,
        // § 7: the policy at the accepted base breaks a validity rule, so no candidate can be
        // judged by it; a bad policy never passes its own candidate.
        Err(policy::LoadError::Invalid(why)) => {
            return Ok(policy_invalid(ctx, op_id, trunk, "base", &why));
        }
        Err(policy::LoadError::Git(why)) => return refuse(why),
    };
    // The missing-policy ruling: no policy at the base is unknown criteria, and unknown is
    // never pass. Only a registry-signed repair, judged by its own new policy alone (§ 7), may
    // stand on such a base; a candidate's own policy never judges it otherwise.
    if base_policy.is_none() && ctx.repair.is_none() {
        return Ok(policy_missing(ctx, op_id, trunk));
    }
    // Runs `policy`'s checks with the adapter from `from`, under check id prefix `id`. An
    // unchecked policy runs nothing and says so.
    let run = |id: &str, policy: &policy::Policy, from: &str| {
        if policy.unchecked {
            return Ok((UNCHECKED, None));
        }
        run_policy(ctx, op_id, id, commit, tree, trunk, policy, from, inputs)
    };
    let changed = policy_changed(git, trunk, commit, base_policy.as_ref())?;
    if !changed {
        if ctx.repair.is_some() {
            return refuse("a policy repair changes only .bay/policy.toml and adapter paths, and this change does not change them".to_owned());
        }
        let Some(policy) = &base_policy else {
            return Ok(policy_missing(ctx, op_id, trunk));
        };
        let (verdict, refusal) = run(candidate_id, policy, trunk)?;
        return Ok((!policy.unchecked, verdict, refusal));
    }

    // A change to the policy or an adapter path (§ 7): it must raise the revision; it is
    // checked by the base's adapter and by its own new adapter, and both must pass. A
    // registry-signed repair is checked by its own new adapter alone.
    let own_policy = match policy::load_at(git, commit) {
        Ok(Some(policy)) => policy,
        Ok(None) => {
            return refuse(format!(
                "this change removes {}; the policy cannot be removed",
                policy::POLICY_PATH
            ));
        }
        // § 7: the candidate's own new policy breaks a validity rule. The same refusal as for
        // the base's, naming the candidate, so the agent repairs it in a new revision.
        Err(policy::LoadError::Invalid(why)) => {
            return Ok(policy_invalid(ctx, op_id, commit, "candidate", &why));
        }
        Err(policy::LoadError::Git(why)) => return refuse(why),
    };
    // An invalid base policy still states its revision; a repair must raise it too.
    let base_revision = base_policy
        .as_ref()
        .map(|policy| policy.revision)
        .or_else(|| policy::revision_at(git, trunk))
        .unwrap_or(0);
    if own_policy.revision <= base_revision {
        return refuse(format!(
            "this change edits the policy or an adapter path, so it must raise revision above {base_revision}; it has {}",
            own_policy.revision
        ));
    }
    if let Some(principal) = &ctx.repair {
        if !only_policy_paths(git, trunk, commit, base_policy.as_ref(), &own_policy)? {
            return refuse(format!(
                "a policy repair signed by {principal} may touch only .bay/policy.toml and adapter paths"
            ));
        }
        let own_id = format!("{candidate_id}-own");
        let (verdict, refusal) = run(&own_id, &own_policy, commit)?;
        return Ok((!own_policy.unchecked, verdict, refusal));
    }
    let Some(policy) = &base_policy else {
        return Ok(policy_missing(ctx, op_id, trunk));
    };
    // The unchecked-base ruling: the criteria are judged at the base. Under an unchecked base
    // the candidate lands unchecked by the base's policy; a policy or adapter it adds takes
    // effect only after it lands, never for its own landing, so none of its checks runs here.
    if policy.unchecked {
        return Ok((false, UNCHECKED, None));
    }
    let (verdict, refusal) = run(candidate_id, policy, trunk)?;
    if refusal.is_some() {
        return Ok((true, verdict, refusal));
    }
    let own_id = format!("{candidate_id}-own");
    let (own_verdict, refusal) = run(&own_id, &own_policy, commit)?;
    // Checked twice (§ 7): the base's checks ran and passed, so a candidate whose own new
    // policy is unchecked is still a checked pass by the base.
    let verdict = if own_verdict == UNCHECKED {
        verdict
    } else {
        own_verdict
    };
    Ok((true, verdict, refusal))
}

/// The verdict word of an attempt whose judging policies declare `unchecked = true`: it ran
/// no check, so it is never a `pass` (the missing-policy ruling).
pub const UNCHECKED: &str = "unchecked";

/// What a landing under an unchecked policy says, in its result and its terminal record.
pub const UNCHECKED_BY_POLICY: &str = "unchecked by policy";

/// `POLICY_MISSING` (the missing-policy ruling): the base has no `.bay/policy.toml`, so the
/// criteria are unknown. The detail names the base commit and the path.
fn policy_missing(ctx: &Ctx, op_id: &str, base: &str) -> (bool, &'static str, Option<BayResult>) {
    (
        true,
        "unknown",
        Some(
            BayResult::blocked(Reason::PolicyMissing)
                .with_change(ctx.id.clone())
                .with_op(op_id)
                .with_detail(json!({
                    "base": base,
                    "path": policy::POLICY_PATH,
                    "why": format!(
                        "the base {base} has no {}, so the criteria are unknown; add a policy there, or declare unchecked = true in it to land without checks",
                        policy::POLICY_PATH
                    ),
                })),
        ),
    )
}

/// `POLICY_INVALID` (§ 2, § 7): the policy at `at`, the base's or the candidate's own (`which`),
/// breaks a validity rule, so it judges nothing.
fn policy_invalid(
    ctx: &Ctx,
    op_id: &str,
    at: &str,
    which: &str,
    why: &str,
) -> (bool, &'static str, Option<BayResult>) {
    (
        true,
        "unknown",
        Some(
            BayResult::blocked(Reason::PolicyInvalid)
                .with_change(ctx.id.clone())
                .with_op(op_id)
                .with_detail(json!({"policy_at": at, "policy": which, "why": why})),
        ),
    )
}

/// Whether the candidate changes `.bay/policy.toml` or any adapter path of the base policy,
/// a check's or an input provider's.
fn policy_changed(
    git: &Git,
    trunk: &str,
    commit: &str,
    base: Option<&policy::Policy>,
) -> Result<bool, String> {
    let mut paths = vec![policy::POLICY_PATH.to_owned()];
    if let Some(base) = base {
        paths.extend(base.adapter_paths());
    }
    let mut args = vec!["diff", "--name-only", trunk, commit, "--"];
    args.extend(paths.iter().map(String::as_str));
    Ok(!git.run(&args).map_err(|error| error.message())?.is_empty())
}

/// Whether every path the candidate changes is the policy file or an adapter path (of either
/// policy): the only change a repair may make.
fn only_policy_paths(
    git: &Git,
    trunk: &str,
    commit: &str,
    base: Option<&policy::Policy>,
    own: &policy::Policy,
) -> Result<bool, String> {
    let adapters: Vec<String> = base
        .into_iter()
        .chain(std::iter::once(own))
        .flat_map(policy::Policy::adapter_paths)
        .collect();
    let changed = git
        .run(&["diff", "--name-only", trunk, commit])
        .map_err(|error| error.message())?;
    Ok(changed.lines().all(|path| {
        path == policy::POLICY_PATH
            || adapters.iter().any(|adapter| {
                if adapter.ends_with('/') {
                    path.starts_with(adapter.as_str())
                } else {
                    path == adapter
                }
            })
    }))
}

/// Runs `policy`'s checks on the candidate with the adapter materialized from `adapter_from`,
/// and combines the evidence on its tree under that policy's manifest.
#[allow(clippy::too_many_arguments)]
fn run_policy(
    ctx: &Ctx,
    op_id: &str,
    candidate_id: &str,
    commit: &str,
    tree: &str,
    trunk: &str,
    policy: &policy::Policy,
    adapter_from: &str,
    inputs: &[String],
) -> Result<(&'static str, Option<BayResult>), String> {
    let git = &ctx.repo.git;
    let base_tree = tree_of(git, trunk)?;
    let scratch = ctx
        .repo
        .common_dir
        .join("bay")
        .join("check")
        .join(candidate_id);
    let (producer, producer_signer) = ctx.producer()?;
    // A candidate's own new policy reaches trunk through this publication; the base's policy
    // reached it however its paths last changed.
    let policy_provenance = if adapter_from == trunk {
        provenance(ctx, trunk, policy)?
    } else {
        policy::Provenance::Bay
    };
    let (finished, reused) = finished_checks(
        ctx,
        op_id,
        policy,
        &Judged {
            tree,
            commit,
            base_tree: &base_tree,
            provenance: policy_provenance,
            inputs,
        },
    )?;
    // G146: what is required of the candidate follows its own diff against the base, never the
    // paths the change declared: a write outside those is recorded, not refused.
    let changed = check::changed_paths(git, trunk, commit)?;
    // § 7: a required pair runs only on a box that declares its environment. A pair the box
    // does not serve, with no evidence yet, runs nothing here: it waits for a worker.
    let missing = check::unserved(policy, &changed, &finished, &check::Served::read(git));
    if !missing.is_empty() {
        return Ok(("unknown", Some(unavailable(ctx, op_id, &missing, None))));
    }
    let mut keep = |record: &mut evidence::EvidenceRecord| -> Result<(), String> {
        // The producer signs outside the check's sandbox, after reading its output (§ 7).
        if let Some(signer) = &producer_signer {
            record.sign(&signer.key)?;
        }
        evidence::append(git, &producer, record)
    };
    let check::Ran {
        records,
        unavailable: outage,
    } = check::run_all(
        git,
        &scratch,
        policy,
        &Candidate {
            commit,
            tree,
            base: adapter_from,
            base_tree: &base_tree,
            changed_paths: &changed,
            id: candidate_id,
            policy_provenance,
            inputs,
        },
        &producer,
        &check::Limits {
            budget: ctx.repo.config.execution_budget(),
            finished,
            heartbeat: Some(ctx.repo.config.heartbeat()),
            hidden: hidden_paths(ctx),
            sandbox: check::Sandbox::here(),
        },
        &mut check::Hooks {
            keep: &mut keep,
            beat: &mut || heartbeat(ctx, op_id),
        },
    )?;
    let _ = std::fs::remove_dir_all(&scratch);
    // The adapter reached no worker for a check: nothing was observed, so it is no verdict.
    if let Some((environment, reason)) = outage {
        return Ok((
            "unknown",
            Some(unavailable(ctx, op_id, &[environment], Some(&reason))),
        ));
    }
    let all = evidence::counted(
        git,
        evidence::records_for(git, tree)?,
        ctx.registry.as_ref(),
    )?;
    // What to repair: this run's records and the ones reused in place of a run (§ 8).
    let named: Vec<&evidence::EvidenceRecord> = records.iter().chain(&reused).collect();
    let (required, _) = policy.required(&changed);
    let Some((code, word, why)) = verdict(policy, &required, &all, &named, commit) else {
        return Ok(("pass", None));
    };
    Ok((
        word,
        Some(
            BayResult::blocked(code)
                .with_change(ctx.id.clone())
                .with_op(op_id)
                .with_detail(json!({
                    "candidate": candidate_id,
                    "adapter_from": adapter_from,
                    "checks": why,
                    "trust": evidence::trust(ctx.registry.as_ref()),
                })),
        ),
    ))
}

/// `CHECK_UNAVAILABLE` (§ 7): no worker answered for `environments`. With `adapter`, the check
/// adapter's reason; without, this box does not declare them (`bay.checkEnvironment`).
fn unavailable(
    ctx: &Ctx,
    op_id: &str,
    environments: &[String],
    adapter: Option<&str>,
) -> BayResult {
    let mut detail = json!({"environments": environments});
    match adapter {
        Some(reason) => detail["unavailable"] = json!(reason),
        None => {
            detail["recovery"] = json!(format!(
                "an executor whose box serves them claims this operation; a box declares each with git config --global --add {} <environment>",
                check::ENVIRONMENT_KEY
            ));
        }
    }
    let mut result = BayResult::refused(Decision::Queued, Reason::CheckUnavailable)
        .with_change(ctx.id.clone())
        .with_op(op_id)
        .with_detail(detail);
    result.published = Some(false);
    result.waiting_for = Some("worker".to_owned());
    result
}

/// The current time in Unix milliseconds.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The gap before an operation released after an adapter's `unavailable` answer rejoins
/// (§ 6): one lease, doubled with each repeated answer, at most ten leases.
fn rejoin_gap(lease: Duration, repeats: u32) -> Duration {
    lease * 2u32.saturating_pow(repeats).min(10)
}

/// `CHECK_UNAVAILABLE` reached (§ 7): nothing was observed, so no attempt is recorded and no
/// evidence is pushed. The operation stays this executor's and records what it waits for, and
/// the executor stops renewing its lease, so an executor whose box serves it claims it after
/// the lease (§ 9a). This executor passes over it until its own box serves it
/// ([`waits_for_worker`]).
fn wait_for_worker(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    waiting: BayResult,
) -> Result<BayResult, String> {
    let detail = waiting.detail.clone().unwrap_or_default();
    let environments: Vec<String> = detail["environments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|environment| environment.as_str().map(str::to_owned))
        .collect();
    let adapter = detail["unavailable"].as_str().map(str::to_owned);
    let op = ctx.op_mut(op_id);
    // The gap doubles only while the adapter's answer repeats.
    let repeats = match (&op.unavailable, &adapter) {
        (Some(before), Some(_)) if before.adapter.is_some() => before.repeats.saturating_add(1),
        _ => 0,
    };
    op.outcome = Some(Reason::CheckUnavailable.code().to_owned());
    op.unavailable = Some(Unavailable {
        environments: environments.clone(),
        adapter,
        at_ms: now_ms(),
        repeats,
    });
    ctx.save(
        revision,
        None,
        &format!(
            "unavailable {op_id}: no worker for {}",
            environments.join(", ")
        ),
    )?;
    ctx.push_chain();
    Ok(waiting)
}

/// An operation waiting for a worker (§ 6, § 7) is passed over with nothing written while this
/// box does not serve what it waits for, and, after an adapter's `unavailable` answer, until
/// its rejoin gap has passed ([`rejoin_gap`]). Writing nothing is what lets the lease of the
/// executor that recorded the wait lapse, so an executor that serves it takes it over (§ 9a).
fn waits_for_worker(ctx: &Ctx, op_id: &str) -> Option<BayResult> {
    let op = ctx.op(op_id);
    let waiting = op.unavailable.as_ref()?;
    if op.state.is_terminal() || op.state == OpState::Blocked {
        return None;
    }
    let served = check::Served::read(&ctx.repo.git);
    let unserved = waiting
        .environments
        .iter()
        .any(|environment| !served.serves(environment));
    let early = waiting.adapter.is_some() && {
        let gap = rejoin_gap(ctx.repo.config.lease(), waiting.repeats);
        now_ms()
            < waiting
                .at_ms
                .saturating_add(u64::try_from(gap.as_millis()).unwrap_or(u64::MAX))
    };
    if !unserved && !early {
        return None;
    }
    Some(unavailable(
        ctx,
        op_id,
        &waiting.environments,
        waiting.adapter.as_deref(),
    ))
}

/// The verdict of `policy` over every counted record on the tree (`all`): `None` for a pass,
/// else its refusal code and word, and what `named` say about each gap. `required` is the part
/// of its manifest the candidate's diff requires (G146).
fn verdict(
    policy: &policy::Policy,
    required: &[policy::ManifestEntry],
    all: &[evidence::EvidenceRecord],
    named: &[&evidence::EvidenceRecord],
    commit: &str,
) -> Option<(Reason, &'static str, Vec<serde_json::Value>)> {
    // G163: a record from a producer the check's list leaves out never answers it.
    let listed: Vec<evidence::EvidenceRecord> = policy.answering(all.to_vec());
    // G30: coverage is judged against the frozen manifest, never against counts.
    let gaps: Vec<(String, Combined)> = evidence::aggregate(
        &listed,
        required,
        &policy.manifest_digest,
        &|id| policy.commit_keyed(id),
        commit,
    )
    .into_iter()
    .map(|(id, combined)| (id.id, combined))
    .collect();
    // No gap is a pass.
    let (_, worst) = gaps
        .iter()
        .find(|(_, combined)| *combined == Combined::Flaky)
        .or_else(|| {
            gaps.iter()
                .find(|(_, combined)| *combined == Combined::ProductFailure)
        })
        .or_else(|| gaps.first())?;
    let (code, word) = match worst {
        Combined::Flaky => (Reason::CheckFlaky, "flaky"),
        Combined::ProductFailure => (Reason::CheckFailed, "product_failure"),
        Combined::Unknown | Combined::Pass => (Reason::CheckUnknown, "unknown"),
    };
    let in_gap =
        |record: &evidence::EvidenceRecord| gaps.iter().any(|(id, _)| *id == record.check.id);
    let mut why: Vec<serde_json::Value> = named
        .iter()
        .filter(|record| in_gap(record) && policy.listed(record))
        .map(|record| {
            json!({"check": record.check.id, "verdict": record.verdict, "why": record.why,
                "reasons": evidence::reasons(&record.units)})
        })
        .collect();
    // G163: each record that would have answered a gap but for the check's producers list.
    why.extend(
        all.iter()
            .filter(|record| {
                record.kind == evidence::KIND_CHECK
                    && record.manifest_digest == policy.manifest_digest
                    && in_gap(record)
                    && !policy.listed(record)
            })
            .map(|record| {
                json!({"check": record.check.id, "producer": record.producer.name,
                    "verdict": record.verdict, "why": "unlisted_producer"})
            }),
    );
    Some((code, word, why))
}

/// How the policy at `trunk` reached trunk (`DESIGN` § 10a, `INTERFACES` § 6). Each edit
/// outside bay that it still stands on is recorded as a `policy_changed_outside_bay` event
/// (`INTERFACES` § 5) in this executor's log, unless a log already holds it. Two executors
/// that land at once may both write it; its id (`evt_<commit>`) is the same in both, so a
/// reader de-duplicates (`INTERFACES` § 4). A new event is told to the notify adapter only once
/// the operation reached its outcome (`Ctx::tell_deferred`), never while it holds its lease.
fn provenance(
    ctx: &Ctx,
    trunk: &str,
    policy: &policy::Policy,
) -> Result<policy::Provenance, String> {
    // One read of the remote's landed markers serves every path (§ 10a).
    let landed: std::cell::OnceCell<std::collections::HashMap<String, String>> =
        std::cell::OnceCell::new();
    let edits = policy::outside_edits(&ctx.repo.git, trunk, policy, &|commit| {
        let markers = if let Some(markers) = landed.get() {
            markers
        } else {
            let read = landed_markers(ctx.repo)?;
            landed.get_or_init(|| read)
        };
        is_publication(&ctx.repo.git, markers, commit)
    })?;
    if edits.is_empty() {
        return Ok(policy::Provenance::Bay);
    }
    let executor = ctx.executor_name();
    let signer = ctx.executor_signer().ok().flatten();
    for edit in edits {
        let event = json!({
            "schema": crate::log::EVENT_SCHEMA,
            "id": format!("evt_{}", edit.commit),
            "at": crate::clock::now_rfc3339(),
            "kind": OUTSIDE_EVENT,
            "change": ctx.id,
            "actor": {"name": executor},
            "commit": edit.commit,
            "paths": edit.paths,
            "other_paths": edit.other_paths,
            "recurrence_key": format!("{OUTSIDE_EVENT}/{}", edit.commit),
        });
        if crate::log::append_event(ctx.repo, &executor, signer.as_ref(), &event)? {
            ctx.deferred.borrow_mut().push(event);
        }
    }
    Ok(policy::Provenance::OutsideBay)
}

const OUTSIDE_EVENT: &str = "policy_changed_outside_bay";

/// Whether trunk commit `commit` is a bay publication (`DESIGN` § 10a): it carries one
/// `Change-Id` and the remote's `refs/bay/landed/<Change-Id>` (in `markers`) names it.
fn is_publication(
    git: &Git,
    markers: &std::collections::HashMap<String, String>,
    commit: &str,
) -> Result<bool, String> {
    let change = git
        .run(&[
            "log",
            "-1",
            "--format=%(trailers:key=Change-Id,valueonly,separator=%x2c)",
            commit,
        ])
        .map_err(|error| error.message())?;
    let change = change.trim();
    if change.is_empty() || change.contains(',') {
        return Ok(false);
    }
    Ok(markers.get(change).map(String::as_str) == Some(commit))
}

/// The remote's landed markers: `Change-Id` to the commit its `refs/bay/landed/<Change-Id>`
/// names, in one `ls-remote`, and those the shard indexes record (§ 10a, one fetch of them).
/// A remote that cannot be read is an error, never "no marker":
/// an unread provenance is never taken for either answer.
fn landed_markers(repo: &Repo) -> Result<std::collections::HashMap<String, String>, String> {
    let listed = repo
        .git
        .run(&[
            "ls-remote",
            repo.remote(),
            &format!("{}*", crate::names::LANDED),
        ])
        .map_err(|error| error.message())?;
    let mut markers: std::collections::HashMap<String, String> = listed
        .lines()
        .filter_map(|line| {
            let (oid, name) = line.split_once('\t')?;
            Some((
                name.strip_prefix(crate::names::LANDED)?.to_owned(),
                oid.to_owned(),
            ))
        })
        .collect();
    // § 10a: a compacted marker's shard index record stands in for the ref.
    crate::compact::fetch(repo)?;
    for (name, target) in crate::compact::entries(&repo.git, crate::names::LANDED)? {
        if let Some(id) = name.strip_prefix(crate::names::LANDED) {
            markers.entry(id.to_owned()).or_insert(target);
        }
    }
    Ok(markers)
}

/// Renews the executor's lease while a check runs (§ 9a): one heartbeat commit on the chain,
/// signed by the executor, pushed under the lease of the head this executor last pushed. A
/// push that fails means the chain moved (a takeover); the persist step then finds out.
fn heartbeat(ctx: &Ctx, op_id: &str) {
    let git = &ctx.repo.git;
    let mut chain = Chain::new(git);
    chain.sign_with(ctx.executor_signer().ok().flatten());
    let Ok(Some(head)) = chain.head(&ctx.id) else {
        return;
    };
    let expected = ctx
        .beat_pushed
        .borrow()
        .clone()
        .or_else(|| ctx.pushed_head.clone());
    if expected.as_deref() != Some(head.as_str()) {
        return;
    }
    let (Ok(record), Ok(drafts)) = (chain.record_at(&head), chain.drafts(&ctx.id)) else {
        return;
    };
    let Ok(beat) = chain.append(
        &head,
        &record,
        &drafts[0],
        &format!("heartbeat {op_id} epoch {}", ctx.my_epoch),
    ) else {
        return;
    };
    let name = change_ref(&ctx.id);
    let pushed = git
        .run(&[
            "push",
            "-q",
            &format!("--force-with-lease={name}:{head}"),
            ctx.repo.remote(),
            &format!("{name}:{name}"),
        ])
        .is_ok();
    if pushed {
        *ctx.beat_pushed.borrow_mut() = Some(beat);
    } else {
        let _ = git.run(&["update-ref", &name, &head, &beat]);
    }
}

/// Watches an operation another executor holds for one full lease on this process's monotonic
/// clock, from its own first sighting (§ 9a). Returns whether the lease expired: the chain head
/// never changed. Any change means the owner is alive.
fn lease_expired(ctx: &mut Ctx) -> bool {
    let lease = ctx.repo.config.lease();
    let first = ctx.head.clone();
    let seen = Instant::now();
    while seen.elapsed() < lease {
        std::thread::sleep(Duration::from_millis(200).min(lease));
        if ctx.refetch().is_ok() && ctx.head != first {
            return false;
        }
    }
    true
}

/// What a check may neither read nor write (§ 7): [`check::hidden_for`].
fn hidden_paths(ctx: &Ctx) -> Vec<std::path::PathBuf> {
    check::hidden_for(ctx.repo, &ctx.keys)
}

/// What a policy's checks judge: the candidate, its base and the request's named inputs.
struct Judged<'a> {
    tree: &'a str,
    commit: &'a str,
    base_tree: &'a str,
    provenance: policy::Provenance,
    /// The request's `inputs` (`INTERFACES` § 3).
    inputs: &'a [String],
}

/// Checks that already finished on `tree` under the policy's manifest: a resume after a dead
/// worker runs only what did not finish (§ 7, G22). A check whose inputs include the commit
/// finished only for `commit` itself, so a re-signed C runs it again (ruling 9a, G33). Only
/// records that verify against the current registry count, so a revoked producer key's
/// results are rechecked. With `[policy] evidence_reuse = "exact-inputs"`, a check also
/// finished when another operation's records on the same inputs answer it (G7); those records
/// are returned beside the checks, so a refusal can name what they say.
fn finished_checks(
    ctx: &Ctx,
    op_id: &str,
    policy: &policy::Policy,
    judged: &Judged,
) -> Result<(Vec<evidence::CheckId>, Vec<evidence::EvidenceRecord>), String> {
    let git = &ctx.repo.git;
    let reuse = reuses(ctx, judged);
    // An attempt under another epoch may have run on another executor (a takeover); this
    // epoch's own attempts wrote only this producer's local records. Reuse reads every
    // producer's records on the tree.
    if reuse
        || ctx
            .op(op_id)
            .attempts
            .iter()
            .any(|attempt| attempt.epoch != ctx.my_epoch)
    {
        ctx.repo.fetch_evidence(&ctx.keys.producer_name);
    }
    // This operation's own records are a resume; another operation's are reuse, and with
    // reuse off another operation on the same tree is rechecked (G29).
    let mine = format!("/{op_id}.");
    // G163: a record from a producer the check's list leaves out answers nothing here either.
    let (resumed, others): (Vec<_>, Vec<_>) = policy
        .answering(evidence::counted(
            git,
            evidence::records_for(git, judged.tree)?,
            ctx.registry.as_ref(),
        )?)
        .into_iter()
        .partition(|record| record.id.contains(&mine));
    let digest = &policy.manifest_digest;
    let mut finished = Vec::new();
    let mut reused = Vec::new();
    for entry in &policy.manifest {
        let id = evidence::CheckId::new(&entry.id, &entry.environment);
        let subject = policy.commit_keyed(&id).then_some(judged.commit);
        if answered(&resumed, &id, digest, subject) {
            finished.push(id);
            continue;
        }
        let Some(check) = policy.check(&id).filter(|_| reuse) else {
            continue;
        };
        let candidates = evidence::reusable(
            &others,
            &evidence::ReuseKey {
                tree: judged.tree,
                base_tree: judged.base_tree,
                policy_revision: policy.revision,
                policy_provenance: judged.provenance.as_str(),
                fixtures: &check.fixtures,
            },
        );
        if answered(&candidates, &id, digest, subject) {
            reused.extend(candidates.into_iter().filter(|record| record.check == id));
            finished.push(id);
        }
    }
    Ok((finished, reused))
}

/// Whether evidence reuse applies to this judgement (G7): `[policy] evidence_reuse =
/// "exact-inputs"`, a producer registry, so that only producer-signed records stand for a run
/// (`DESIGN` § 7: an unsigned record never counts as evidence), and a request without named
/// inputs. A record does not hold the request's input versions, so it cannot show they
/// matched: a change with named inputs is always rechecked.
fn reuses(ctx: &Ctx, judged: &Judged) -> bool {
    ctx.repo.config.evidence_reuse() && ctx.registry.is_some() && judged.inputs.is_empty()
}

/// Whether `records` answer check `id` under `digest` (for `subject`, when commit-keyed):
/// they combine to a verdict, never `unknown`.
fn answered(
    records: &[evidence::EvidenceRecord],
    id: &evidence::CheckId,
    digest: &str,
    subject: Option<&str>,
) -> bool {
    matches!(
        evidence::combine(records, id, digest, subject),
        Combined::Pass | Combined::ProductFailure | Combined::Flaky
    )
}

/// Step 3: the candidate commit, signed by the publishing executor when signing is on.
fn commit_candidate(ctx: &Ctx, tree: &str, trunk: &str, message: &str) -> Result<String, String> {
    let git = &ctx.repo.git;
    let signer = ctx.executor_signer()?;
    let mut args: Vec<String> = Vec::new();
    if let Some(signer) = &signer {
        args.extend(signer.config_args());
    }
    args.extend(["commit-tree", tree, "-p", trunk].map(str::to_owned));
    args.push(
        if signer.is_some() {
            "-S"
        } else {
            "--no-gpg-sign"
        }
        .to_owned(),
    );
    args.extend(["-m".to_owned(), message.to_owned()]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let identity: Vec<(String, String)> = signer.as_ref().map_or_else(
        || {
            EXECUTOR_IDENTITY
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect()
        },
        Signer::identity,
    );
    let envs: Vec<(&str, &str)> = identity
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    git.run_with(&args, &envs, None)
        .map_err(|error| error.message())
}

/// Holds `oid` (the merged tree, then the candidate commit) under the operation's in-flight
/// ref, `refs/bay/inflight/<op id>`, local to this store and never pushed (G173). Until the
/// change chain links the candidate (step 5), no other ref holds it, and a
/// `git gc --prune=now` or a maintenance run with a short prune window would remove it
/// between `merge-tree` and the push.
fn anchor(git: &Git, op_id: &str, oid: &str) -> Result<(), String> {
    let name = crate::names::inflight_ref(op_id)?;
    git.run(&["update-ref", &name, oid])
        .map(drop)
        .map_err(|error| format!("the candidate cannot be anchored: {}", error.message()))
}

/// An operation's in-flight ref for as long as it runs: deleted when the operation reaches
/// its outcome, whatever it is (landed, blocked, failed, superseded or given up). An executor
/// killed mid-run leaves it; the operation's next run, which recovery makes, deletes it, and
/// `bay status` lists one whose operation ended as a leftover.
struct Anchored<'a> {
    git: &'a Git,
    name: String,
}

impl<'a> Anchored<'a> {
    fn new(git: &'a Git, op_id: &str) -> Result<Self, String> {
        let name = crate::names::inflight_ref(op_id)?;
        Ok(Self { git, name })
    }
}

impl Drop for Anchored<'_> {
    fn drop(&mut self) {
        let _ = self.git.run(&["update-ref", "-d", &self.name]);
    }
}

fn candidate_message(
    ctx: &Ctx,
    revision: &str,
    request_id: &str,
    candidate_id: &str,
    registry_id: &str,
) -> String {
    let config = &ctx.repo.config;
    let record = &ctx.record;
    format!(
        "{reference}: change {id}\n\nChange-Id: {id}\nRevision-Id: {revision}\nRequest-Id: {request_id}\nCandidate-Id: {candidate_id}\nRegistry-Id: {registry_id}\nBay-Epoch: {epoch}\n{intent_key}: {reference}@{intent_revision}\n{actor_key}: {actor}\nBay-Session: {session}\n{supersedes}{reverts}{fixes}",
        reference = record.intent.reference,
        id = ctx.id,
        intent_key = config.intent_trailer(),
        intent_revision = record.intent.revision,
        actor_key = config.actor_trailer(),
        epoch = ctx.my_epoch,
        supersedes = record
            .supersedes
            .as_ref()
            .map_or_else(String::new, |landed| format!("Supersedes: {landed}\n")),
        reverts = record
            .reverts
            .as_ref()
            .map_or_else(String::new, |undone| format!("Reverts: {undone}\n")),
        fixes = record
            .fixes
            .as_ref()
            .map_or_else(String::new, |fixed| format!("Fixes: {fixed}\n")),
        actor = record.actor.name,
        session = record.actor.session,
    )
}

/// Pushes the producer's notes ref and confirms the remote now holds exactly that commit.
fn push_and_read_back_evidence(repo: &Repo, producer: &str) -> Result<(), String> {
    let notes = evidence::notes_ref(producer);
    let Some(local) = repo
        .git
        .ref_target(&notes)
        .map_err(|error| error.message())?
    else {
        // This producer wrote nothing: every record the pass rests on was resumed from
        // another producer's ref, fetched from the remote (a takeover, G22).
        return Ok(());
    };
    repo.git
        .run(&["push", "-q", repo.remote(), &format!("{notes}:{notes}")])
        .map_err(|error| {
            let message = error.message();
            if message.contains("non-fast-forward") || message.contains("fetch first") || message.contains("rejected") {
                // § 7: one producer name used by two executors is a collision.
                format!(
                    "producer {producer} collides with another executor: the remote's {notes} holds records this executor never wrote; give each executor its own bay.producerName"
                )
            } else {
                format!("evidence could not be pushed: {message}")
            }
        })?;
    let remote = repo
        .git
        .run(&["ls-remote", repo.remote(), &notes])
        .map_err(|error| error.message())?;
    if remote.split_whitespace().next() == Some(local.as_str()) {
        Ok(())
    } else {
        Err(format!(
            "the remote does not hold the evidence for {notes}; nothing was published"
        ))
    }
}

/// Step 6: one atomic push of trunk and the landed marker. Never forced.
fn publish_push(
    repo: &Repo,
    id: &str,
    commit: &str,
    release: Option<&(u64, String)>,
    claim: Option<&crate::change::IntentClaim>,
) -> Result<(), String> {
    let trunk = format!("{commit}:refs/heads/{}", repo.trunk());
    let landed = format!("{commit}:{}{id}", crate::names::LANDED);
    let done =
        release.map(|(seq, release)| format!("{release}:{}{seq}", crate::queue::DONE_PREFIX));
    // § 6: each generation of an intent lands once, even if two changes were opened for it.
    let landed_intent = claim.map(|claim| format!("{commit}:{}", claim.landed_ref()));
    // The markers are create-only: a lease that expects no ref refuses one that exists, even
    // when the new value fast-forwards it (a later publication descends from an earlier one).
    let landed_lease = format!("--force-with-lease={}{id}:", crate::names::LANDED);
    let intent_lease = claim.map(|claim| format!("--force-with-lease={}:", claim.landed_ref()));
    let mut args = vec!["push", "-q", "--atomic", landed_lease.as_str()];
    if let Some(lease) = &intent_lease {
        args.push(lease);
    }
    args.extend([repo.remote(), trunk.as_str(), landed.as_str()]);
    if let Some(done) = &done {
        // § 6: trunk, the head's release and the landed marker move in one transaction.
        args.push(done);
    }
    if let Some(landed_intent) = &landed_intent {
        args.push(landed_intent);
    }
    repo.git
        .run(&args)
        .map(|_| ())
        .map_err(|error| error.message())
}

/// When trunk already holds a publication of the change, returns the result: this revision
/// is landed, or another revision landed (`CHANGE_ALREADY_LANDED`).
fn scanned(ctx: &mut Ctx, trunk: &str, revision: &str) -> Result<Option<BayResult>, String> {
    let found = scan(&ctx.repo.git, &ctx.record.base_first, trunk, &ctx.id)?;
    let Some(publication) = found.publications.first() else {
        return Ok(None);
    };
    if publication.revision == revision {
        let op_id = ctx
            .record
            .operations
            .iter()
            .find(|op| op.revision == revision)
            .map(|op| op.op_id.clone());
        let commit = publication.commit.clone();
        if let Some(op_id) = op_id {
            return derive_landed(ctx, &op_id, revision, &commit).map(Some);
        }
        // No operation of this revision in the chain, so no attempt shows a check passed:
        // judged at its base, fail closed.
        let mut result = landed_result(&ctx.id, None, &commit, revision, ctx.registry.as_ref());
        let unchecked = unchecked_reason(&ctx.repo.git, None, &commit);
        if let (Some(detail), Some(why)) = (result.detail.as_mut(), unchecked) {
            detail["unchecked"] = json!(why);
        }
        return Ok(Some(result));
    }
    Ok(Some(
        BayResult::blocked(Reason::ChangeAlreadyLanded)
            .with_change(ctx.id.clone())
            .with_detail(
                json!({"landed": publication.commit, "landed_revision": publication.revision}),
            ),
    ))
}

/// Step 7: derive "landed" from trunk, record it, and push the chain (idempotent).
fn derive_landed(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    commit: &str,
) -> Result<BayResult, String> {
    if ctx.op(op_id).state != OpState::Published || ctx.record.state != ChangeState::Landed {
        ctx.record.state = ChangeState::Landed;
        let op = ctx.op_mut(op_id);
        op.state = OpState::Published;
        op.outcome = None;
        ctx.save(revision, Some(commit), &format!("landed {op_id}"))?;
    }
    if view::journal(ctx.repo, &ctx.id).is_some() {
        view::set_journal(ctx.repo, &ctx.id, view::Journal::Published)?;
    }
    ctx.push_chain();
    write_terminal(ctx, op_id, "published", Some(commit))?;
    let mut result = landed_result(
        &ctx.id,
        Some(op_id),
        commit,
        revision,
        ctx.registry.as_ref(),
    );
    let unchecked = unchecked_landing(ctx, op_id, commit);
    if let (Some(detail), Some(why)) = (result.detail.as_mut(), unchecked) {
        detail["unchecked"] = json!(why);
    }
    // The change is published: a fault while reading its gaps or its inputs never turns the
    // landing into an error. The result says what could not be read, so none is hidden.
    let gaps = match coverage_gaps(ctx, op_id) {
        Ok(gaps) if gaps.is_empty() => None,
        Ok(gaps) => Some(("coverage_gaps", serde_json::Value::Array(gaps))),
        Err(why) => Some(("coverage_gaps_unread", serde_json::Value::String(why))),
    };
    // G146: a check the landing's diff did not require is named, never silently skipped.
    let skipped = match not_required(ctx, op_id) {
        Ok(skipped) if skipped.is_empty() => None,
        Ok(skipped) => Some(("not_required", json!(skipped))),
        Err(why) => Some(("not_required_unread", serde_json::Value::String(why))),
    };
    // G91: the post-publication checks, pending for the landed commit until their records
    // arrive.
    let post = match post_checks(&ctx.repo.git, commit, ctx.registry.as_ref()) {
        Ok(post) if post.is_empty() => None,
        Ok(post) => Some(("post_checks", serde_json::Value::Array(post))),
        Err(why) => Some(("post_checks_unread", serde_json::Value::String(why))),
    };
    // § 8: every answer carries its coverage; a landing of a change with named inputs lists
    // them as judged at the trunk it landed on.
    let base = ctx
        .op(op_id)
        .attempts
        .last()
        .map(|attempt| attempt.base.clone());
    let judged = match base {
        Some(base) => match inputs::judge(ctx.repo, &ctx.record, &base, inputs::Probe::Run) {
            Ok(judged) if judged.is_empty() => None,
            Ok(judged) => Some(("inputs", inputs::to_json(judged.iter()))),
            Err(why) => Some(("inputs_unread", serde_json::Value::String(why))),
        },
        None => None,
    };
    if let Some(detail) = result
        .detail
        .as_mut()
        .and_then(|detail| detail.as_object_mut())
    {
        for (key, value) in gaps.into_iter().chain(skipped).chain(post).chain(judged) {
            detail.insert(key.to_owned(), value);
        }
    }
    Ok(result)
}

/// G30: the checks the landing passed without testing code, each a coverage gap: a check
/// that passed on a `declared_empty` selection under a policy that judged it (§ 7), and each
/// quarantined check that no policy judging the landing required (ruling 8c).
///
/// The judging policies are read from the base and the candidate, never from the request, so
/// a replay gives the same answer: the base's policy when it is valid, and the candidate's own
/// when it differs (a policy change is checked by both; a registry-signed repair of an
/// invalid base policy by its own alone).
fn coverage_gaps(ctx: &Ctx, op_id: &str) -> Result<Vec<serde_json::Value>, String> {
    let Some(attempt) = ctx.op(op_id).attempts.last() else {
        return Ok(Vec::new());
    };
    let git = &ctx.repo.git;
    let mut judged: Vec<policy::Policy> = Vec::new();
    for at in [&attempt.base, &attempt.commit] {
        match policy::load_at(git, at) {
            Ok(Some(policy))
                if !judged.iter().any(|other| {
                    other.manifest_digest == policy.manifest_digest
                        && other.quarantined == policy.quarantined
                }) =>
            {
                judged.push(policy);
            }
            Ok(_) | Err(policy::LoadError::Invalid(_)) => {}
            Err(policy::LoadError::Git(why)) => return Err(why),
        }
    }
    if judged.is_empty() {
        return Ok(Vec::new());
    }
    let records = evidence::counted(
        git,
        evidence::records_for(git, &attempt.tree)?,
        ctx.registry.as_ref(),
    )?;
    let mut gaps: Vec<serde_json::Value> = Vec::new();
    for policy in &judged {
        for entry in &policy.manifest {
            let empty_pass = records.iter().any(|record| {
                policy.listed(record)
                    && record.check.id == entry.id
                    && record.check.environment == entry.environment
                    && record.manifest_digest == policy.manifest_digest
                    && record.verdict == evidence::Verdict::Pass
                    && record.selection.as_deref() == Some(check::DECLARED_EMPTY)
            });
            let gap = json!({
                "check": entry.id,
                "environment": entry.environment,
                "selection": check::DECLARED_EMPTY,
            });
            if empty_pass && !gaps.contains(&gap) {
                gaps.push(gap);
            }
        }
    }
    // Ruling 8c: a quarantined check that no judging policy required was never tested.
    for quarantine in judged.iter().flat_map(|policy| policy.quarantined.iter()) {
        let required = judged.iter().any(|policy| {
            policy.manifest.iter().any(|entry| {
                entry.id == quarantine.id && entry.environment == quarantine.environment
            })
        });
        let gap = json!({
            "check": quarantine.id,
            "environment": quarantine.environment,
            "quarantine": {"reason": quarantine.reason, "end": quarantine.end},
        });
        if !required && !gaps.contains(&gap) {
            gaps.push(gap);
        }
    }
    Ok(gaps)
}

/// G146: the checks that no policy judging the landing required, because the landing's diff
/// against its base changed no path their `when_paths` match, each with the reason. Read from
/// the base and the candidate like [`coverage_gaps`], so a replay gives the same answer.
fn not_required(ctx: &Ctx, op_id: &str) -> Result<Vec<policy::NotRequired>, String> {
    let Some(attempt) = ctx.op(op_id).attempts.last() else {
        return Ok(Vec::new());
    };
    let git = &ctx.repo.git;
    let changed = check::changed_paths(git, &attempt.base, &attempt.commit)?;
    let mut required: Vec<policy::ManifestEntry> = Vec::new();
    let mut skipped: Vec<policy::NotRequired> = Vec::new();
    for at in [&attempt.base, &attempt.commit] {
        match policy::load_at(git, at) {
            Ok(Some(policy)) => {
                let (by, not_by) = policy.required(&changed);
                required.extend(by);
                for entry in not_by {
                    if !skipped.contains(&entry) {
                        skipped.push(entry);
                    }
                }
            }
            Ok(None) | Err(policy::LoadError::Invalid(_)) => {}
            Err(policy::LoadError::Git(why)) => return Err(why),
        }
    }
    skipped.retain(|entry| {
        !required
            .iter()
            .any(|by| by.id == entry.check && by.environment == entry.environment)
    });
    Ok(skipped)
}

/// G91: the post-publication checks of landed `commit`, judged in the policy at its base
/// (`commit^1`), each with its state on the commit's tree: `pending` until a counted record of
/// it arrives under that policy's manifest, then the records' combined verdict; `not_required`,
/// with the reason, when the landing's diff changed no path its `when_paths` match (G146). None
/// when the base has no valid policy: then nothing was judged there.
///
/// # Errors
///
/// A git fault, or an unreadable note.
pub fn post_checks(
    git: &Git,
    commit: &str,
    registry: Option<&crate::registry::Registry>,
) -> Result<Vec<serde_json::Value>, String> {
    post_answers(git, commit, registry, false)
}

/// [`post_checks`], and with `recorded` the answers on `commit` whatever its diff: a recorded
/// run of a check its `when_paths` did not require still says what that commit's tree does
/// (G244: a later pass on trunk turns it green).
///
/// # Errors
///
/// A git fault, or an unreadable note.
pub fn post_answers(
    git: &Git,
    commit: &str,
    registry: Option<&crate::registry::Registry>,
    recorded: bool,
) -> Result<Vec<serde_json::Value>, String> {
    let base = format!("{commit}^1");
    let policy = match policy::load_at(git, &base) {
        Ok(Some(policy)) if !policy.post.is_empty() => policy,
        Ok(_) | Err(policy::LoadError::Invalid(_)) => return Ok(Vec::new()),
        Err(policy::LoadError::Git(why)) => return Err(why),
    };
    let changed = check::changed_paths(git, &base, commit)?;
    // G163: only records from the producers a check lists answer it.
    let records = policy.answering(evidence::counted(
        git,
        evidence::records_for(git, &tree_of(git, commit)?)?,
        registry,
    )?);
    let mut out = Vec::new();
    for entry in &policy.post {
        let id = evidence::CheckId::new(&entry.id, &entry.environment);
        let mut row = json!({"check": entry.id, "environment": entry.environment});
        if let Some(skipped) = policy
            .check(&id)
            .and_then(|check| policy.unwoken(check, &changed))
            .filter(|_| !recorded)
        {
            row["state"] = json!("not_required");
            row["reason"] = json!(skipped.reason);
            out.push(row);
            continue;
        }
        let subject = policy.commit_keyed(&id).then_some(commit);
        let state = match evidence::combine(&records, &id, &policy.manifest_digest, subject) {
            Combined::Pass => "pass",
            Combined::ProductFailure => "product_failure",
            Combined::Flaky => "flaky",
            // Only an `unknown` or `infra_failure` record, or none: nothing has answered it.
            Combined::Unknown => {
                let recorded = records.iter().any(|record| {
                    record.kind == evidence::KIND_CHECK
                        && record.check == id
                        && record.manifest_digest == policy.manifest_digest
                        && subject.is_none_or(|commit| record.subject.commit == commit)
                });
                if recorded { "unknown" } else { "pending" }
            }
        };
        row["state"] = json!(state);
        out.push(row);
    }
    Ok(out)
}

/// The compact terminal record that outlives the drafts (§ 9a). Written once per operation.
fn write_terminal(
    ctx: &Ctx,
    op_id: &str,
    outcome: &str,
    published: Option<&str>,
) -> Result<(), String> {
    let executor = ctx.executor_name();
    crate::log::fetch(ctx.repo);
    let existing = crate::log::records_for(&ctx.repo.git, &ctx.id)?;
    if existing
        .iter()
        .any(|record| record["op_id"] == op_id && record["outcome"] == outcome)
    {
        return Ok(());
    }
    let op = ctx.op(op_id);
    let attempt = op.attempts.last();
    let mut record = json!({
        "schema": "bay.terminal/1",
        "op_id": op_id,
        "change": ctx.id,
        "request_digest": op.request_digest,
        "request": op.request_id,
        "revision": op.revision,
        "outcome": outcome,
        "published": published,
        "evidence_tree": attempt.map(|attempt| attempt.tree.clone()),
        "candidate": attempt.map(|attempt| attempt.candidate_id.clone()),
        "executor": executor,
        "policy_repair": ctx.repair,
    });
    if let Some(why) = published
        .filter(|_| outcome == "published")
        .and_then(|commit| unchecked_landing(ctx, op_id, commit))
    {
        // Executor-signed with the record, so status never shows this landing as checked.
        record["unchecked"] = json!(why);
    }
    if outcome == "published" {
        // G146: what the landing did not require, and why, outlives its drafts.
        match not_required(ctx, op_id) {
            Ok(skipped) if skipped.is_empty() => {}
            Ok(skipped) => record["not_required"] = json!(skipped),
            Err(why) => record["not_required_unread"] = json!(why),
        }
    }
    let signer = ctx.executor_signer().ok().flatten();
    crate::log::append(ctx.repo, &executor, signer.as_ref(), &record).map(|_| ())
}

/// Why landed `commit` was never checked (the missing-policy and unchecked-base rulings), or
/// `None` when it was. `verdict` is the verdict of the attempt that built `commit`, when the
/// chain holds it. Judged at the base (`commit^1`), and fail closed: a base that cannot be
/// read, or a missing attempt where only a passing one would show a check ran, never reads as
/// checked.
#[must_use]
pub fn unchecked_reason(git: &Git, verdict: Option<&str>, commit: &str) -> Option<&'static str> {
    if verdict == Some(UNCHECKED) {
        return Some(UNCHECKED_BY_POLICY);
    }
    let passed = verdict == Some("pass");
    match policy::load_at(git, &format!("{commit}^1")) {
        Ok(Some(base)) if !base.unchecked => None,
        // Only a registry-signed repair runs checks under an unchecked base (its own new
        // policy's), and its attempt says they passed.
        Ok(Some(_)) => (!passed).then_some(UNCHECKED_BY_POLICY),
        // Only a registry-signed repair lands on a base without a policy, judged by its own
        // new policy alone: checked only when that policy has checks and they passed.
        Ok(None) => match policy::load_at(git, commit) {
            Ok(Some(own)) if passed && !own.unchecked => None,
            _ => Some("no policy at its base"),
        },
        // An invalid base policy judges nothing; only a passing repair lands over it.
        Err(policy::LoadError::Invalid(_)) => {
            (!passed).then_some("the policy at its base is invalid")
        }
        Err(policy::LoadError::Git(_)) => Some("the policy at its base could not be read"),
    }
}

/// [`unchecked_reason`] for `commit`, published by `op_id`, judged by the attempt that built
/// it.
fn unchecked_landing(ctx: &Ctx, op_id: &str, commit: &str) -> Option<&'static str> {
    let verdict = ctx
        .op(op_id)
        .attempts
        .iter()
        .rev()
        .find(|attempt| attempt.commit == commit)
        .map(|attempt| attempt.verdict.as_str());
    unchecked_reason(&ctx.repo.git, verdict, commit)
}

/// A landing, with the `trust` its evidence was judged under (G160).
fn landed_result(
    id: &str,
    op_id: Option<&str>,
    commit: &str,
    revision: &str,
    registry: Option<&Registry>,
) -> BayResult {
    let mut result = BayResult::new(Decision::Landed)
        .with_change(id)
        .with_detail(json!({
            "commit": commit,
            "revision": revision,
            "trust": evidence::trust(registry),
        }));
    result.published = Some(true);
    if let Some(op_id) = op_id {
        result = result.with_op(op_id);
    }
    result
}

fn tree_of(git: &Git, commit: &str) -> Result<String, String> {
    git.run(&["rev-parse", &format!("{commit}^{{tree}}")])
        .map_err(|error| error.message())
}

fn parent_of(git: &Git, commit: &str) -> Result<Option<String>, String> {
    match git.run(&["rev-parse", "--verify", "-q", &format!("{commit}^1")]) {
        Ok(parent) => Ok(Some(parent)),
        Err(GitError::Failed { .. }) => Ok(None),
        Err(error) => Err(error.message()),
    }
}

/// Whether `ancestor` is an ancestor of (or equal to) `descendant`.
///
/// # Errors
///
/// A git fault.
pub fn is_ancestor(git: &Git, ancestor: &str, descendant: &str) -> Result<bool, String> {
    let (code, _, stderr) = git
        .run_status(&["merge-base", "--is-ancestor", ancestor, descendant])
        .map_err(|error| error.message())?;
    match code {
        0 => Ok(true),
        1 => Ok(false),
        _ => Err(format!("merge-base --is-ancestor: {stderr}")),
    }
}

/// What a retention cut keeps, and since when the change has been closed.
struct Kept {
    /// Unix seconds of the landing or the abandon.
    since: u64,
    /// The commits the cut head links as parents.
    links: Vec<String>,
    /// For a landing: the published operation and commit, for its terminal record.
    terminal: Option<(String, String)>,
}

fn commit_time(git: &Git, commit: &str) -> Result<u64, String> {
    Ok(git
        .run(&["log", "-1", "--format=%ct", commit])
        .map_err(|error| error.message())?
        .trim()
        .parse()
        .unwrap_or(0))
}

/// A landed change keeps the landed revision, the published commit and the latest land request.
fn landed_keeps(ctx: &Ctx) -> Result<Option<Kept>, String> {
    let Some(op) = ctx
        .record
        .operations
        .iter()
        .rev()
        .find(|op| op.state == OpState::Published)
    else {
        return Ok(None);
    };
    let Some(published) = op.attempts.last().map(|attempt| attempt.commit.clone()) else {
        return Ok(None);
    };
    let mut links = vec![op.revision.clone(), published.clone()];
    links.extend(op.request_id.clone().filter(|request| !request.is_empty()));
    Ok(Some(Kept {
        since: commit_time(&ctx.repo.git, &published)?,
        links,
        terminal: Some((op.op_id.clone(), published)),
    }))
}

/// An abandoned change keeps what its abandoning state commit links (the latest draft, which an
/// adopter re-roots from, and the abandon request) and the latest land request, if any (§ 10a).
fn abandoned_keeps(ctx: &Ctx) -> Result<Option<Kept>, String> {
    let git = &ctx.repo.git;
    let parents = git
        .run(&["rev-list", "--parents", "-n", "1", &ctx.head])
        .map_err(|error| error.message())?;
    let mut links: Vec<String> = parents
        .split_whitespace()
        .skip(2)
        .map(str::to_owned)
        .collect();
    if let Some(request) = git
        .ref_target(&format!("{}{}", crate::names::REQUESTS, ctx.id))
        .map_err(|error| error.message())?
        && !links.contains(&request)
    {
        links.push(request);
    }
    Ok(Some(Kept {
        since: commit_time(git, &ctx.head)?,
        links,
        terminal: None,
    }))
}

/// The retention cut (§ 5): once a landed or abandoned change is older than the retention
/// window, its chain is cut to one head that keeps what still matters. Superseded drafts become
/// collectable; a landed change's terminal record stays in the log.
///
/// # Errors
///
/// A fault of bay's own.
pub fn retain(repo: &Repo, id: &str) -> Result<Option<String>, String> {
    let step = Cell::new("retention");
    let mut ctx = Ctx::load(repo, &step, id)?;
    if ctx.refetch().is_err() || ctx.record.retention_cut {
        return Ok(None);
    }
    let kept = match ctx.record.state {
        ChangeState::Landed => landed_keeps(&ctx)?,
        ChangeState::Abandoned => abandoned_keeps(&ctx)?,
        _ => None,
    };
    let Some(kept) = kept else {
        return Ok(None);
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    if now.saturating_sub(kept.since) < repo.config.retention_window().as_secs() {
        return Ok(None);
    }
    // The cut must be signed by an executor key, so read the registry even when no operation
    // on this machine fetched it.
    repo.fetch_registry();
    ctx.registry = Registry::current(&repo.git)?;
    let executor = ctx.executor_signer()?;
    ctx.chain.sign_with(executor);
    if let Some((op_id, published)) = &kept.terminal {
        write_terminal(&ctx, op_id, "published", Some(published))?;
    }
    let mut record = ctx.record.clone();
    record.retention_cut = true;
    record.operations.clear();
    record.receipts.clear();
    let links: Vec<&str> = kept.links.iter().map(String::as_str).collect();
    let head = ctx
        .chain
        .cut(&ctx.head, &record, &links, &format!("retention cut {id}"))
        .map_err(|error| error.message())?;
    ctx.head.clone_from(&head);
    ctx.record = record;
    if !ctx.push_chain() {
        return Err(format!("the retention cut of {id} could not be pushed"));
    }
    Ok(Some(head))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::{UNCHECKED, UNCHECKED_BY_POLICY, unchecked_reason};
    use crate::git::Git;

    /// A test pause ends: at its release, at its time limit, or once its parent is gone.
    #[test]
    fn a_test_pause_never_waits_forever() {
        use std::time::{Duration, Instant};
        let dir = tempfile::TempDir::new().unwrap();
        let release = dir.path().join("release");
        let started = Instant::now();
        assert!(!super::released(
            &release,
            Duration::from_millis(200),
            || true
        ));
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the limit ends it"
        );
        let started = Instant::now();
        assert!(!super::released(&release, Duration::from_secs(600), || {
            false
        }));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a gone parent ends it"
        );
        std::fs::write(&release, "").unwrap();
        assert!(super::released(&release, Duration::from_secs(600), || {
            false
        }));
        assert!(super::parent_alive()(), "this test's own parent is there");
    }

    const CHECKED: &str = "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n";
    const UNCHECKED_POLICY: &str = "revision = 1\nunchecked = true\n";

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@bay.invalid"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// A commit holding `own` as `.bay/policy.toml` whose parent holds `base` (`None`: no
    /// policy); returns (dir, git, commit).
    fn landing(base: Option<&str>, own: Option<&str>) -> (tempfile::TempDir, Git, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        std::fs::create_dir_all(root.join(".bay")).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(root.join("scripts/bay-check"), "#!/bin/sh\n").unwrap();
        for (policy, message) in [(base, "base"), (own, "landing")] {
            let path = root.join(".bay/policy.toml");
            match policy {
                Some(text) => std::fs::write(&path, text).unwrap(),
                None => {
                    let _ = std::fs::remove_file(&path);
                }
            }
            std::fs::write(root.join("a"), message).unwrap();
            git(root, &["add", "-A"]);
            git(root, &["commit", "-q", "-m", message]);
        }
        let commit = git(root, &["rev-parse", "HEAD"]);
        let git = Git::new(root);
        (dir, git, commit)
    }

    /// Judged at the base and fail closed: a landing reads as checked only when a checked base
    /// judged it, or a passing attempt on the repair path shows its own new checks ran.
    #[test]
    fn a_landing_reads_as_checked_only_when_a_check_judged_it() {
        // `unchecked = true` beside a `[[check]]` is invalid.
        let invalid = CHECKED.replacen("revision = 1\n", "revision = 1\nunchecked = true\n", 1);
        let own = CHECKED.replacen("revision = 1", "revision = 2", 1);

        let (_dir, git, commit) = landing(Some(CHECKED), Some(CHECKED));
        assert_eq!(unchecked_reason(&git, None, &commit), None);
        assert_eq!(unchecked_reason(&git, Some("pass"), &commit), None);
        assert_eq!(
            unchecked_reason(&git, Some(UNCHECKED), &commit),
            Some(UNCHECKED_BY_POLICY)
        );

        // Under an unchecked base a candidate's own checks never judge its landing; only a
        // passing attempt (the repair path) shows that a check ran.
        let (_dir, git, commit) = landing(Some(UNCHECKED_POLICY), Some(&own));
        assert_eq!(
            unchecked_reason(&git, None, &commit),
            Some(UNCHECKED_BY_POLICY)
        );
        assert_eq!(unchecked_reason(&git, Some("pass"), &commit), None);

        let (_dir, git, commit) = landing(None, Some(&own));
        assert_eq!(
            unchecked_reason(&git, None, &commit),
            Some("no policy at its base")
        );
        assert_eq!(unchecked_reason(&git, Some("pass"), &commit), None);
        let (_dir, git, commit) = landing(None, None);
        assert_eq!(
            unchecked_reason(&git, Some("pass"), &commit),
            Some("no policy at its base")
        );

        // An invalid base judges nothing: unknown, never checked.
        let (_dir, git, commit) = landing(Some(&invalid), Some(&own));
        assert_eq!(
            unchecked_reason(&git, None, &commit),
            Some("the policy at its base is invalid")
        );
    }
}
