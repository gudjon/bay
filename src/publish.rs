//! Publication: freeze → merge-tree → commit-tree → check → persist → push → derive
//! (`DESIGN` § 6).
//!
//! Only the trunk push makes a change landed. Before it, everything is retained in the
//! change chain and the evidence notes; after it, "landed" is derived from trunk. Every
//! attempt first scans `base_first..B` for an earlier publication of the change.
//!
//! `bay merge` does the agent's part and returns at once; `bay runner run` (or `land --wait`)
//! does the rest. After two moves of trunk the operation joins the publication queue (§ 6).

use std::cell::Cell;
use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::canonical::digest;
use crate::change::{
    Attempt, Chain, ChainError, ChangeRecord, ChangeState, HeldChain, OpState, Operation, Precheck,
    Unavailable, change_ref, freeze_draft,
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
use crate::throughput;
use crate::view;

/// The producer name of the built-in local executor's evidence.
pub const LOCAL_PRODUCER: &str = "local";
/// After this many `MAIN_MOVED` retries an operation joins the queue instead of rebuilding again.
const TRUNK_MOVED_LIMIT: u32 = 2;

/// The identity bay commits candidates under until executor keys exist (item 11).
const EXECUTOR_IDENTITY: &[(&str, &str)] = &[
    ("GIT_AUTHOR_NAME", "bay runner"),
    ("GIT_AUTHOR_EMAIL", "executor@bay.invalid"),
    ("GIT_COMMITTER_NAME", "bay runner"),
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
        return BayResult::failed("run bay merge in a change's view, or name the change");
    };
    // G56 (ruling T9): the talks on this change, fetched before the landing and named in its
    // result. A talk never blocks a landing.
    let open_talks = crate::talk::open_on(&repo, &id);
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
            None if wait => wait_land(&repo, &id, &step),
            None => submit_land(&repo, &id, &step, false),
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
    crate::talk::name_open_talks(&mut result, open_talks);
    result
}

/// `land --wait` (`INTERFACES` § 1: run the publication here and return its outcome). A claim
/// another process holds, or one a race moved on, is no outcome of the operation: the lander
/// reads the chain again and goes on until the operation has one. It runs it, waits out the
/// holder's lease and takes it over, or reports the holder's landing or refusal. It never
/// answers `ok` for an operation it did not see land.
fn wait_land(repo: &Repo, id: &str, step: &Cell<&'static str>) -> Result<BayResult, String> {
    loop {
        let result = submit_land(repo, id, step, true)?;
        if !not_taken(&result) {
            return Ok(result);
        }
        adopt_remote_chain(repo, id);
        let (_, record) = Chain::new(&repo.git)
            .read(id)
            .map_err(|error| error.message())?;
        // A live operation, or one another process landed, is the next pass's to run or to
        // report; one that ended otherwise is stated as it ended.
        match record.operations.last() {
            Some(op) if !op.state.is_terminal() || op.state == OpState::Published => {}
            latest => return Ok(ended_elsewhere(repo, id, &result, latest)),
        }
        // The holder's chain moves at most once a heartbeat: never spin on it.
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Whether `result` is a claim this process did not take (§ 9a): another process holds the
/// operation (`lease: held`), or a race moved it on (`superseded`). Neither is the
/// operation's outcome.
fn not_taken(result: &BayResult) -> bool {
    result.decision == Decision::Ok
        && result
            .detail
            .as_ref()
            .is_some_and(|detail| detail["superseded"] == true || detail["lease"] == "held")
}

/// The answer of a `land --wait` whose operation another process ended without a landing:
/// `ok` only for `no_effect` (`INTERFACES` § 1), else the refusal or fault it ended in.
fn ended_elsewhere(
    repo: &Repo,
    id: &str,
    taken: &BayResult,
    latest: Option<&Operation>,
) -> BayResult {
    let op_id = latest
        .map(|op| op.op_id.clone())
        .or_else(|| taken.op_id.clone())
        .unwrap_or_default();
    let state = latest.map(|op| op.state);
    let outcome = latest.and_then(|op| op.outcome.clone());
    if state == Some(OpState::NoEffect) {
        let trunk = repo
            .git
            .ref_target(&repo.trunk_tracking_ref())
            .ok()
            .flatten();
        let mut result = BayResult::new(Decision::Ok)
            .with_change(id)
            .with_op(op_id)
            .with_detail(json!({"trunk_commit": trunk}));
        result.state = Some(NO_EFFECT.to_owned());
        return result;
    }
    if state == Some(OpState::Failed) {
        return BayResult::failed(format!(
            "operation {op_id} failed in another process: {}",
            outcome.unwrap_or_default()
        ))
        .with_change(id)
        .with_op(op_id);
    }
    BayResult::blocked(Reason::ChangeSuperseded)
        .with_change(id)
        .with_op(op_id)
        .with_detail(json!({"state": state, "outcome": outcome, "landed": false}))
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
    // This version's own file, so an update meanwhile never changes the run's version.
    let bay = crate::update::own_binary().ok_or("cannot find the running bay")?;
    let mut run = std::process::Command::new(bay);
    run.arg("-C")
        .arg(dir)
        .args(["merge", "--wait", id])
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
/// names, under the executor that committed it. A commit that names no registry verifies only
/// when it landed under `[policy] signing = "approvals"` at its parent (N-21), where landings are
/// unsigned and trailers decide, as without a registry.
#[must_use]
pub fn verify_publication(git: &Git, commit: &str, registry_id: &str, committer: &str) -> bool {
    if registry_id.is_empty() || registry_id == "none" {
        return !crate::registry::landing_signed(git, commit);
    }
    // A key revoked since (owner ruling 2026-10-22) verifies under no version, also the one the
    // merge was signed under.
    let current = Registry::current(git).ok().flatten();
    Registry::load_at(git, registry_id).is_ok_and(|registry| {
        let registry = match &current {
            Some(current) => registry.without_revoked(current),
            None => registry,
        };
        verifies_under(git, commit, &registry, committer)
    })
}

/// Whether `commit` verifies against `registry`'s executor file, under the executor that
/// committed it.
#[must_use]
pub fn verifies_under(git: &Git, commit: &str, registry: &Registry, committer: &str) -> bool {
    let Ok(executors) = registry.class_file(git, KeyClass::Runner) else {
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
    /// The head this process's first write not yet on the remote went on; `None` once every
    /// write it made is pushed or withdrawn. [`Ctx::take_back`] takes those writes off the
    /// local chain ref, and only those, at every re-read of the remote chain and when the
    /// context ends, whichever path wrote them: claims and state saves alike (G294). A
    /// heartbeat takes its own back at once ([`heartbeat`]). Any other local work the remote
    /// lacks is kept (G289).
    unpushed_from: Option<String>,
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
    /// A check of this operation runs ahead of its turn (`crate::throughput`): this process
    /// does not hold it, so no heartbeat renews a lease on its chain.
    ahead_of_turn: bool,
    /// G284: the operation this process re-armed from `APPROVAL_PENDING` because the evidence
    /// on its candidate, or trunk, changed ([`retry_blocked`]): that candidate is checked
    /// again, never rebuilt, while trunk stands where it was merged onto
    /// ([`approved_candidate`]).
    approved: Option<String>,
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
            unpushed_from: None,
            my_epoch: 0,
            repair: None,
            online: true,
            beat_pushed: std::cell::RefCell::new(None),
            deferred: std::cell::RefCell::new(Vec::new()),
            retries_here: 0,
            ahead_of_turn: false,
            approved: None,
        })
    }

    /// Tells the notify adapter, once each, of the events this run deferred (ruling R6).
    fn tell_deferred(&self) {
        for event in self.deferred.take() {
            let _ = crate::notify::notify(self.repo, &event);
        }
    }

    /// The registry when it signs everything, `None` without one or under `[policy] signing =
    /// "approvals"` (N-21), where landings, requests, chains and check evidence are unsigned.
    fn signing(&self) -> Option<&Registry> {
        Registry::signing(self.registry.as_ref())
    }

    /// Whether a record may stand for a run on base `base` (`DESIGN` § 7): every record is
    /// producer-signed (a registry that signs everything), or evidence counts unsigned (no
    /// registry, or `signing = "approvals"`) and the policy at `base` says `[policy]
    /// reuse_unsigned = true` (G318). There the team already takes an unsigned record as the
    /// check's own pass, so taking it for an equal tree adds no trust. Only the base's policy
    /// says so, never a candidate's own: no candidate turns reuse on for its own landing.
    fn reuse_trusted(&self, base: &str) -> bool {
        self.signing().is_some()
            || matches!(
                policy::load_at(&self.repo.git, base),
                Ok(Some(policy)) if policy.reuse_unsigned
            )
    }

    /// The agent's signer: its key under the change's actor name. Required with a registry
    /// that signs everything.
    fn agent_signer(&self) -> Result<Option<Signer>, String> {
        if self.signing().is_none() {
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

    /// The executor's signer. Required with a registry that signs everything (N-21: never
    /// under `signing = "approvals"`).
    fn executor_signer(&self) -> Result<Option<Signer>, String> {
        if self.signing().is_none() {
            return Ok(None);
        }
        self.keys.executor.clone().map(Some).ok_or_else(|| {
            "the repository has a key registry and this machine has no runner key (git config bay.runnerKey)".to_owned()
        })
    }

    /// The producer name evidence is recorded under, and its signer when signing is on for
    /// everything (N-21: check evidence is unsigned under `signing = "approvals"`).
    fn producer(&self) -> Result<(String, Option<Signer>), String> {
        if self.signing().is_none() {
            return Ok((crate::signing::own_producer_name(&self.repo.git), None));
        }
        let name = self.keys.producer_name.clone();
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
        if !self
            .repo
            .git
            .is_ancestor(&self.record.observed_remote, &trunk)?
        {
            return Ok(Err(BayResult::failed(format!(
                "main was rewound: {} is no longer an ancestor of the remote main ({trunk}); bay never forces",
                self.record.observed_remote
            ))
            .with_change(self.id.clone())));
        }
        self.record.observed_remote.clone_from(&trunk);
        Ok(Ok(trunk))
    }

    /// The operation `op_id` of the record as this process last read it, if it holds one. A
    /// re-read of the remote chain ([`Ctx::refetch`]) can lose an operation this process
    /// appended but never got onto the remote: every lookup after a re-read goes through here.
    fn find_op(&self, op_id: &str) -> Option<&Operation> {
        self.record.operations.iter().find(|op| op.op_id == op_id)
    }

    fn find_op_mut(&mut self, op_id: &str) -> Option<&mut Operation> {
        self.record
            .operations
            .iter_mut()
            .find(|op| op.op_id == op_id)
    }

    /// The operation `op_id`, which this process's record holds: one it appended or read, and
    /// no re-read of the remote chain dropped since ([`Ctx::find_op`] is for after a re-read).
    fn op_mut(&mut self, op_id: &str) -> &mut Operation {
        self.find_op_mut(op_id)
            .unwrap_or_else(|| unreachable!("operation {op_id} is in the record"))
    }

    fn op(&self, op_id: &str) -> &Operation {
        self.find_op(op_id)
            .unwrap_or_else(|| unreachable!("operation {op_id} is in the record"))
    }

    fn save(&mut self, draft: &str, candidate: Option<&str>, message: &str) -> Result<(), String> {
        let head = self
            .chain
            .append_linking(&self.head, &self.record, draft, candidate, message)
            .map_err(|error| error.message())?;
        self.wrote(head);
        Ok(())
    }

    /// This process wrote `head` on its head: until a push takes it, it is unpushed.
    fn wrote(&mut self, head: String) {
        let before = std::mem::replace(&mut self.head, head);
        self.unpushed_from.get_or_insert(before);
    }

    /// [`Ctx::save`] that tells a lost compare-and-swap from a fault: `false` when the local
    /// chain ref is no longer the head this process read, and nothing was written. Every bay
    /// process on this box shares that ref, and a re-read of the remote chain moves it.
    fn save_cas(
        &mut self,
        draft: &str,
        candidate: Option<&str>,
        message: &str,
    ) -> Result<bool, String> {
        match self
            .chain
            .append_linking(&self.head, &self.record, draft, candidate, message)
        {
            Ok(head) => {
                self.wrote(head);
                Ok(true)
            }
            Err(ChainError::Moved { .. }) => Ok(false),
            Err(error) => Err(error.message()),
        }
    }

    /// G298: [`Ctx::save`] for a land's own write before anything runs: a lost compare-and-swap
    /// is [`CHAIN_MOVED`], which `land` answers by reading the chain again and deciding from
    /// what it finds, never a fault for a race the agent did nothing wrong in.
    fn save_or_moved(
        &mut self,
        draft: &str,
        candidate: Option<&str>,
        message: &str,
    ) -> Result<(), String> {
        if self.save_cas(draft, candidate, message)? {
            Ok(())
        } else {
            Err(CHAIN_MOVED.to_owned())
        }
    }

    /// Pushes the chain. It only appends, so this is a fast-forward and never forced; once this
    /// process has pushed, the push is leased on that head, so it fails when another executor
    /// moved the chain since. It pushes this process's own head, never the local ref: another
    /// process on this box may have moved that ref, and a push of it could succeed without
    /// carrying this process's write.
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
        let refspec = format!("{}:{name}", self.head);
        args.extend([self.repo.remote(), refspec.as_str()]);
        let pushed = self.repo.git.run(&args).is_ok();
        if pushed {
            self.pushed_head = Some(self.head.clone());
            self.unpushed_from = None;
            self.catch_up_local();
        }
        pushed
    }

    /// Pushes the chain and moves `refs/bay/requests/<id>` to `request` in one atomic push
    /// (§ 10a). The requests ref always names the latest land request. Like
    /// [`Ctx::push_chain`] it pushes this process's own objects, never the local refs.
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
        let chain = format!("{}:{name}", self.head);
        let request_spec = format!("+{request}:{requests}");
        args.extend([self.repo.remote(), chain.as_str(), request_spec.as_str()]);
        let pushed = self.repo.git.run(&args).is_ok();
        if pushed {
            self.pushed_head = Some(self.head.clone());
            self.unpushed_from = None;
            self.catch_up_local();
        }
        pushed
    }

    /// After this process pushed its head: a local chain ref that another process moved back
    /// to an ancestor of it (its re-read of the remote) moves on to it, so the next
    /// compare-and-swap on this box starts from what the remote holds.
    fn catch_up_local(&self) {
        let name = change_ref(&self.id);
        let Ok(Some(local)) = self.repo.git.ref_target(&name) else {
            return;
        };
        if local != self.head
            && self
                .repo
                .git
                .is_ancestor(&local, &self.head)
                .unwrap_or(false)
        {
            let _ = self
                .repo
                .git
                .run(&["update-ref", &name, &self.head, &local]);
        }
    }

    /// Takes this process's unpushed head off the local chain ref: back to `before`, the head
    /// it wrote on, unless another process on this box moved the ref meanwhile. A write that
    /// never reached the remote then stays this process's own, and no push of the ref by
    /// another process (a sync of offline work) carries it there later.
    fn withdraw(&mut self, before: &str) {
        let name = change_ref(&self.id);
        let _ = self
            .repo
            .git
            .run(&["update-ref", &name, before, &self.head]);
        before.clone_into(&mut self.head);
        if self.unpushed_from.as_deref() == Some(before) {
            self.unpushed_from = None;
        }
    }

    /// Takes back every write of this process the remote never took ([`Ctx::withdraw`] to
    /// [`Ctx::unpushed_from`]), unless this run is offline work (`online` is false), which a
    /// later push carries. A write that lost the remote's compare-and-swap then never stays on
    /// this box's chain ref, where every later compare-and-swap on the ref would lose to it
    /// (G294). Local work any other process wrote is never touched (G289).
    fn take_back(&mut self) {
        if !self.online {
            return;
        }
        if let Some(from) = self.unpushed_from.clone() {
            self.withdraw(&from);
        }
    }

    /// Reads the remote's chain ([`Repo::remote_record`]) and goes on from its head. This
    /// process's own unpushed writes are taken back first ([`Ctx::take_back`]); the local
    /// chain ref is then never forced (G289): it moves only forward, and keeps local work the
    /// remote lacks.
    fn refetch(&mut self) -> Result<(), String> {
        self.take_back();
        let remote = self.repo.remote_record(&self.id)?;
        self.head = remote.head;
        self.record = remote.record;
        self.pushed_head = Some(self.head.clone());
        Ok(())
    }

    fn executor_name(&self) -> String {
        self.keys.executor_name.clone()
    }
}

/// G294: a context that ends with writes the remote never took takes them back, on every path
/// out of it: an early return, an error, a push whose result its caller ignores. Every later
/// compare-and-swap on this box's chain ref, by this process or the next, starts from what the
/// remote holds.
impl Drop for Ctx<'_> {
    fn drop(&mut self) {
        self.take_back();
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
    if let Some(refusal) = actor_unproven(repo, id, &trunk)? {
        return Ok(refusal);
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
    // NEW-4: a change of no lane whose view reaches a lane's commits off trunk never lands them.
    if ctx.record.lane.is_none()
        && let Some(refusal) = lane_leak(&ctx, &trunk, &revision)?
    {
        return Ok(refusal);
    }
    // Owner ruling 2026-10-21: a change over the size limits of the policy at the base is
    // refused here, before anything is submitted and before any check.
    if let Some(refusal) = too_large_at_submit(&ctx, &trunk, &revision)? {
        return Ok(refusal);
    }
    // G310: under `[limits] max_waiting_approvals`, a change that needs an approval waits for
    // room in the review queue before it is submitted.
    if let Some(refusal) = review_full_at_submit(&ctx, &trunk, &revision)? {
        return Ok(refusal);
    }
    kill_point("before-submit");

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

    // G273: a land that does not run the operation says at once that it waits on a change it
    // lands after; the executor's pass records it (`dependency_gate`).
    if !wait
        && online
        && let Some(waiting) = dependency_unrun(&ctx, &op_id, &trunk)?
    {
        return Ok(waiting);
    }
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
    result.waiting_for = Some(if online { "executor" } else { "remote" }.to_owned());
    Ok(with_progress(&ctx.repo.git, ctx.op(&op_id), result))
}

/// `CHANGE_TOO_LARGE` at submit (owner ruling 2026-10-21): the change's own edits, from where
/// `revision` meets `trunk` to `revision`, over a `[limits]` limit of the policy at `trunk`.
/// Nothing is written: no operation, no request.
fn too_large_at_submit(
    ctx: &Ctx,
    trunk: &str,
    revision: &str,
) -> Result<Option<BayResult>, String> {
    let git = &ctx.repo.git;
    let limits = policy::limits_at(git, trunk)?;
    if !limits.is_set() {
        return Ok(None);
    }
    // Histories with no common commit are judged against trunk itself.
    let from = git
        .run(&["merge-base", trunk, revision])
        .unwrap_or_else(|_| trunk.to_owned());
    let exceeded = crate::limits::exceeded(git, &limits, &from, revision)?;
    if exceeded.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        crate::limits::refusal(&limits, &exceeded, trunk, "submit").with_change(ctx.id.clone()),
    ))
}

/// G310: `WIP_LIMIT` at submit when the policy at `trunk` sets `[limits] max_waiting_approvals
/// = N`, the change's own edits (from where `revision` meets `trunk`) wake an approval check of
/// that policy, and N or more other live changes already wait for an approval
/// ([`crate::flow::review_queue`]). Its detail `review` names how many wait, the cap, and the
/// one that has waited longest. A change that needs no approval, or a policy without the key,
/// is never refused here, and nothing is written.
fn review_full_at_submit(
    ctx: &Ctx,
    trunk: &str,
    revision: &str,
) -> Result<Option<BayResult>, String> {
    let git = &ctx.repo.git;
    let Some(max) = policy::limits_at(git, trunk)?.max_waiting_approvals else {
        return Ok(None);
    };
    let Some(policy) = policy::load_at(git, trunk).map_err(String::from)? else {
        return Ok(None);
    };
    let from = git
        .run(&["merge-base", trunk, revision])
        .unwrap_or_else(|_| trunk.to_owned());
    let changed = check::changed_paths(git, &from, revision)?;
    let needs_approval = policy
        .checks
        .iter()
        .any(|entry| entry.is_approval() && policy.requires(entry, &changed));
    if !needs_approval {
        return Ok(None);
    }
    let queue = crate::flow::review_queue(ctx.repo, Some(ctx.id.as_str()))?;
    if queue.waiting < max {
        return Ok(None);
    }
    Ok(Some(
        BayResult::blocked(Reason::WipLimit)
            .with_change(ctx.id.clone())
            .with_detail(json!({
                "review": {
                    "waiting": queue.waiting,
                    "max": max,
                    "oldest": queue.oldest,
                    "age_s": queue.age_s,
                },
                "policy": policy::POLICY_PATH,
                "base": trunk,
                "next": "answer or finish the changes waiting for a yes first",
            })),
    ))
}

/// G243b: under a policy at the base that sets `[agent] require_actor_proof`, a request whose
/// agent key cannot prove the actor is refused `ACTOR_UNPROVEN` before anything is signed or
/// written: several actors run sessions under this OS account, which can read the file-based
/// agent key, so any of them could sign as this one. Judged as doctor's `key-custody` row
/// judges it, at the trunk, so a change never lifts it for itself. Off by default.
fn actor_unproven(repo: &Repo, id: &str, trunk: &str) -> Result<Option<BayResult>, String> {
    if !policy::requires_actor_proof_at(&repo.git, trunk)? {
        return Ok(None);
    }
    let sessions = repo.common_dir.join("bay").join("sessions");
    let proof = crate::custody::actor_proof(&repo.git, Some(&sessions));
    if !proof.unproven() {
        return Ok(None);
    }
    Ok(Some(
        BayResult::blocked(Reason::ActorUnproven)
            .with_change(id)
            .with_detail(json!({
                "actor_proof": proof.proof,
                "actors": proof.actors,
                "policy": policy::POLICY_PATH,
                "base": trunk,
                "recovery": "give each actor its own OS account, or a hardware agent key (ssh-keygen -t ed25519-sk, which signs only with a touch), enrol it, then merge again",
            })),
    ))
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
    let (elapsed, evidence, _) = progress(git, op);
    result.elapsed_s = elapsed;
    result.evidence = Some(evidence);
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
    // G284: an operation waiting for a person's approval is asked again on every pass, as its
    // author's retry asks it: once the evidence on its candidate, or trunk, changed it is
    // re-armed and run below; unchanged, it keeps waiting, with nothing written and no event.
    if awaits_approval(ctx.op(op_id)) {
        let executor = ctx.executor_signer()?;
        ctx.chain.sign_with(executor);
        if let Some(waiting) = retry_blocked(&mut ctx, op_id, &revision, &trunk)? {
            return Ok(waiting);
        }
    }
    let result = execute_op(&mut ctx, op_id, trunk);
    ctx.tell_deferred();
    let result = result?;
    kill_point("before-outcome-event");
    // A log that cannot take the event now leaves the blocked operation on the chain, and the
    // next executor pass writes the event the logs lack (`recover_events`).
    if let Ok(Some(event)) = outcome_event(&ctx, op_id, &result) {
        // A lost notice is never retried: `bay status --since` reads the event from the log.
        let _ = crate::notify::notify(ctx.repo, &event);
    }
    // G307: a landing this pass reached wakes the change's author once. A wake is a
    // convenience, never the outcome: one that cannot be recorded changes nothing here.
    let _ = landed_wake(&ctx, op_id, &result);
    Ok(result)
}

/// G307: asks the wake adapter, once, to wake the author of `ctx`'s change for operation
/// `op_id`'s outcome, and returns what bay records of the answer. Only an executor pass
/// ([`execute`]) asks: an outcome reached inside the author's own `bay merge --wait` returns to
/// that session directly, so it never wakes the turn that caused it. The session to wake is
/// the one the change record names.
fn wake_author(
    ctx: &Ctx,
    op_id: &str,
    decision: Decision,
    reason: Option<Reason>,
) -> serde_json::Value {
    crate::talk::wake::wake_outcome(
        ctx.repo,
        &crate::talk::wake::Outcome {
            change: &ctx.id,
            op_id,
            decision,
            reason,
            actor: &ctx.record.actor,
        },
    )
}

/// G307: a landing an executor pass reached wakes the change's author once, so a session that
/// left the change to the executor can move on. Landings are read from trunk and never logged,
/// so what trunk cannot hold, the adapter's answer, is a `wake` event on this executor's log,
/// keyed by the operation and its outcome: one landing is one wake, however often a pass sees
/// it, and a log that already holds it is never asked again. Without a `[plugin.wake] command`
/// nothing is asked and nothing is written. The notify adapter is not told: the landing is
/// on trunk, and a wake is no outcome of its own.
fn landed_wake(ctx: &Ctx, op_id: &str, result: &BayResult) -> Result<(), String> {
    if result.decision != Decision::Landed || !crate::talk::wake::configured(ctx.repo) {
        return Ok(());
    }
    let id = digest(&json!({"op_id": op_id, "outcome": "landed"}))?;
    let id = format!("evt_{}", &id.trim_start_matches("sha256:")[..20]);
    crate::log::fetch(ctx.repo);
    if crate::log::held_events(&ctx.repo.git)?.contains(&id) {
        return Ok(());
    }
    let wake = wake_author(ctx, op_id, Decision::Landed, None);
    let event = json!({
        "schema": crate::log::EVENT_SCHEMA,
        "id": id,
        "at": crate::clock::now_rfc3339(),
        "kind": "wake",
        "change": ctx.id,
        "op_id": op_id,
        "decision": Decision::Landed.word(),
        "actor": ctx.record.actor,
        "wake": wake,
        "recurrence_key": format!("wake/{op_id}"),
    });
    let executor = ctx.executor_name();
    let signer = ctx.executor_signer().ok().flatten();
    crate::log::append(ctx.repo, &executor, signer.as_ref(), &event).map(|_| ())
}

/// `INTERFACES` § 2, outcomes that arrive after the agent left: a blocked outcome the executor
/// reached, which waits for the actor, is an event for the change's actor on this executor's
/// log. Returns the event when it was new, for the caller to tell the notify adapter. A new
/// event carries the author's wake (G307, [`wake_author`]).
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
    let mut event = blocked_event(&ctx.record, op_id, reason, &detail)?;
    // As `crate::log::append_event` does, an event a log already holds is not new: the
    // outcome was told, and its author woken, by whoever wrote it.
    let id = event["id"].as_str().unwrap_or_default().to_owned();
    crate::log::fetch(ctx.repo);
    if crate::log::held_events(&ctx.repo.git)?.contains(&id) {
        return Ok(None);
    }
    // G307: a block whose next step is the author's wakes the author once, keyed by this
    // event's id, and the answer is recorded on the event as a talk's ack records it. Never
    // retried, also when the log write below fails. A transient block is bay's own to retry,
    // so it wakes no one. APPROVAL_PENDING waits on the approvers the refusal names, but a
    // `bay.wake/1` request addresses a session, and an approver is a registry principal with
    // no session the change record names: the author is woken, and the event names the
    // approvers for whoever reads it.
    if !reason.transient() {
        event["wake"] = wake_author(ctx, op_id, result.decision, Some(reason));
    }
    let executor = ctx.executor_name();
    let signer = ctx.executor_signer().ok().flatten();
    crate::log::append(ctx.repo, &executor, signer.as_ref(), &event)?;
    Ok(Some(event))
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
    // N-21: executor logs are signed only where the registry signs everything.
    let signer = match Registry::current(&repo.git)?.filter(Registry::signs_all) {
        None => None,
        Some(_) => Some(keys.executor.clone().ok_or(
            "the repository has a key registry and this machine has no runner key (git config bay.runnerKey)",
        )?),
    };
    let mut written = 0;
    for (record, op_id, reason) in blocked {
        let event = blocked_event(record, op_id, reason, &serde_json::Value::Null)?;
        if held.contains(event["id"].as_str().unwrap_or_default()) {
            continue;
        }
        // G307: no wake. The pass that reached this outcome may have asked the wake adapter
        // before its log write was lost, and a wake is never retried.
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
    // G273: a change publishes only after every change it names has landed (`crate::after`).
    // Until then it holds no queue place, so whatever is behind it reaches the head.
    if ctx.record.lane.is_none()
        && let Some(waiting) = dependency_gate(ctx, op_id, &revision, &trunk)?
    {
        return Ok(waiting);
    }
    // N-8: while trunk is held only a fix or a revert publishes (`crate::hold`). A lane change
    // never publishes, so no hold holds it.
    if ctx.record.lane.is_none()
        && let Some(waiting) = hold_gate(ctx, op_id, &revision, &trunk)?
    {
        return Ok(waiting);
    }
    // § 6: while a queue is live only its head runs; anyone else holds a place and publishes
    // nothing. With a throughput lever on, it is checked ahead of its turn (`crate::throughput`).
    // NEW-4: a lane change only checks, so it never reserves a place, and never waits for one.
    let gate = if ctx.record.lane.is_some() {
        Gate::Free
    } else {
        queue_gate(ctx, op_id)?
    };
    let mut seq = match gate {
        Gate::Wait(result) => return Ok(ahead_of_turn(ctx, op_id, &trunk, *result)),
        Gate::Head(seq) => Some(seq),
        Gate::Free => None,
    };
    if let Some(report) = claim(ctx, op_id, &revision, &trunk)? {
        return Ok(report);
    }
    // G173: whatever the outcome, the operation's anchor goes with it.
    let repo = ctx.repo;
    // A claim this process gives up before any attempt is given back, never left `running`.
    let _anchored = match Anchored::new(&repo.git, op_id) {
        Ok(anchored) => anchored,
        Err(why) => {
            release_after_fault(ctx, op_id, &revision);
            return Err(why);
        }
    };
    let outcome = run_attempts(ctx, op_id, &revision, trunk, &mut seq);
    if outcome.is_err() {
        release_after_fault(ctx, op_id, &revision);
    }
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

/// A fault ended this executor's run of `op_id` after its claim (§ 9a): the claim is given
/// back on the remote chain, so the operation never stays `running` under an owner that has
/// stopped. The next executor takes it at once, with no lease to wait out. Best effort: when
/// the chain moved to another epoch, or the remote cannot be reached, that state stands.
fn release_after_fault(ctx: &mut Ctx, op_id: &str, revision: &str) {
    let mine = ctx.my_epoch;
    if mine == 0 || ctx.refetch().is_err() {
        return;
    }
    let name = ctx.executor_name();
    let Some(op) = ctx.find_op_mut(op_id) else {
        return;
    };
    let held = matches!(op.state, OpState::Running | OpState::Checked);
    if op.epoch != mine || op.owner.as_deref() != Some(name.as_str()) || !held {
        return;
    }
    op.owner = None;
    if op.state == OpState::Running {
        op.state = if op.job.is_some() {
            OpState::Admitted
        } else {
            OpState::Submitted
        };
    }
    let message = format!("released {op_id} epoch {mine} after a fault");
    if ctx.save_cas(revision, None, &message).unwrap_or(false) {
        ctx.push_chain();
    }
}

/// Steps 2–7 for one operation, rebuilding when trunk moves; joins the queue after two
/// `MAIN_MOVED` refusals. `seq` is this operation's place when it is the queue's head.
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
        // A batch member's candidate never lands alone (`crate::throughput`).
        let ready = ctx.op(op_id).attempts.last().filter(|attempt| {
            ctx.op(op_id).state == OpState::Checked
                && attempt.base == trunk
                && attempt.epoch == ctx.my_epoch
                && attempt.batch.is_none()
        });
        let attempt = if let Some(attempt) = ready {
            attempt.clone()
        } else {
            // G284: the candidate an approval arrived for is checked again as it is; no batch
            // folds it into a tree no one approved.
            let approved = approved_candidate(ctx, op_id, &trunk)?;
            // Batching (`crate::throughput`): the head may land together with the entries
            // behind it, on one check of their combined tree.
            if let Some(at) = *seq
                && ctx.record.lane.is_none()
                && approved.is_none()
                && let Some(landed) = batch(ctx, op_id, &revision, &trunk, at)?
            {
                return Ok(landed);
            }
            match build_and_check(ctx, op_id, &revision, &trunk, approved)? {
                Ok(attempt) => attempt,
                Err(refusal) if ctx.record.lane.is_some() => {
                    return lane_never_lands(ctx, op_id, &revision, Some(refusal));
                }
                Err(refusal) => return Ok(refusal),
            }
        };
        // NEW-4: a lane change's checked candidate is never published, by this guard and by the
        // host hook's (`crate::hook`). This one stands before every publishing push.
        if ctx.record.lane.is_some() {
            return lane_never_lands(ctx, op_id, &revision, None);
        }
        // N-8: the hold is asked again before each publishing push, so one that began while
        // the candidate was checked holds it too. The head's place is then released as
        // `trunk_held` (`execute_op`), which counts toward no limit.
        if let Some(waiting) = hold_gate(ctx, op_id, &revision, &trunk)? {
            return Ok(waiting);
        }

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

/// Batching with bisection (`crate::throughput`): the head `op_id`, at queue place `at`, and
/// the live entries behind it that fold onto it, checked once as one tree on `trunk`. On a pass
/// they land together; on a failure the bisection finds the largest passing prefix, which lands
/// together when it holds two members or more, and notes the first failing member. `None` when
/// no batch forms or none lands: the head runs its ordinary path, on the records of its own
/// prefix when the bisection checked it, since that prefix is its own candidate's tree on its
/// own base.
fn batch(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    trunk: &str,
    at: u64,
) -> Result<Option<BayResult>, String> {
    let repo = ctx.repo;
    let Some((policy, members)) = batch_plan(ctx, op_id, revision, trunk, at)? else {
        return Ok(None);
    };
    // N-8: a held trunk publishes no batch, whose members need not all be fixes; the head
    // runs its ordinary path, judged by the hold alone.
    if crate::hold::held(repo, trunk).is_some() {
        return Ok(None);
    }
    if let Some(red) = red_gate(ctx, op_id, revision, trunk)? {
        return Ok(Some(red));
    }
    ctx.step.set("check");
    let id = throughput::batch_id(op_id, trunk)?;
    let Some((search, checks)) = bisect_batch(ctx, op_id, trunk, &policy, &members, &id)? else {
        return Ok(None);
    };
    if let Some(culprit) = search.culprit().filter(|culprit| *culprit >= 2) {
        let member = &members[culprit - 1];
        let _ = note_culprit(repo, &member.change, &member.op_id, &id);
    }
    if search.passed() < 2 {
        return Ok(None);
    }
    let landing = &members[..search.passed()];
    let mut others = Vec::new();
    for member in &landing[1..] {
        if let Ok(Some(other)) = joined(ctx, member, trunk) {
            others.push(other);
        } else {
            give_back(&mut others, landing);
            return Ok(None);
        }
    }
    match land_batch(ctx, &mut others, landing, trunk, &id) {
        Ok(Some(mut result)) => {
            if let Some(detail) = result.detail.as_mut() {
                detail["batch"]["checks"] = json!(checks);
                detail["batch"]["members"] = json!(
                    landing
                        .iter()
                        .map(|member| json!({"change": member.change, "op_id": member.op_id}))
                        .collect::<Vec<_>>()
                );
            }
            Ok(Some(result))
        }
        other => {
            give_back(&mut others, landing);
            other
        }
    }
}

/// The batch the head `op_id` at place `at` would form on `trunk`, with the policy at
/// `trunk`: `None` when batching is off, reuse cannot stand for a run (§ 7,
/// [`Ctx::reuse_trusted`]), the head is a repair, a culprit or names inputs, a check is keyed
/// by commit (the tip's own commit was never checked), fewer members than `max(min, 2)` fold,
/// or the lane's recent failure rate is above `max_failure_rate`.
fn batch_plan(
    ctx: &Ctx,
    op_id: &str,
    revision: &str,
    trunk: &str,
    at: u64,
) -> Result<Option<(policy::Policy, Vec<throughput::Member>)>, String> {
    let repo = ctx.repo;
    let git = &repo.git;
    let Some(bounds) = repo.config.batch() else {
        return Ok(None);
    };
    if !repo.config.evidence_reuse()
        || ctx.repair.is_some()
        || ctx.op(op_id).batch_culprit.is_some()
        || !inputs::names(&ctx.record.declared_inputs).is_empty()
    {
        return Ok(None);
    }
    let Ok(Some(policy)) = policy::load_at(git, trunk) else {
        return Ok(None);
    };
    let commit_keyed = policy.checks.iter().any(policy::CheckEntry::commit_keyed);
    if !ctx.reuse_trusted(trunk) || policy.unchecked || commit_keyed {
        return Ok(None);
    }
    let queue = crate::queue::Queue::local(git)?;
    let head = throughput::Queued {
        seq: at,
        change: ctx.id.clone(),
        op_id: op_id.to_owned(),
        revision: revision.to_owned(),
        live: true,
        batchable: true,
        checked: None,
    };
    let behind: Vec<throughput::Queued> = queue
        .behind(at)
        .into_iter()
        .map_while(|entry| throughput::Queued::read(git, entry))
        .collect();
    let by_lane = repo.config.footprint_lanes();
    let mut members =
        throughput::plan_batch(git, &policy, trunk, &head, &behind, bounds.max, by_lane)?;
    // Owner ruling 2026-10-21: a member over the size limits never lands in a batch; the batch
    // ends before it, and it is refused at its own candidate build.
    if policy.limits.is_set() {
        let mut from = trunk.to_owned();
        let mut within = 0;
        for member in &members {
            if !crate::limits::exceeded(git, &policy.limits, &from, &member.tree)?.is_empty() {
                break;
            }
            from.clone_from(&member.tree);
            within += 1;
        }
        members.truncate(within);
    }
    if members.len() < bounds.min.max(2) {
        return Ok(None);
    }
    // NEW-10: the lane is the places whose footprint overlaps the head's.
    let lane = if by_lane {
        let changed = check::changed_paths(git, trunk, &members[0].tree)?;
        Some(throughput::Footprint::of(&policy, &changed))
    } else {
        None
    };
    if lane_failure_rate(git, &queue, &policy, lane.as_ref()) > bounds.max_failure_rate {
        return Ok(None);
    }
    Ok(Some((policy, members)))
}

/// The failure rate of the last [`throughput::RATE_WINDOW`] check outcomes of released queue
/// places in `lane` (every place without footprint lanes). A place whose footprint cannot be
/// read counts in every lane.
fn lane_failure_rate(
    git: &Git,
    queue: &crate::queue::Queue,
    policy: &policy::Policy,
    lane: Option<&throughput::Footprint>,
) -> f64 {
    let outcomes: Vec<&str> = queue
        .releases_newest_first()
        .filter(|(_, reason)| throughput::is_check_outcome(reason))
        .filter(|(entry, _)| {
            lane.is_none_or(|lane| {
                throughput::place_footprint(git, policy, entry)
                    .is_none_or(|footprint| footprint.overlaps(lane))
            })
        })
        .take(throughput::RATE_WINDOW)
        .map(|(_, reason)| reason)
        .collect();
    throughput::failure_rate(outcomes)
}

/// Checks the batch `members` on `trunk` as the head's (heartbeats and producer), the whole
/// first and then the bisection's prefixes, each its predicted tip, and pushes the evidence.
/// Returns the search and what each check found; `None` when no worker answered.
fn bisect_batch(
    ctx: &mut Ctx,
    op_id: &str,
    trunk: &str,
    policy: &policy::Policy,
    members: &[throughput::Member],
    id: &str,
) -> Result<Option<(throughput::Bisect, Vec<serde_json::Value>)>, String> {
    let mut search = throughput::Bisect::new(members.len());
    let mut checks = Vec::new();
    while let Some(prefix) = search.next_prefix() {
        let tip = &members[prefix - 1];
        let (verdict, refusal) = run_policy(
            ctx,
            op_id,
            &format!("batch-{id}-{prefix}"),
            // A batch's tip lands several changes, so its request names none of them.
            (tip.commit.as_str(), tip.tree.as_str(), None),
            trunk,
            policy,
            trunk,
            &[],
        )?;
        if refusal
            .as_ref()
            .is_some_and(|refusal| refusal.reason == Some(Reason::CheckUnavailable))
        {
            return Ok(None);
        }
        search.record(prefix, refusal.is_none());
        checks.push(json!({"members": prefix, "tree": tip.tree, "verdict": verdict}));
    }
    // Heartbeats moved the head's chain while the checks ran; continue from the last one.
    if let Some(beat) = ctx.beat_pushed.borrow_mut().take() {
        ctx.head.clone_from(&beat);
        ctx.pushed_head = Some(beat);
        ctx.unpushed_from = None;
    }
    push_and_read_back_evidence(ctx.repo, &ctx.producer()?.0)?;
    Ok(Some((search, checks)))
}

/// A batch member other than the head, loaded, verified and claimed by this executor (§ 9a),
/// or `None` when it cannot join: its request no longer verifies, or it is not this
/// executor's to run.
fn joined<'a>(
    ctx: &Ctx<'a>,
    member: &throughput::Member,
    trunk: &str,
) -> Result<Option<Ctx<'a>>, String> {
    let mut other = Ctx::load(ctx.repo, ctx.step, &member.change)?;
    other.registry.clone_from(&ctx.registry);
    let signer = other.executor_signer()?;
    other.chain.sign_with(signer);
    // A repair is judged by its own policy alone, never in a batch (§ 7). G273: a member that
    // lands after a change not yet on trunk never joins one.
    if verify_request(&mut other, &member.op_id)?.is_some()
        || other.repair.is_some()
        || dependency_waiting(&other, &member.op_id, trunk)?.is_some()
        || claim(&mut other, &member.op_id, &member.revision, trunk)?.is_some()
    {
        return Ok(None);
    }
    Ok(Some(other))
}

/// The claims of a batch's other members (`others`, for `members` after the head) go back, so
/// each runs as its own head with no lease to wait out (§ 9a).
fn give_back(others: &mut [Ctx], members: &[throughput::Member]) {
    for (other, member) in others.iter_mut().zip(&members[1..]) {
        release_after_fault(other, &member.op_id, &member.revision);
    }
}

/// A batch member's candidate, as [`land_batch`] builds it: its id, the commit it is parented
/// on, and the commit.
struct Built {
    candidate_id: String,
    base: String,
    commit: String,
}

/// Lands `members` (the head first, then the claimed `others`) as batch `id`, checked on
/// `trunk` as their last member's tree: each member's candidate is built ([`build_batch`]) and
/// persisted as checked in its own chain ([`persist_batch`]), then one atomic push moves trunk
/// to the last member with every member's landed marker, intent marker and queue release, and
/// each landing is derived from trunk. `None` when a member is no longer this executor's:
/// nothing was pushed.
fn land_batch<'a>(
    ctx: &mut Ctx<'a>,
    others: &mut [Ctx<'a>],
    members: &[throughput::Member],
    trunk: &str,
    id: &str,
) -> Result<Option<BayResult>, String> {
    let repo = ctx.repo;
    let git = &repo.git;
    let (built, anchors) = build_batch(ctx, others, members, trunk, id)?;
    if !persist_batch(ctx, others, members, &built, trunk, id)? {
        return Ok(None);
    }
    ctx.step.set("publish");
    let signer = ctx.executor_signer().ok().flatten();
    let mut pushes = Vec::new();
    for (index, member) in members.iter().enumerate() {
        let owner: &mut Ctx = if index == 0 {
            &mut *ctx
        } else {
            &mut others[index - 1]
        };
        if still_owner(owner, &member.op_id, &member.revision)?.is_some() {
            return Ok(None);
        }
        let release = crate::queue::release_commit(
            git,
            signer.as_ref(),
            member.seq,
            &member.op_id,
            "published",
        )?;
        pushes.push(BatchPush {
            change: member.change.clone(),
            commit: built[index].commit.clone(),
            release: (member.seq, release),
            claim: owner.record.claim.clone(),
        });
    }
    let tip = &built[members.len() - 1].commit;
    kill_point("before-publish");
    publish_batch_push(repo, tip, &pushes)
        .map_err(|error| format!("batch {id} was refused: {error}"))?;
    kill_point("after-publish");
    drop(anchors);
    for (index, member) in members.iter().enumerate().skip(1) {
        let other = &mut others[index - 1];
        // A landing whose record could not be written now is derived from trunk on a later run.
        let _ = derive_landed(other, &member.op_id, &member.revision, &built[index].commit);
        other.tell_deferred();
    }
    derive_landed(
        ctx,
        &members[0].op_id,
        &members[0].revision,
        &built[0].commit,
    )
    .map(Some)
}

/// Step 5 for a batch: each member's candidate linked into its own chain as checked, with the
/// batch it belongs to and what the batch checked, the others' first: the head's is persisted
/// only once every other member's is, so no partial batch is ever the head's to publish.
/// Returns whether every write reached the remote under this executor's epoch.
fn persist_batch<'a>(
    ctx: &mut Ctx<'a>,
    others: &mut [Ctx<'a>],
    members: &[throughput::Member],
    built: &[Built],
    trunk: &str,
    id: &str,
) -> Result<bool, String> {
    let size = members.len();
    for index in (1..size).chain(std::iter::once(0)) {
        let owner: &mut Ctx = if index == 0 {
            &mut *ctx
        } else {
            &mut others[index - 1]
        };
        let attempt = Attempt {
            candidate_id: built[index].candidate_id.clone(),
            base: built[index].base.clone(),
            commit: built[index].commit.clone(),
            tree: members[index].tree.clone(),
            verdict: "pass".to_owned(),
            epoch: owner.my_epoch,
            batch: Some(crate::change::BatchMember {
                id: id.to_owned(),
                position: index + 1,
                size,
                tip: built[size - 1].commit.clone(),
                tree: members[size - 1].tree.clone(),
                base: trunk.to_owned(),
            }),
        };
        let member = &members[index];
        let message = format!("checked {} in batch {id}", attempt.candidate_id);
        let persisted = persist_owned(
            owner,
            &member.op_id,
            &member.revision,
            &attempt.commit,
            &message,
            |op| {
                op.attempts.push(attempt.clone());
                op.unavailable = None;
                op.state = OpState::Checked;
                op.outcome = None;
                op.blocked_on = None;
            },
        )?;
        if !matches!(persisted, Persisted::Pushed) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Step 3 for a batch: each member's executor-signed candidate of its predicted tree, parented
/// on the member before it (the first on `trunk`), with its own trailers and `Bay-Batch`, each
/// anchored (G173). The tree is the one the batch checked: the real merge onto the member
/// before it is that tree, as `crate::throughput` shows.
fn build_batch<'a>(
    ctx: &Ctx<'a>,
    others: &[Ctx<'a>],
    members: &[throughput::Member],
    trunk: &str,
    id: &str,
) -> Result<(Vec<Built>, Vec<Anchored<'a>>), String> {
    let repo: &'a Repo = ctx.repo;
    let git = &repo.git;
    // N-21: an unsigned landing (no registry, or `signing = "approvals"`) names none.
    let registry_id = ctx
        .signing()
        .map_or_else(|| "none".to_owned(), |registry| registry.commit.clone());
    let mut parent = trunk.to_owned();
    let mut built = Vec::new();
    let mut anchors = Vec::new();
    for (index, member) in members.iter().enumerate() {
        let owner: &Ctx = if index == 0 { ctx } else { &others[index - 1] };
        let op = owner.op(&member.op_id);
        let candidate_id = format!("{}.{}", member.op_id, op.attempts.len() + 1);
        let request_id = op.request_id.clone().unwrap_or_default();
        let mut message = candidate_message(
            owner,
            &member.revision,
            &request_id,
            &candidate_id,
            &registry_id,
        );
        let tag = throughput::BatchTag {
            id: id.to_owned(),
            position: index + 1,
            size: members.len(),
        };
        message.push_str(&tag.trailer());
        let commit = commit_candidate(owner, &member.tree, &parent, &message)?;
        // The head's anchor is its operation's; each other member's goes with this run.
        if index > 0 {
            anchors.push(Anchored::new(git, &member.op_id)?);
        }
        anchor(git, &member.op_id, &commit)?;
        built.push(Built {
            candidate_id,
            base: parent,
            commit: commit.clone(),
        });
        parent = commit;
    }
    Ok((built, anchors))
}

/// One batch member's part of the publishing push.
struct BatchPush {
    change: String,
    commit: String,
    release: (u64, String),
    claim: Option<crate::change::IntentClaim>,
}

/// Step 6 for a batch (`crate::throughput`): one atomic push moves trunk to `tip`, the last
/// member, and creates every member's landed marker, intent marker and queue release. Never
/// forced; each marker and release is create-only.
fn publish_batch_push(repo: &Repo, tip: &str, members: &[BatchPush]) -> Result<(), String> {
    let mut leases = Vec::new();
    let mut refspecs = vec![format!("{tip}:refs/heads/{}", repo.trunk())];
    for member in members {
        let landed = format!("{}{}", crate::names::LANDED, member.change);
        leases.push(format!("--force-with-lease={landed}:"));
        refspecs.push(format!("{}:{landed}", member.commit));
        let (seq, release) = &member.release;
        let done = format!("{}{seq}", crate::queue::DONE_PREFIX);
        leases.push(format!("--force-with-lease={done}:"));
        refspecs.push(format!("{release}:{done}"));
        if let Some(claim) = &member.claim {
            let intent = claim.landed_ref();
            leases.push(format!("--force-with-lease={intent}:"));
            refspecs.push(format!("{}:{intent}", member.commit));
        }
    }
    let mut args = vec!["push", "-q", "--atomic"];
    args.extend(leases.iter().map(String::as_str));
    args.push(repo.remote());
    args.extend(refspecs.iter().map(String::as_str));
    repo.git
        .run(&args)
        .map(|_| ())
        .map_err(|error| error.message())
}

/// Batching (`crate::throughput`): notes on the queued operation `op_id` of change `id` that
/// batch `batch` found it the first member that fails, so it runs alone when it is head. Like
/// the early-conflict note (G177) it takes no ownership and moves neither the operation's
/// state nor its place. Returns whether the chain was pushed.
fn note_culprit(repo: &Repo, id: &str, op_id: &str, batch: &str) -> Result<bool, String> {
    let step = Cell::new("batch");
    let mut ctx = Ctx::load(repo, &step, id)?;
    let Some(op) = ctx.record.operations.iter().find(|op| op.op_id == op_id) else {
        return Ok(false);
    };
    if op.state.is_terminal() || op.state == OpState::Blocked || op.batch_culprit.is_some() {
        return Ok(false);
    }
    let revision = op.revision.clone();
    ctx.registry = Registry::current(&repo.git)?;
    let executor = ctx.executor_signer()?;
    ctx.chain.sign_with(executor);
    ctx.op_mut(op_id).batch_culprit = Some(batch.to_owned());
    ctx.save(
        &revision,
        None,
        &format!("{op_id} is the first member of batch {batch} that fails"),
    )?;
    Ok(ctx.push_chain())
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
        // refusal that counts (`MAIN_MOVED`, § 6), and the rebuild signs under the current
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
                "the merging push was refused: this head's queue place was released ({reason}) while it merged; nothing was merged"
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
        return enqueue(ctx, op_id, revision, Reason::Queued).map(Some);
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
    let held = error.contains(crate::hook::QUEUE_HELD_BY);
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
/// no longer counts, so it is rechecked first). Not counted toward `MERGE_RETRY_EXHAUSTED`.
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
        return Err("the change chain could not be pushed; nothing was merged".to_owned());
    }
    Ok(None)
}

/// § 10a: whether the operation's holder signed its last chain commit with an executor key the
/// current registry no longer holds. That key can write nothing more (no heartbeat, no
/// release), so its lease is not waited out: the revocation released the operation.
fn holder_revoked(ctx: &Ctx, op_id: &str) -> Result<bool, String> {
    let (Some(registry), Some(owner)) = (ctx.signing(), &ctx.op(op_id).owner) else {
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
    Ok(registry.holds(KeyClass::Runner, &signer.name, &key_type, &blob))
}

/// After this many uncounted publish retries an operation is `MERGE_RETRY_EXHAUSTED` (§ 6).
const PUBLISH_RETRY_LIMIT: u32 = 5;
/// The short wait before a publishing push the hook timed out is tried again.
const HOOK_RETRY_WAIT: Duration = Duration::from_secs(1);

/// The trunk hook timed out on the publishing push: retry the same candidate after a short wait
/// (`None`), uncounted toward `MAIN_MOVED` and the queue's deadline (`INTERFACES` § 2).
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
/// uncounted toward `MAIN_MOVED` and the queue's deadline, as a hook timeout is.
fn lock_held(ctx: &mut Ctx, op_id: &str, revision: &str) -> Result<Option<BayResult>, String> {
    if let Some(blocked) = uncounted_retry(ctx, op_id, revision, "remote ref lock held")? {
        return Ok(Some(blocked));
    }
    std::thread::sleep(HOOK_RETRY_WAIT);
    Ok(None)
}

/// Counts one uncounted retry (a `HOOK_TIMEOUT`, or a deadline release during a publish): the
/// fifth blocks the operation with `MERGE_RETRY_EXHAUSTED` (`DESIGN` § 6). The count lives in
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
        return enqueue(ctx, op_id, revision, Reason::Queued).map(Some);
    }
    if policy_identity(&ctx.repo.git, moved.base)? != policy_identity(&ctx.repo.git, moved.trunk)? {
        return recheck_under_new_policy(ctx, op_id, revision, seq);
    }
    let op = ctx.op_mut(op_id);
    op.trunk_moved += 1;
    // § 6: bay retries once more, then the operation joins the publication queue.
    if op.trunk_moved >= TRUNK_MOVED_LIMIT && seq.is_none() {
        return enqueue(ctx, op_id, revision, Reason::Queued).map(Some);
    }
    Ok(None)
}

/// § 7: a candidate checked under an older policy is `POLICY_CHANGED` and rechecked under the new
/// one. That is no `MAIN_MOVED` refusal. A queue head is released with `policy_changed`, which
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
    enqueue(ctx, op_id, &revision, Reason::Queued).map(|result| Gate::Wait(Box::new(result)))
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
        op.outcome = Some(Reason::Queued.code().to_owned());
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
    result.waiting_for = Some("queue".to_owned());
    Ok(result)
}

/// NEW-2, NEW-10 (`crate::throughput`): an operation that holds a place behind the queue's head
/// is checked now, ahead of its turn, by this executor, which is not running the head. Nothing is
/// published, no attempt is recorded and no ownership is taken; the operation's chain lists the
/// check under `prechecks`, and its records count only through reuse once the operation's own
/// candidate has the same inputs. A fault is reported in `detail.precheck`, never as the
/// operation's outcome, which stays its place in the queue.
fn ahead_of_turn(ctx: &mut Ctx, op_id: &str, trunk: &str, mut result: BayResult) -> BayResult {
    let repo = ctx.repo;
    let config = &repo.config;
    if result.decision != Decision::Queued || (config.ahead() == 0 && !config.footprint_lanes()) {
        return result;
    }
    let precheck = match precheck(ctx, op_id, trunk) {
        Ok(None) => return result,
        Ok(Some(precheck)) => precheck,
        Err(error) => json!({"error": error}),
    };
    if let Some(detail) = result.detail.as_mut() {
        detail["precheck"] = precheck;
    }
    result
}

/// The check [`ahead_of_turn`] makes, or `None` when there is none to make: the operation is
/// further behind the head than `[runner] ahead` and shares a footprint with an entry ahead
/// (or footprint lanes are off), an entry ahead cannot be read or changes the policy, its own
/// merge onto the base conflicts, ends `no_effect` or changes the policy, it names inputs, or
/// that base and tree were checked already. The base is what the live entries ahead predict
/// ([`throughput::predict`]), or, beyond `ahead` and with `[runner] footprint_lanes`, the
/// trunk tip for an operation whose footprint is disjoint from every entry's ahead of it
/// ([`throughput::disjoint`]). The operation's merge onto it is checked as an unsigned
/// predicted commit under the policy at that base, with the adapter from it, by this
/// executor's producer, and the evidence is pushed and read back.
fn precheck(ctx: &mut Ctx, op_id: &str, trunk: &str) -> Result<Option<serde_json::Value>, String> {
    let repo = ctx.repo;
    let git = &repo.git;
    // § 7: no record shows named inputs, and a pre-check's records count only through reuse.
    if !repo.config.evidence_reuse() || !inputs::names(&ctx.record.declared_inputs).is_empty() {
        return Ok(None);
    }
    let queue = crate::queue::Queue::local(git)?;
    let Some(place) = queue.of(op_id).map(|entry| entry.seq) else {
        return Ok(None);
    };
    let mut ahead = Vec::new();
    for entry in queue.ahead_of(place) {
        let Some(queued) = throughput::Queued::read(git, entry) else {
            return Ok(None);
        };
        ahead.push(queued);
    }
    let Ok(Some(at_trunk)) = policy::load_at(git, trunk) else {
        return Ok(None);
    };
    // § 7, G318: a pre-check is worth making only where the policy at the base lets its
    // record stand for a run.
    if ahead.is_empty() || !ctx.reuse_trusted(trunk) {
        return Ok(None);
    }
    let revision = ctx.op(op_id).revision.clone();
    // Within `ahead` of the head: on the base the entries ahead predict. Further behind, with
    // footprint lanes, an entry disjoint from every entry ahead: on the tip, for its footprint.
    let (kind, base) = if ahead.len() <= repo.config.ahead() {
        match throughput::predict(git, &at_trunk, trunk, &ahead)? {
            Some(base) => ("predicted", base),
            None => return Ok(None),
        }
    } else if repo.config.footprint_lanes()
        && throughput::disjoint(git, &at_trunk, trunk, &revision, &ahead)?
    {
        ("tip", trunk.to_owned())
    } else {
        return Ok(None);
    };
    let Ok(Some(policy)) = policy::load_at(git, &base) else {
        return Ok(None);
    };
    let throughput::Fold::Applied { commit, tree, .. } =
        throughput::fold(git, &policy, &base, &revision)?
    else {
        return Ok(None);
    };
    let base_tree = tree_of(git, &base)?;
    let done = |precheck: &Precheck| precheck.tree == tree && precheck.base_tree == base_tree;
    if policy.unchecked || ctx.op(op_id).prechecks.iter().any(done) {
        return Ok(None);
    }
    let candidate_id = format!("pre-{op_id}-{}", now_ms());
    let facts = change_facts(ctx, &revision);
    ctx.ahead_of_turn = true;
    let ran = run_policy(
        ctx,
        op_id,
        &candidate_id,
        (commit.as_str(), tree.as_str(), Some(&facts)),
        &base,
        &policy,
        &base,
        &[],
    );
    ctx.ahead_of_turn = false;
    let (verdict, refusal) = ran?;
    // No worker answered: nothing was observed, so nothing is recorded.
    if refusal
        .as_ref()
        .is_some_and(|refusal| refusal.reason == Some(Reason::CheckUnavailable))
    {
        return Ok(None);
    }
    push_and_read_back_evidence(repo, &ctx.producer()?.0)?;
    let op = ctx.op_mut(op_id);
    op.prechecks.push(Precheck {
        base: base.clone(),
        base_tree: base_tree.clone(),
        tree: tree.clone(),
        verdict: verdict.to_owned(),
        at_ms: now_ms(),
    });
    let excess = op
        .prechecks
        .len()
        .saturating_sub(throughput::PRECHECKS_KEPT);
    if excess > 0 {
        op.prechecks = op.prechecks.split_off(excess);
    }
    ctx.save(&revision, None, &format!("prechecked {op_id} on {base}"))?;
    ctx.push_chain();
    Ok(Some(json!({
        "on": kind,
        "base": base,
        "base_tree": base_tree,
        "tree": tree,
        "verdict": verdict,
    })))
}

/// G177: records on the queued operation `op_id` of change `id` the conflict the executor's
/// early probe found against trunk, or clears it with `None`, as one executor-signed chain
/// state. It never takes ownership, moves the operation's state or touches its queue place.
/// Returns whether the chain was pushed; an operation that is no longer queued, or whose note
/// already names these paths, writes nothing.
///
/// # Errors
///
/// The chain cannot be read or written, or a key registry with no executor key here.
pub fn note_early_conflict(
    repo: &Repo,
    id: &str,
    op_id: &str,
    conflict: Option<crate::change::EarlyConflict>,
) -> Result<bool, String> {
    let step = Cell::new("probe");
    let mut ctx = Ctx::load(repo, &step, id)?;
    let Some(op) = ctx.record.operations.iter().find(|op| op.op_id == op_id) else {
        return Ok(false);
    };
    let same = op.early_conflict.as_ref().map(|noted| &noted.paths)
        == conflict.as_ref().map(|found| &found.paths);
    if same || op.state.is_terminal() || op.state == OpState::Blocked {
        return Ok(false);
    }
    let revision = op.revision.clone();
    repo.fetch_registry();
    ctx.registry = Registry::current(&repo.git)?;
    let executor = ctx.executor_signer()?;
    ctx.chain.sign_with(executor);
    let message = match &conflict {
        Some(found) => format!("{op_id} conflicts with trunk {}", found.trunk),
        None => format!("{op_id} merges onto trunk again"),
    };
    ctx.op_mut(op_id).early_conflict = conflict;
    ctx.save(&revision, None, &message)?;
    Ok(ctx.push_chain())
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
    // The remote chain lacks an operation this process appended: its request never reached
    // the remote (another process on this box re-read the chain over it). The caller reads
    // the chain again and writes the request again.
    let Some(op) = ctx.find_op(op_id) else {
        return Err(CHAIN_MOVED.to_owned());
    };
    // An operation this executor left waiting for a worker holds no lease against it: it
    // takes it back at once once its box serves it (§ 7).
    let own_wait = op.unavailable.is_some() && op.owner.as_deref() == Some(name.as_str());
    let held =
        op.owner.is_some() && matches!(op.state, OpState::Running | OpState::Checked) && !own_wait;
    // Another process holds the operation: take it over only once its lease expired, on this
    // process's own clock. A restarted supervisor watches a full lease again. A holder whose
    // executor key the registry revoked holds no lease (§ 10a).
    if held && !holder_revoked(ctx, op_id)? && !lease_expired(ctx) {
        let mut result = BayResult::new(Decision::Ok).with_change(&id).with_op(op_id);
        result.state = Some("running".to_owned());
        let (owner, epoch) = ctx
            .find_op(op_id)
            .map_or((None, 0), |op| (op.owner.clone(), op.epoch));
        return Ok(Some(result.with_detail(
            json!({"lease": "held", "owner": owner, "epoch": epoch}),
        )));
    }
    kill_point("before-claim");
    // A claim is this process's own: its commit names the process, so two claims on one box
    // are never one commit, and a push that carries another's claim is never taken for one.
    let nonce = format!(
        "{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    );
    for _ in 0..3 {
        let Some(op) = ctx.find_op_mut(op_id) else {
            return Err(CHAIN_MOVED.to_owned());
        };
        if op.state.is_terminal() {
            return Ok(Some(superseded(&id, op_id, op.epoch, op.owner.as_ref())));
        }
        let read = op.epoch;
        op.epoch += 1;
        op.owner = Some(name.clone());
        if op.state != OpState::Checked {
            op.state = OpState::Running;
        }
        let epoch = op.epoch;
        let message = format!("claim {op_id} epoch {epoch} by {name} (process {nonce})");
        let before = ctx.head.clone();
        if ctx.save_cas(revision, None, &message)? {
            kill_point("before-claim-push");
            if ctx.push_chain() {
                ctx.my_epoch = epoch;
                return Ok(None);
            }
            // The claim never reached the remote: it leaves this box's chain ref too, so no
            // other process here pushes it later for an owner that gave up (§ 9a).
            ctx.withdraw(&before);
        }
        // Lost: another process on this box moved the local chain ref first, or the remote's
        // compare-and-swap refused the push. Read the remote chain again.
        if let Some(lost) = lost_claim(ctx, op_id, read)? {
            return Ok(Some(lost));
        }
    }
    Err(format!(
        "the change chain kept moving under the claim of {op_id}; nothing was run"
    ))
}

/// After a claim of `op_id` this process lost, `read` being the epoch it claimed over: the
/// remote chain is read again. An operation another claim holds, or one that ended, is
/// `superseded` for this process, which runs nothing. One that no claim holds (the other
/// claim never reached the remote, or was given back) is never taken for another's: `None`,
/// and this process claims it again.
fn lost_claim(ctx: &mut Ctx, op_id: &str, read: u64) -> Result<Option<BayResult>, String> {
    ctx.refetch()?;
    let Some(op) = ctx.find_op(op_id) else {
        return Err(CHAIN_MOVED.to_owned());
    };
    let ended = op.state.is_terminal() || op.state == OpState::Blocked;
    let claimed = op.epoch != read
        && op.owner.is_some()
        && matches!(op.state, OpState::Running | OpState::Checked);
    if ended || claimed {
        return Ok(Some(superseded(
            &ctx.id,
            op_id,
            op.epoch,
            op.owner.as_ref(),
        )));
    }
    Ok(None)
}

/// Before the publishing push: the executor reads the chain from the remote, and if its epoch
/// is no longer current it never pushes (§ 9a, G24). It reports the existing publication, or
/// that it was superseded.
fn still_owner(ctx: &mut Ctx, op_id: &str, revision: &str) -> Result<Option<BayResult>, String> {
    if ctx.refetch().is_err() {
        return Ok(None);
    }
    // An operation the remote chain lacks is no one's to publish.
    let (epoch, owner) = match ctx.find_op(op_id) {
        Some(op) if op.epoch == ctx.my_epoch && op.state != OpState::Superseded => {
            return Ok(None);
        }
        Some(op) => (op.epoch, op.owner.clone()),
        None => (0, None),
    };
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
        // Each record by its id and its whole content: a record replaced under the same id
        // (an unsigned copy, then the signed one) is a changed condition.
        Some(tree) => evidence::records_for(git, tree)?
            .into_iter()
            .map(|record| {
                let content = serde_json::to_value(&record).map_err(|error| error.to_string())?;
                Ok(format!("{}:{}", record.id, digest(&content)?))
            })
            .collect::<Result<_, String>>()?,
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
    let lane_checked = op.outcome.as_deref() == Some(crate::lane::CHECKED);
    let tree = op.attempts.last().map(|attempt| attempt.tree.clone());
    let then = op.blocked_on.clone();
    // N-9: a failed or unanswered check of a policy with a waivable check may get a waiver.
    let waivable = waivable_block(ctx, op_id, reason)?;
    // G245: the approval it waits on is written on another machine and reaches this one only
    // through the remote, so the retry reads the remote's records before it judges the change.
    // N-9: so is a waiver.
    if (reason == Some(Reason::ApprovalPending) || waivable) && ctx.online {
        ctx.repo.fetch_evidence(&ctx.keys);
    }
    let now = condition(
        &ctx.repo.git,
        revision,
        trunk,
        tree.as_deref(),
        &ctx.record.observed_versions,
    )?;
    // A lane change whose checks ran says `ok` again, with no work, while nothing changed.
    if lane_checked && then.as_deref() == Some(now.as_str()) {
        return Ok(Some(
            BayResult::new(Decision::Ok)
                .with_change(ctx.id.clone())
                .with_op(op_id)
                .with_detail(
                    json!({"unchanged": true, "condition": now, "note": crate::lane::NOTE}),
                ),
        ));
    }
    // A lock is no part of the condition: the session that takes it again changes nothing the
    // condition names, so `LOCK_NOT_HELD` is judged again on every retry (its next step).
    if let Some(reason) = reason
        && !reason.transient()
        && reason != Reason::LockNotHeld
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
    ctx.save_or_moved(revision, None, &format!("rearmed {op_id}"))?;
    ctx.push_chain();
    // G284: an approval (or a rejection) arrived for the candidate it waited on, or trunk
    // moved: the run that follows checks that same candidate again while trunk stands.
    if reason == Some(Reason::ApprovalPending) {
        ctx.approved = Some(op_id.to_owned());
    }
    Ok(None)
}

/// N-9: whether `op_id`, blocked for `reason`, may be answered by a waiver: the reason is a
/// failed, flaky or unanswered check, its last candidate is no batch member's, and the policy
/// at that candidate's base marks some check `waivable = true` ([`waivable_policy`]).
fn waivable_block(ctx: &Ctx, op_id: &str, reason: Option<Reason>) -> Result<bool, String> {
    if !matches!(
        reason,
        Some(Reason::CheckFailed | Reason::CheckFlaky | Reason::CheckUnknown)
    ) {
        return Ok(false);
    }
    waivable_policy(ctx, op_id)
}

/// N-9: whether the last candidate of `op_id` is no batch member's and the policy at its base
/// marks some check `waivable = true`.
fn waivable_policy(ctx: &Ctx, op_id: &str) -> Result<bool, String> {
    let Some(attempt) = ctx
        .op(op_id)
        .attempts
        .last()
        .filter(|attempt| attempt.batch.is_none())
    else {
        return Ok(false);
    };
    match policy::load_at(&ctx.repo.git, &attempt.base) {
        Ok(Some(policy)) => Ok(policy.checks.iter().any(|entry| entry.waivable)),
        Ok(None) | Err(policy::LoadError::Invalid(_)) => Ok(false),
        Err(policy::LoadError::Git(why)) => Err(why),
    }
}

/// N-9: whether the counted records on the last candidate of `op_id`, read after a fetch of
/// the remote's evidence, hold a waiver about that exact candidate commit, under a policy with
/// a waivable check. Whether it lets a check through is the judgement's to say
/// ([`policy::Policy::waiver`]); this only keeps the candidate it names. Nothing is read for a
/// policy that marks no check waivable.
fn waiver_answers(ctx: &Ctx, op_id: &str) -> Result<bool, String> {
    if !waivable_policy(ctx, op_id)? {
        return Ok(false);
    }
    let Some(attempt) = ctx.op(op_id).attempts.last() else {
        return Ok(false);
    };
    // The waiver is written on the approver's machine and reaches this one through the remote.
    if ctx.online {
        ctx.repo.fetch_evidence(&ctx.keys);
    }
    let git = &ctx.repo.git;
    Ok(evidence::counted(
        git,
        evidence::records_for(git, &attempt.tree)?,
        ctx.registry.as_ref(),
    )?
    .iter()
    .any(|record| record.kind == evidence::KIND_WAIVER && record.subject.commit == attempt.commit))
}

/// G284: whether `op` waits for a person's approval: blocked `APPROVAL_PENDING`. That is a
/// wait, not an end. It holds no queue place (its place was released as `approval_pending`,
/// which counts toward no limit, § 6), and every executor pass asks it again ([`execute`]):
/// an approval for its exact candidate lands that candidate, a rejection ends it
/// `CHECK_FAILED`, and a moved trunk rebuilds it, so the approval must name the new one.
#[must_use]
pub fn awaits_approval(op: &Operation) -> bool {
    op.state == OpState::Blocked
        && op.outcome.as_deref().and_then(Reason::from_code) == Some(Reason::ApprovalPending)
}

/// G284: the candidate an approval arrived for, checked again rather than rebuilt: the last
/// attempt of `op_id`, when this process re-armed it from `APPROVAL_PENDING`
/// ([`retry_blocked`]) or, N-9, a counted waiver names that exact candidate
/// ([`waiver_answers`]), it was merged onto `trunk` (trunk has not moved since), it is no batch
/// member's, and this process would commit it as it stands, under its own executor identity
/// and the registry the candidate names. Otherwise `None`: the operation builds a new
/// candidate, and an approval or a waiver must name that one.
fn approved_candidate(ctx: &Ctx, op_id: &str, trunk: &str) -> Result<Option<Attempt>, String> {
    if ctx.record.lane.is_some() {
        return Ok(None);
    }
    let Some(attempt) = ctx
        .op(op_id)
        .attempts
        .last()
        .filter(|attempt| attempt.base == trunk && attempt.batch.is_none())
    else {
        return Ok(None);
    };
    // N-9: a waiver names one exact candidate, so a candidate a waiver arrived for is checked
    // again as it is, by whichever process runs it, never rebuilt into a commit it does not
    // name.
    if ctx.approved.as_deref() != Some(op_id) && !waiver_answers(ctx, op_id)? {
        return Ok(None);
    }
    let registry_id = ctx
        .signing()
        .map_or_else(|| "none".to_owned(), |registry| registry.commit.clone());
    let committer = ctx
        .executor_signer()?
        .map_or_else(|| EXECUTOR_IDENTITY[2].1.to_owned(), |signer| signer.name);
    let shown = ctx
        .repo
        .git
        .run(&["show", "-s", "--format=%cn%n%B", &attempt.commit])
        .map_err(|error| error.message())?;
    let mut lines = shown.lines();
    let same_committer = lines.next() == Some(committer.as_str());
    let named = format!("Registry-Id: {registry_id}");
    let same_registry = lines.any(|line| line.trim() == named);
    Ok((same_committer && same_registry).then(|| attempt.clone()))
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
    // G104b: a request file its tree names that cannot be read is said so, never "not a request".
    let text = match ctx.repo.git.file_at(request_id, "request.json") {
        Ok(Some(text)) => text,
        Ok(None) => return refuse("its request is not a request commit"),
        Err(error) => return refuse(&error.message()),
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
/// a key in the agent file, under the actor the change records (G25). Unsigned, as without a
/// registry, under `[policy] signing = "approvals"` (N-21).
fn verify_request(ctx: &mut Ctx, op_id: &str) -> Result<Option<BayResult>, String> {
    if let Some(refusal) = verify_request_digest(ctx, op_id)? {
        return Ok(Some(refusal));
    }
    let Some(registry) = ctx.signing() else {
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
        return Ok(Err(BayResult::blocked(Reason::ViewIncomplete)
            .with_change(id)
            .with_detail(
                json!({"view": repo.view_path(id).display().to_string()}),
            )));
    }
    // BACKLOG 49: a new file over the limit leaves the view's freeze for the assets plugin.
    crate::assets::offload_view(repo, &repo.view_path(id))?;
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
        early_conflict: None,
        prechecks: Vec::new(),
        batch_culprit: None,
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
        "registry": ctx.signing().map(|registry| registry.commit.clone()),
    });
    let request_id = ctx
        .chain
        .request_commit(&request, revision, &format!("request {op_id}"))
        .map_err(|error| error.message())?;
    ctx.op_mut(&op_id).request_id = Some(request_id.clone());
    ctx.save_or_moved(revision, Some(&request_id), &format!("request {op_id}"))?;
    kill_point("before-request-push");
    if !ctx.push_chain_and_request(&request_id) && ctx.online {
        // The remote chain moved while this request was written (an executor appended): the
        // caller re-reads the chain and writes the request again.
        return Err(CHAIN_MOVED.to_owned());
    }
    admit(ctx, &op_id, revision)?;
    Ok(op_id)
}

/// `submit` or a rearm lost a race with another writer of the chain (G298).
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
        "registry": ctx.signing().map(|registry| registry.commit.clone()),
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

/// Brings the remote's chain here (§ 9a): the remote holds what executors recorded. The local
/// chain moves only forward ([`Repo::remote_chain`], G289): local-only work the sync before
/// this could not push is kept, and this landing carries it.
fn adopt_remote_chain(repo: &Repo, id: &str) {
    let _ = repo.remote_chain(id);
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
    let name = command.split_whitespace().next().unwrap_or_default();
    let timeout = ctx.repo.config.submission_timeout();
    // The executor runs as any adapter does (`crate::run`): from the work tree, with only
    // `PATH` and `HOME` of bay's environment, killed whole at its bound. What it needs it is
    // given explicitly: its action as an argument and the request on stdin.
    let started = match crate::run::command(&ctx.repo.work_root, &command) {
        Some(Ok(cmd)) => Ok(cmd),
        Some(Err(why)) => Err(("submission_failed", why)),
        None => Err((
            "submission_failed",
            "the runner command is empty".to_owned(),
        )),
    }
    .and_then(|mut cmd| {
        // The executor adapter alone keeps bay's whole environment, as it always has: an
        // external executor may need its own credentials, which the minimal environment of the
        // other adapters (`crate::run::command`) would drop.
        cmd.envs(std::env::vars_os());
        cmd.arg("submit");
        crate::run::captured(cmd, Some(request.to_string().as_bytes()), timeout).map_err(|fault| {
            match fault {
                crate::run::Fault::Timeout(_) => (
                    "submission_timeout",
                    format!(
                        "the runner did not admit {op_id} within the submission timeout of {}s",
                        timeout.as_secs()
                    ),
                ),
                crate::run::Fault::Spawn(error) => (
                    "submission_failed",
                    format!("the runner command {name} could not be started: {error}"),
                ),
                other => ("submission_failed", other.to_string()),
            }
        })
    })
    .and_then(|output| {
        if output.status.success() {
            Ok(output)
        } else {
            Err((
                "submission_refused",
                format!(
                    "the runner refused the submission: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ))
        }
    })
    .and_then(|output| {
        let reply: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|error| {
            (
                "submission_refused",
                format!("the runner's submit reply is not JSON: {error}"),
            )
        })?;
        reply["job"].as_str().map(str::to_owned).ok_or((
            "submission_refused",
            "the runner's submit reply has no job".to_owned(),
        ))
    });
    // A submission the executor never took ends the operation with a stated outcome (§ 9a),
    // so no executor pass lands what this land answered as failed.
    let job = match started {
        Ok(job) => job,
        Err((outcome, why)) => return Err(not_admitted(ctx, op_id, revision, outcome, why)),
    };
    let op = ctx.op_mut(op_id);
    op.job = Some(job);
    op.state = OpState::Admitted;
    ctx.save(revision, None, &format!("admitted {op_id}"))?;
    ctx.push_chain();
    Ok(())
}

/// Ends operation `op_id` `failed` with `outcome` and pushes it; returns `why`.
fn not_admitted(ctx: &mut Ctx, op_id: &str, revision: &str, outcome: &str, why: String) -> String {
    let op = ctx.op_mut(op_id);
    op.state = OpState::Failed;
    op.outcome = Some(outcome.to_owned());
    let label = outcome.replace('_', " ");
    if let Err(error) = ctx.save(revision, None, &format!("failed {op_id}: {label}")) {
        return format!("{why}; recording the failure failed too: {error}");
    }
    ctx.push_chain();
    why
}

/// NEW-4: the outcome of a lane change's land request, a check-only operation. Its candidate
/// was built on the current trunk and checked like any other (`refusal` is how the check ended
/// when it did not pass); it is never published. The operation is held, `lane::CHECKED`, on the
/// condition of that candidate, and the result is `ok` with the verdicts so the agent learns
/// them (0.3.0: it is not a refusal). A refusal that is not a check's verdict (a conflict, a check no worker could run, an
/// operation another executor holds) is returned as it is: no check ran to an answer.
fn lane_never_lands(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    refusal: Option<BayResult>,
) -> Result<BayResult, String> {
    const VERDICTS: [Reason; 4] = [
        Reason::CheckFailed,
        Reason::CheckFlaky,
        Reason::CheckUnknown,
        Reason::InputChanged,
    ];
    let refusal = match refusal {
        Some(refusal)
            if !refusal
                .reason
                .is_some_and(|reason| VERDICTS.contains(&reason)) =>
        {
            return Ok(refusal);
        }
        refusal => refusal,
    };
    let Some(attempt) = ctx
        .find_op(op_id)
        .and_then(|op| op.attempts.last())
        .cloned()
    else {
        return Ok(refusal.unwrap_or_else(|| {
            BayResult::failed(format!("operation {op_id} has no checked candidate"))
        }));
    };
    let git = &ctx.repo.git;
    let checks: Vec<serde_json::Value> = evidence::records_for(git, &attempt.tree)?
        .into_iter()
        .filter(|record| record.subject.commit == attempt.commit)
        .map(|record| {
            json!({"check": record.check.id, "verdict": record.verdict,
                "producer": record.producer.name, "why": record.why})
        })
        .collect();
    let blocked_on = condition(
        git,
        revision,
        &attempt.base,
        Some(&attempt.tree),
        &ctx.record.observed_versions,
    )?;
    let lane = ctx.record.lane.clone();
    if let Some(op) = ctx.find_op_mut(op_id) {
        op.state = OpState::Blocked;
        op.outcome = Some(crate::lane::CHECKED.to_owned());
        op.blocked_on = Some(blocked_on);
        ctx.save(
            revision,
            None,
            &format!("blocked {op_id}: a lane change never lands"),
        )?;
        ctx.push_chain();
    }
    let result = BayResult::new(Decision::Ok)
        .with_change(ctx.id.clone())
        .with_op(op_id)
        .with_detail(json!({
            "lane": lane,
            "trunk": attempt.base,
            "candidate": attempt.candidate_id,
            "verdict": attempt.verdict,
            "checks": checks,
            "check_refusal": refusal.as_ref().and_then(|refusal| refusal.reason),
            "refused_checks": refusal
                .as_ref()
                .and_then(|refusal| refusal.detail.as_ref())
                .map(|detail| detail["checks"].clone()),
            "note": crate::lane::NOTE,
        }));
    Ok(result)
}

/// NEW-4: `LANE_LEAK` for a change of no lane whose history reaches commits of a lane ref that
/// are not on `trunk`: its frozen `revision`, its view's `HEAD`, and a merge or cherry-pick under
/// way there (`MERGE_HEAD`, `CHERRY_PICK_HEAD`). The lane refs are fetched first, best effort.
/// Bytes copied into the view by hand carry no history, so they are never found here.
fn lane_leak(ctx: &Ctx, trunk: &str, revision: &str) -> Result<Option<BayResult>, String> {
    let repo = ctx.repo;
    if repo.config.lanes.is_empty() {
        return Ok(None);
    }
    crate::lane::fetch(repo);
    let (tips, truncated) = crate::lane::tips(&repo.git, &repo.config.lanes, repo.remote())?;
    if tips.is_empty() {
        return Ok(None);
    }
    let view = Git::new(repo.view_path(&ctx.id));
    let mut heads = vec![revision.to_owned()];
    for name in ["HEAD", "MERGE_HEAD", "CHERRY_PICK_HEAD"] {
        if let Ok(commit) =
            view.run(&["rev-parse", "--verify", "-q", &format!("{name}^{{commit}}")])
            && !heads.contains(&commit)
        {
            heads.push(commit);
        }
    }
    let heads: Vec<&str> = heads.iter().map(String::as_str).collect();
    let leaks = crate::lane::reached(&repo.git, &tips, &heads, Some(trunk))?;
    if leaks.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        BayResult::blocked(Reason::LaneLeak)
            .with_change(ctx.id.clone())
            .with_detail(json!({
                "lane": leaks[0].lane,
                "leaks": leaks.iter().map(crate::lane::Leak::to_json).collect::<Vec<_>>(),
                "trunk": trunk,
                "tips_truncated": truncated,
                "repair": "take the lane's commits out of the view (reset it to the change's base and redo the work there), then bay merge again; or start the work in the lane: bay start <intent> --lane <lane>",
            })),
    ))
}

/// G244: trunk is red on paths this change touches, and the change does not name the landing
/// that made it red (by `--fixes`, an undo, or a supersede): `MAIN_RED`, naming the owner and
/// the ways out. The operation is blocked on trunk's state, never failed. A check no landing
/// in the window answers (NEW-6, [`crate::red::Pending`]) is unknown, not red: it refuses
/// nothing here, and `bay status` lists it as `red_pending`, never as clear.
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
                    "merge a change that names the owner: bay start --fixes <owner>, or bay undo <owner>",
                    "a later run of the post check passes on main",
                    "quarantine the check in the policy",
                ],
                "owners": owners,
            })),
    ))
}

/// N-8 (`crate::hold`): trunk is held, and this change carries neither `Fixes:` nor `Reverts:`:
/// `MAIN_HELD`, naming the hold. Nothing is published and no limit is counted. The operation
/// stays queued and holds no queue place, so a fix behind it reaches the head: a place it holds
/// is released as `trunk_held`, which counts toward neither `QUEUE_DEADLINE_EXCEEDED` nor
/// `MERGE_RETRY_EXHAUSTED` (§ 6). A claim this executor holds is given back, and a later
/// pass tries the operation again; once the hold clears it rejoins the queue. An operation
/// another executor holds is left as it is. `None` when trunk is clear or the change passes.
fn hold_gate(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    trunk: &str,
) -> Result<Option<BayResult>, String> {
    let op = ctx.op(op_id);
    if op.state.is_terminal() || op.state == OpState::Blocked {
        return Ok(None);
    }
    if crate::hold::passes(ctx.record.fixes.as_ref(), ctx.record.reverts.as_ref()) {
        return Ok(None);
    }
    let Some(hold) = crate::hold::held(ctx.repo, trunk) else {
        return Ok(None);
    };
    wait_unplaced(
        ctx,
        op_id,
        revision,
        Reason::TrunkHeld,
        crate::hold::RELEASE,
        &format!("held {op_id}: trunk is held ({})", hold.source),
    )?;
    let why = match &hold.end {
        Some(end) => format!(
            "main is held until {end} ({}), and this change carries no Fixes: or Reverts:",
            hold.reason
        ),
        None => format!(
            "main is held ({}), and this change carries no Fixes: or Reverts:",
            hold.reason
        ),
    };
    let mut result = BayResult::refused(Decision::Queued, Reason::TrunkHeld)
        .with_change(ctx.id.clone())
        .with_op(op_id)
        .with_detail(json!({
            "hold": hold.to_json(),
            "why": why,
            "trunk": trunk,
            "ways_out": [
                "wait: the operation stays queued and merges once the hold clears (bay merge --wait again, or a runner pass)",
                "merge a fix or a revert: bay start --fixes <change>, or bay undo <change>",
            ],
        }));
    result.waiting_for = Some("hold".to_owned());
    Ok(Some(result))
}

/// Whether another executor holds `op_id` (it runs it, or has checked it): a gate then leaves
/// the operation as it is.
fn held_elsewhere(ctx: &Ctx, op_id: &str) -> bool {
    let name = ctx.executor_name();
    let op = ctx.op(op_id);
    op.owner.as_ref().is_some_and(|owner| *owner != name)
        && matches!(op.state, OpState::Running | OpState::Checked)
}

/// Gives back the queue place `op_id` holds, if any, released as `release`.
fn give_back_place(ctx: &Ctx, op_id: &str, release: &str) -> Result<(), String> {
    let queue = crate::queue::Queue::read(ctx.repo)?;
    if let Some(entry) = queue.of(op_id).cloned() {
        let signer = ctx.executor_signer().ok().flatten();
        crate::queue::release(ctx.repo, signer.as_ref(), entry.seq, op_id, release)?;
    }
    Ok(())
}

/// N-8, G273: `op_id` waits, as `reason`, on a condition outside its own work (a held trunk,
/// a dependency that has not landed), and holds no queue place meanwhile, so the operations
/// behind it reach the head. A place it holds is released as `release`, a reason that counts
/// toward neither `QUEUE_DEADLINE_EXCEEDED` nor `MERGE_RETRY_EXHAUSTED` (§ 6). A claim this
/// executor holds is given back (a running operation returns to admitted or submitted) and the
/// outcome names `reason`, written once with the chain message `message`; a later pass tries
/// the operation again. An operation another executor holds is left as it is.
fn wait_unplaced(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    reason: Reason,
    release: &str,
    message: &str,
) -> Result<(), String> {
    if held_elsewhere(ctx, op_id) {
        return Ok(());
    }
    give_back_place(ctx, op_id, release)?;
    let code = reason.code();
    let op = ctx.op_mut(op_id);
    if op.outcome.as_deref() != Some(code) || op.owner.is_some() {
        op.outcome = Some(code.to_owned());
        op.owner = None;
        if op.state == OpState::Running {
            op.state = if op.job.is_some() {
                OpState::Admitted
            } else {
                OpState::Submitted
            };
        }
        ctx.save(revision, None, message)?;
        ctx.push_chain();
    }
    Ok(())
}

/// G273 (`crate::after`): the change lands only after the changes its record names. While one
/// of them has not landed on trunk at `trunk`, the operation waits with `DEPENDENCY_PENDING`,
/// publishes nothing and holds no queue place, released as `dependency_pending`
/// ([`wait_unplaced`]); a later pass tries it again. Once one of them ended without landing,
/// the operation is blocked `DEPENDENCY_ABANDONED`, naming it. `None` when the change names no
/// dependency, or every one has landed.
fn dependency_gate(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    trunk: &str,
) -> Result<Option<BayResult>, String> {
    if ctx.record.after.is_empty() {
        return Ok(None);
    }
    let op = ctx.op(op_id);
    if op.state.is_terminal() || op.state == OpState::Blocked {
        return Ok(None);
    }
    let Some(result) = dependency_waiting(ctx, op_id, trunk)? else {
        return Ok(None);
    };
    if result.reason == Some(Reason::DependencyAbandoned) {
        if !held_elsewhere(ctx, op_id) {
            give_back_place(ctx, op_id, "dependency_abandoned")?;
            let op = ctx.op_mut(op_id);
            op.state = OpState::Blocked;
            op.outcome = Some(Reason::DependencyAbandoned.code().to_owned());
            op.owner = None;
            ctx.save(
                revision,
                None,
                &format!("blocked {op_id}: a change it lands after ended without landing"),
            )?;
            ctx.push_chain();
        }
        return Ok(Some(result));
    }
    let waiting_on = ctx.record.after.join(", ");
    wait_unplaced(
        ctx,
        op_id,
        revision,
        Reason::DependencyPending,
        crate::after::RELEASE,
        &format!("waits {op_id}: it lands after {waiting_on}"),
    )?;
    Ok(Some(result))
}

/// G273: what a land that does not run `op_id` answers while a change it lands after is not on
/// trunk at `trunk` ([`dependency_waiting`]), with the operation's progress; nothing is written.
/// `None` for a lane change, one that names no dependency, or an operation that has ended or is
/// blocked.
fn dependency_unrun(ctx: &Ctx, op_id: &str, trunk: &str) -> Result<Option<BayResult>, String> {
    let op = ctx.op(op_id);
    if ctx.record.lane.is_some()
        || ctx.record.after.is_empty()
        || op.state.is_terminal()
        || op.state == OpState::Blocked
    {
        return Ok(None);
    }
    Ok(dependency_waiting(ctx, op_id, trunk)?
        .map(|waiting| with_progress(&ctx.repo.git, op, waiting)))
}

/// G273: the result of `op_id` while a change it lands after is not on trunk at `trunk`, read
/// with no write: `DEPENDENCY_ABANDONED` (blocked) when one ended without landing, else
/// `DEPENDENCY_PENDING` (queued) when one has not landed yet, else `None`.
fn dependency_waiting(ctx: &Ctx, op_id: &str, trunk: &str) -> Result<Option<BayResult>, String> {
    use crate::after::Standing;
    let standing = crate::after::standing(ctx.repo, &ctx.record.after, trunk)?;
    let after: Vec<serde_json::Value> = standing
        .iter()
        .map(crate::after::Dependency::to_json)
        .collect();
    let ended: Vec<&crate::after::Dependency> = standing
        .iter()
        .filter(|dependency| matches!(dependency.standing, Standing::Ended(_)))
        .collect();
    if let Some(first) = ended.first() {
        let intent = &ctx.record.intent.reference;
        let named = &first.change;
        let result = BayResult::blocked(Reason::DependencyAbandoned)
            .with_change(ctx.id.clone())
            .with_op(op_id)
            .with_detail(json!({
                "ended": ended.iter().copied().map(crate::after::Dependency::to_json).collect::<Vec<_>>(),
                "after": after,
                "why": format!(
                    "this change merges only after {named}, which ended without merging, so it can never merge"
                ),
                "trunk": trunk,
                "recovery": format!(
                    "drop the dependency: bay abandon here, then bay start {intent} --new without --after {named}; or restart {named} (bay start of its intent adopts it), merge it, then bay merge again"
                ),
            }));
        return Ok(Some(result));
    }
    let pending: Vec<&crate::after::Dependency> = standing
        .iter()
        .filter(|dependency| matches!(dependency.standing, Standing::Pending(_)))
        .collect();
    if pending.is_empty() {
        return Ok(None);
    }
    let names: Vec<&str> = pending
        .iter()
        .map(|dependency| dependency.change.as_str())
        .collect();
    let mut result = BayResult::refused(Decision::Queued, Reason::DependencyPending)
        .with_change(ctx.id.clone())
        .with_op(op_id)
        .with_detail(json!({
            "pending": pending.iter().copied().map(crate::after::Dependency::to_json).collect::<Vec<_>>(),
            "after": after,
            "why": format!(
                "this change merges only after {}, not yet merged into main",
                names.join(", ")
            ),
            "trunk": trunk,
            "ways_out": [
                "wait: the operation stays queued and merges once every change it names has merged (bay merge --wait again, or a runner pass)",
                "merge the changes it names first",
            ],
        }));
    result.waiting_for = Some("dependencies".to_owned());
    Ok(Some(result))
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

/// `CHANGE_TOO_LARGE` at candidate build (owner ruling 2026-10-21): the merged `tree` changes
/// `trunk` past a `[limits]` limit of the policy at `trunk`. The operation is blocked on the
/// limit, as on a conflict; no candidate is committed and no check runs.
fn too_large(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    trunk: &str,
    tree: &str,
) -> Result<Option<BayResult>, String> {
    let repo = ctx.repo;
    let git = &repo.git;
    let limits = policy::limits_at(git, trunk)?;
    let exceeded = crate::limits::exceeded(git, &limits, trunk, tree)?;
    if exceeded.is_empty() {
        return Ok(None);
    }
    let condition = condition(git, revision, trunk, None, &ctx.record.observed_versions)?;
    let op = ctx.op_mut(op_id);
    op.state = OpState::Blocked;
    op.outcome = Some(Reason::ChangeTooLarge.code().to_owned());
    op.blocked_on = Some(condition);
    ctx.save(revision, None, "blocked: change too large")?;
    ctx.push_chain();
    Ok(Some(
        crate::limits::refusal(&limits, &exceeded, trunk, "candidate")
            .with_change(ctx.id.clone())
            .with_op(op_id),
    ))
}

/// `LOCK_NOT_HELD` at candidate build (BACKLOG 57): the merged `tree` changes a path the policy
/// at `trunk` names lockable (`[locks] paths`), and no commit of that path's lock names this
/// change (a save names its change in its lock). Such an edit
/// went around the lock, and two copies of a file that never merges cannot be combined. The
/// operation is blocked, as on a conflict; no candidate is committed and no check runs.
fn lock_not_held(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    trunk: &str,
    tree: &str,
) -> Result<Option<BayResult>, String> {
    let repo = ctx.repo;
    let paths = crate::locks::not_held(repo, trunk, tree, &ctx.id)?;
    if paths.is_empty() {
        return Ok(None);
    }
    let condition = condition(
        &repo.git,
        revision,
        trunk,
        None,
        &ctx.record.observed_versions,
    )?;
    let op = ctx.op_mut(op_id);
    op.state = OpState::Blocked;
    op.outcome = Some(Reason::LockNotHeld.code().to_owned());
    op.blocked_on = Some(condition);
    ctx.save(revision, None, "blocked: lock not held")?;
    ctx.push_chain();
    Ok(Some(
        BayResult::blocked(Reason::LockNotHeld)
            .with_change(ctx.id.clone())
            .with_op(op_id)
            .with_detail(json!({"paths": paths, "main": trunk})),
    ))
}

/// `ASSET_MISSING` at candidate build (BACKLOG 49, owner ruling 2026-10-24): the merged `tree`'s
/// `.bay-assets` adds lines whose bytes the assets plugin does not hold (`exists`), or the
/// plugin cannot say. A merge would name bytes no store keeps, so the operation is blocked, as
/// on a conflict; no candidate is committed and no check runs.
fn asset_missing(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    trunk: &str,
    tree: &str,
) -> Result<Option<BayResult>, String> {
    let repo = ctx.repo;
    let detail = match crate::assets::not_held_at_merge(repo, trunk, tree) {
        Ok(paths) if paths.is_empty() => return Ok(None),
        Ok(paths) => json!({"paths": paths, "main": trunk}),
        Err(why) => json!({"why": why, "main": trunk}),
    };
    let condition = condition(
        &repo.git,
        revision,
        trunk,
        None,
        &ctx.record.observed_versions,
    )?;
    let op = ctx.op_mut(op_id);
    op.state = OpState::Blocked;
    op.outcome = Some(Reason::AssetMissing.code().to_owned());
    op.blocked_on = Some(condition);
    ctx.save(revision, None, "blocked: asset missing")?;
    ctx.push_chain();
    Ok(Some(
        BayResult::blocked(Reason::AssetMissing)
            .with_change(ctx.id.clone())
            .with_op(op_id)
            .with_detail(detail),
    ))
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
    approved: Option<Attempt>,
) -> Result<Result<Attempt, BayResult>, String> {
    let judged = match input_gate(ctx, op_id, revision, trunk)? {
        Ok(judged) => judged,
        Err(refusal) => return Ok(Err(refusal)),
    };
    // NEW-4: a red trunk holds landings; a lane change never lands, so its checks run.
    if ctx.record.lane.is_none()
        && let Some(red) = red_gate(ctx, op_id, revision, trunk)?
    {
        return Ok(Err(red));
    }
    let again = approved.is_some();
    // G284: the candidate an approval arrived for, on the trunk it was merged onto: that same
    // commit is checked again, so the evidence it already has answers its checks.
    let (candidate_id, commit, tree) = if let Some(attempt) = approved {
        anchor(&ctx.repo.git, op_id, &attempt.commit)?;
        (attempt.candidate_id, attempt.commit, attempt.tree)
    } else {
        let tree = match merged(ctx, op_id, revision, trunk)? {
            Ok(tree) => tree,
            Err(refusal) => return Ok(Err(refusal)),
        };
        // Owner ruling 2026-10-21: the candidate's change to trunk, over a size limit of the
        // policy at trunk, is refused before it is committed or checked.
        if let Some(refusal) = too_large(ctx, op_id, revision, trunk, &tree)? {
            return Ok(Err(refusal));
        }
        // BACKLOG 57: a file that never merges reaches main only from the change that held
        // its lock.
        if let Some(refusal) = lock_not_held(ctx, op_id, revision, trunk, &tree)? {
            return Ok(Err(refusal));
        }
        // BACKLOG 49: the bytes a change's manifest names are in the store before it merges.
        if let Some(refusal) = asset_missing(ctx, op_id, revision, trunk, &tree)? {
            return Ok(Err(refusal));
        }
        if let Some(no_effect) = no_effect(ctx, op_id, revision, trunk, &tree)? {
            return Ok(Err(no_effect));
        }
        anchor(&ctx.repo.git, op_id, &tree)?;

        let candidate_id = format!("{op_id}.{}", ctx.op(op_id).attempts.len() + 1);
        let request_id = ctx.op(op_id).request_id.clone().unwrap_or_default();
        // N-21: an unsigned landing (no registry, or `signing = "approvals"`) names none.
        let registry_id = ctx
            .signing()
            .map_or_else(|| "none".to_owned(), |registry| registry.commit.clone());
        let message = candidate_message(ctx, revision, &request_id, &candidate_id, &registry_id);
        let commit = commit_candidate(ctx, &tree, trunk, &message)?;
        anchor(&ctx.repo.git, op_id, &commit)?;
        kill_point("after-commit");
        (candidate_id, commit, tree)
    };

    // Step 4: check the exact candidate under the policy at its base.
    ctx.step.set("check");
    let (has_policy, verdict, refusal) = check_candidate(
        ctx,
        op_id,
        &candidate_id,
        (commit.as_str(), tree.as_str(), revision),
        trunk,
        &inputs::request_inputs(&judged),
    )?;
    let refusal = judged_outcome(ctx, op_id, &candidate_id, refusal, &judged);
    // Heartbeats moved the chain while the check ran; continue from the last one pushed.
    if let Some(beat) = ctx.beat_pushed.borrow_mut().take() {
        ctx.head.clone_from(&beat);
        ctx.pushed_head = Some(beat);
        ctx.unpushed_from = None;
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
        candidate_id,
        base: trunk.to_owned(),
        commit,
        tree,
        verdict: verdict.to_owned(),
        epoch: ctx.my_epoch,
        batch: None,
    };
    persist_attempt(ctx, op_id, revision, attempt, refusal, has_policy, again)
}

/// Step 5 for a checked `attempt`: its evidence is pushed and read back from the remote, then
/// the candidate is linked into the chain as checked, or as blocked on `refusal`. With `again`
/// (G284) the attempt is the operation's last candidate checked once more, and it takes that
/// attempt's place: one candidate, one attempt.
fn persist_attempt(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    attempt: Attempt,
    refusal: Option<BayResult>,
    has_policy: bool,
    again: bool,
) -> Result<Result<Attempt, BayResult>, String> {
    let blocked_on = if refusal.is_some() {
        Some(condition(
            &ctx.repo.git,
            revision,
            &attempt.base,
            Some(&attempt.tree),
            &ctx.record.observed_versions,
        )?)
    } else {
        None
    };
    // The evidence is on the remote before the chain names the candidate checked.
    if refusal.is_none() && has_policy {
        push_and_read_back_evidence(ctx.repo, &ctx.producer()?.0)?;
    }
    // N-9: a check's refusal shares its evidence too, so the approver who may waive the check
    // reads its real verdict. The refusal stands when that push fails; the approver then
    // reads `unknown`, never a pass.
    if has_policy
        && refusal.as_ref().is_some_and(|refusal| {
            matches!(
                refusal.reason,
                Some(Reason::CheckFailed | Reason::CheckFlaky | Reason::CheckUnknown)
            )
        })
    {
        let _ = push_and_read_back_evidence(ctx.repo, &ctx.producer()?.0);
    }
    let (state, word) = if refusal.is_some() {
        (OpState::Blocked, "blocked")
    } else {
        (OpState::Checked, "checked")
    };
    let outcome = refusal
        .as_ref()
        .and_then(|refusal| refusal.reason)
        .map(|reason| reason.code().to_owned());
    let message = format!("{word} {}", attempt.candidate_id);
    let persisted = persist_owned(ctx, op_id, revision, &attempt.commit, &message, |op| {
        if again
            && op
                .attempts
                .last()
                .is_some_and(|last| last.candidate_id == attempt.candidate_id)
        {
            op.attempts.pop();
        }
        op.attempts.push(attempt.clone());
        op.unavailable = None;
        op.state = state;
        op.outcome.clone_from(&outcome);
        op.blocked_on.clone_from(&blocked_on);
    })?;
    match (persisted, refusal) {
        (Persisted::Lost(result), _) => Ok(Err(*result)),
        (_, Some(refusal)) => Ok(Err(refusal)),
        (Persisted::Unpushed, None) => {
            Err("the change chain could not be pushed; nothing was merged".to_owned())
        }
        (Persisted::Pushed, None) => {
            kill_point("after-persist");
            Ok(Ok(attempt))
        }
    }
}

/// What [`persist_owned`] did.
enum Persisted {
    /// The write is on the remote chain.
    Pushed,
    /// The write is in the local chain only: the remote could not be reached or kept moving.
    Unpushed,
    /// The operation is no longer this executor's: the result says so.
    Lost(Box<BayResult>),
}

/// Step 5's write (§ 9a): `apply` to the operation this executor claimed, appended and pushed
/// under the chain's compare-and-swap. A lost race, on this box's shared chain ref or on the
/// remote, re-reads the remote chain and applies again while the operation is still this
/// executor's epoch; once another epoch holds it, or the remote lacks it, it is `superseded`.
fn persist_owned(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    candidate: &str,
    message: &str,
    apply: impl Fn(&mut Operation),
) -> Result<Persisted, String> {
    for _ in 0..3 {
        let mine = ctx.my_epoch;
        let Some(op) = ctx.find_op_mut(op_id) else {
            return Ok(Persisted::Lost(Box::new(superseded(
                &ctx.id, op_id, 0, None,
            ))));
        };
        if op.epoch != mine || op.state.is_terminal() {
            let (epoch, owner) = (op.epoch, op.owner.clone());
            return Ok(Persisted::Lost(Box::new(superseded(
                &ctx.id,
                op_id,
                epoch,
                owner.as_ref(),
            ))));
        }
        apply(op);
        if ctx.save_cas(revision, Some(candidate), message)? && ctx.push_chain() {
            return Ok(Persisted::Pushed);
        }
        if ctx.refetch().is_err() {
            return Ok(Persisted::Unpushed);
        }
    }
    Ok(Persisted::Unpushed)
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
                "repair": format!("make main's provider for {} answer", unknown.input),
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
        format!("read the required version: bay start {}", ctx.id)
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

/// Why a candidate that edits the policy or an adapter path does not raise the revision of the
/// policy at `trunk` (§ 7), or that revision's read fault; `None` when it raises it. An invalid
/// base policy still states its revision, and a repair must raise it too.
fn unraised(
    git: &Git,
    trunk: &str,
    base_policy: Option<&policy::Policy>,
    own_policy: &policy::Policy,
) -> Option<String> {
    let base_revision = match base_policy {
        Some(policy) => policy.revision,
        None => match policy::revision_at(git, trunk) {
            Ok(revision) => revision.unwrap_or(0),
            Err(why) => return Some(why),
        },
    };
    (own_policy.revision <= base_revision).then(|| {
        format!(
            "this change edits the policy or an adapter path, so it must raise revision above {base_revision}; it has {}",
            own_policy.revision
        )
    })
}

/// Step 4: runs the base policy's checks on the candidate and combines the evidence on its
/// tree. Returns whether a judging policy ran checks (so there is evidence to push), the
/// attempt's verdict word ([`UNCHECKED`] when the policy that judges it is unchecked), and the
/// refusal when any required check is not `pass` or the base has no policy.
fn check_candidate(
    ctx: &Ctx,
    op_id: &str,
    candidate_id: &str,
    (commit, tree, revision): (&str, &str, &str),
    trunk: &str,
    inputs: &[String],
) -> Result<(bool, &'static str, Option<BayResult>), String> {
    let git = &ctx.repo.git;
    let facts = change_facts(ctx, revision);
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
        run_policy(
            ctx,
            op_id,
            id,
            (commit, tree, Some(&facts)),
            trunk,
            policy,
            from,
            inputs,
        )
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
    if let Some(why) = unraised(git, trunk, base_policy.as_ref(), &own_policy) {
        return refuse(why);
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
                        "the base {base} has no {}, so the criteria are unknown; add a policy there, or declare unchecked = true in it to merge without checks",
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
    let mut args = vec!["diff", "--name-only", "--no-renames", trunk, commit, "--"];
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
        .run(&["diff", "--name-only", "--no-renames", trunk, commit])
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

/// The change `ctx` lands at `revision`, as a check request names it (`INTERFACES` § 3).
fn change_facts(ctx: &Ctx, revision: &str) -> check::ChangeFacts {
    check::ChangeFacts {
        id: ctx.id.clone(),
        intent_reference: ctx.record.intent.reference.clone(),
        intent_revision: ctx.record.intent.revision.clone(),
        actor: ctx.record.actor.name.clone(),
        revision: revision.to_owned(),
    }
}

/// The counted records on `tree`, with the approvals of earlier candidates of this change that
/// still hold (G292: an approval holds while the files it covers are unchanged; it answers as
/// its approver signed it, and nothing new is written). A batch's tip lands several changes,
/// so no one change's approvals carry onto it (`one_change` false).
fn counted_and_carried(
    ctx: &Ctx,
    policy: &policy::Policy,
    (tree, base_tree): (&str, &str),
    one_change: bool,
) -> Result<(Vec<evidence::EvidenceRecord>, throughput::CarriedApprovals), String> {
    let git = &ctx.repo.git;
    let mut all = evidence::counted(
        git,
        evidence::records_for(git, tree)?,
        ctx.registry.as_ref(),
    )?;
    let carried = if one_change {
        throughput::carried_approvals(
            git,
            policy,
            ctx.registry.as_ref(),
            (tree, base_tree),
            &all,
            &throughput::candidates_of(&ctx.record),
        )?
    } else {
        throughput::CarriedApprovals::default()
    };
    all.extend(carried.records.iter().cloned());
    Ok((all, carried))
}

/// Runs `policy`'s checks on the candidate with the adapter materialized from `adapter_from`,
/// and combines the evidence on its tree under that policy's manifest. `change` is the change
/// the candidate lands, when it lands one (a batch's lands several).
#[allow(clippy::too_many_arguments)]
fn run_policy(
    ctx: &Ctx,
    op_id: &str,
    candidate_id: &str,
    (commit, tree, change): (&str, &str, Option<&check::ChangeFacts>),
    trunk: &str,
    policy: &policy::Policy,
    adapter_from: &str,
    inputs: &[String],
) -> Result<(&'static str, Option<BayResult>), String> {
    let git = &ctx.repo.git;
    let base_tree = tree_of(git, trunk)?;
    // The checks' scratch stays inside the git common dir: private to the repository, on its
    // filesystem, and hidden from every check but its own part (G26, G62). Its path holds a
    // `.git` part, so an adapter judges paths relative to its tree (G241).
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
            candidate: candidate_id,
            tree,
            commit,
            base: trunk,
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
            onto: trunk,
            change,
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
            sandbox: check::Sandbox::here().reading(git),
            baseline: baseline_of(ctx, policy, trunk, &base_tree)?,
            parallel: ctx.repo.config.parallel_checks(),
        },
        &mut check::Hooks {
            keep: &mut keep,
            beat: &mut || heartbeat(ctx, op_id),
            seen: None,
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
    let (all, carried) = counted_and_carried(ctx, policy, (tree, &base_tree), change.is_some())?;
    // What to repair: this run's records and the ones reused in place of a run (§ 8).
    let named: Vec<&evidence::EvidenceRecord> = records.iter().chain(&reused).collect();
    Ok(judge_seen(
        ctx,
        op_id,
        policy,
        &Seen {
            candidate_id,
            adapter_from,
            commit,
            tree,
            changed: &changed,
            all: &all,
            named: &named,
            carried: &carried,
        },
    ))
}

/// N-3: what a check of `policy` with `baseline = "base"` is judged against: `trunk`, the
/// commit the candidate was merged onto, and the counted records on its tree (`base_tree`) that
/// answer the policy's checks, read from every producer after a fetch, so the base's own
/// record, or a base run another candidate made, is reused rather than run again. `None` when no
/// check of `policy` asks for a baseline: nothing is read.
///
/// # Errors
///
/// A git fault, an unreadable note, or a class file that cannot be written.
fn baseline_of(
    ctx: &Ctx,
    policy: &policy::Policy,
    trunk: &str,
    base_tree: &str,
) -> Result<Option<check::Baseline>, String> {
    if !policy.checks.iter().any(|entry| entry.baseline.is_some()) {
        return Ok(None);
    }
    let git = &ctx.repo.git;
    ctx.repo.fetch_evidence(&ctx.keys);
    let records = policy.answering(evidence::counted(
        git,
        evidence::records_for(git, base_tree)?,
        ctx.registry.as_ref(),
    )?);
    Ok(Some(check::Baseline {
        commit: trunk.to_owned(),
        records,
    }))
}

/// What one run of a policy's checks saw on a candidate, for its verdict (§ 7).
struct Seen<'a> {
    candidate_id: &'a str,
    adapter_from: &'a str,
    commit: &'a str,
    tree: &'a str,
    /// The paths the candidate's diff against its base changes (G146).
    changed: &'a [String],
    /// Every counted record on the candidate's tree.
    all: &'a [evidence::EvidenceRecord],
    /// What to repair: this run's records and the ones reused in place of a run (§ 8).
    named: &'a [&'a evidence::EvidenceRecord],
    /// G292: the approvals of earlier candidates of the change that answer here (in `all`
    /// too), and those set aside because the files they cover changed.
    carried: &'a throughput::CarriedApprovals,
}

/// The verdict of `policy` on what `seen` saw: `pass`, or the verdict word and the refusal
/// that names each gap, with the approvals still owed (G245).
fn judge_seen(
    ctx: &Ctx,
    op_id: &str,
    policy: &policy::Policy,
    seen: &Seen,
) -> (&'static str, Option<BayResult>) {
    let (required, _) = policy.required(seen.changed);
    let registry = ctx.registry.as_ref();
    let Some((code, word, why)) = verdict(policy, &required, seen.all, seen.named, seen.commit)
    else {
        return ("pass", None);
    };
    let mut detail = json!({
        "candidate": seen.candidate_id,
        "adapter_from": seen.adapter_from,
        "checks": why,
        "trust": evidence::trust(registry),
    });
    // G245: the approvals still owed, who can give them, and the exact candidate to name.
    let owed = approval_rows(
        policy,
        seen.changed,
        registry,
        (seen.all, seen.carried),
        seen.commit,
    );
    if !owed.is_empty() {
        detail["approval"] = approval_detail(&owed, seen.commit, seen.tree, registry);
        // G281: the one command an approver runs to answer it.
        detail["approval"]["approve"] = json!(format!("bay approve {}", ctx.id));
    }
    (
        word,
        Some(
            BayResult::blocked(code)
                .with_change(ctx.id.clone())
                .with_op(op_id)
                .with_detail(detail),
        ),
    )
}

/// G245: each approval check `policy` requires of a candidate whose diff changes `changed`
/// that the counted `records` on its tree do not pass: its state (`pending`, or `rejected` by a
/// `product_failure`), the enrolled approvers who may answer it (the registry's approver file,
/// narrowed by the check's `producers` list), the changed paths its `when_paths` cover (G280),
/// and the command that records the answer for `commit`, the candidate. `records` hold the
/// approvals `carried` from earlier candidates of the change (G292); each earlier approval
/// `carried` set aside is named on its row under `set_aside`, with the gated paths that changed.
fn approval_rows(
    policy: &policy::Policy,
    changed: &[String],
    registry: Option<&Registry>,
    (records, carried): (&[evidence::EvidenceRecord], &throughput::CarriedApprovals),
    commit: &str,
) -> Vec<serde_json::Value> {
    let listed = policy.answering(records.to_vec());
    let enrolled = registry
        .map(|registry| registry.principals(KeyClass::Approver))
        .unwrap_or_default();
    policy
        .checks
        .iter()
        .filter(|entry| entry.is_approval() && policy.requires(entry, changed))
        .filter_map(|entry| {
            let id = evidence::CheckId::new(&entry.id, &entry.environment);
            let state = match evidence::combine(&listed, &id, &policy.manifest_digest, None) {
                Combined::Pass => return None,
                Combined::Unknown => "pending",
                Combined::ProductFailure | Combined::Flaky => "rejected",
            };
            let approvers: Vec<&String> = enrolled
                .iter()
                .filter(|name| {
                    entry
                        .producers
                        .as_ref()
                        .is_none_or(|names| names.contains(*name))
                })
                .collect();
            let mut row = json!({
                "check": entry.id,
                "environment": entry.environment,
                "state": state,
                "approvers": approvers,
                // G280: the changed paths this check's `when_paths` cover, what is approved.
                "paths": entry.woken_paths(changed),
                "record": format!(
                    "bay evidence record --kind approval --check {} --verdict pass --commit {commit}",
                    entry.id
                ),
            });
            // G292: an earlier approval that no longer holds, and the gated paths that changed.
            let set_aside = carried.set_aside_of(&entry.id, &entry.environment);
            if !set_aside.is_empty() {
                row["set_aside"] = json!(set_aside);
            }
            Some(row)
        })
        .collect()
}

/// The `approval` detail of a refusal or a status (G245): the owed `rows`, the candidate's
/// commit and tree an approval must name, and, without a registry, why none can pass.
fn approval_detail(
    rows: &[serde_json::Value],
    commit: &str,
    tree: &str,
    registry: Option<&Registry>,
) -> serde_json::Value {
    let mut detail = json!({"checks": rows, "commit": commit, "tree": tree});
    // G284: what lands it once an approver said yes; nothing else asks the change again.
    detail["once_approved"] = json!(
        "a runner's next pass merges this candidate; without a runner, the author's bay merge --wait merges it"
    );
    if registry.is_none() {
        detail["why"] = json!(format!(
            "this repository has no key registry ({REGISTRY_REF}), so no approver key can be verified and an approval check cannot pass; enrol each approver in the registry's approver file"
        ));
    }
    detail
}

/// G245: the approvals the candidate `commit` (tree `tree`, merged onto `base`) still waits
/// on, as `bay status` names them, judged by the policy at `base` like its land; `None` when it
/// waits on none. G292: an approval of one of the change's `earlier` candidates that still
/// holds answers here too, and one set aside is named with the gated paths that changed.
///
/// # Errors
///
/// A git fault, an unreadable policy or note, or a class file that cannot be written.
pub fn pending_approval(
    git: &Git,
    (base, commit, tree): (&str, &str, &str),
    earlier: &[throughput::Earlier],
    registry: Option<&Registry>,
) -> Result<Option<serde_json::Value>, String> {
    let Some(policy) = policy::load_at(git, base).map_err(String::from)? else {
        return Ok(None);
    };
    let changed = check::changed_paths(git, base, commit)?;
    let mut records = evidence::counted(git, evidence::records_for(git, tree)?, registry)?;
    let carried = throughput::carried_approvals(
        git,
        &policy,
        registry,
        (tree, tree_of(git, base)?.as_str()),
        &records,
        earlier,
    )?;
    records.extend(carried.records.iter().cloned());
    let rows = approval_rows(&policy, &changed, registry, (&records, &carried), commit);
    Ok((!rows.is_empty()).then(|| approval_detail(&rows, commit, tree, registry)))
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
                "a runner whose box serves them claims this operation; a box declares each with git config --global --add {} <environment>",
                check::ENVIRONMENT_KEY
            ));
        }
    }
    let mut result = BayResult::refused(Decision::Queued, Reason::CheckUnavailable)
        .with_change(ctx.id.clone())
        .with_op(op_id)
        .with_detail(detail);
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
/// of its manifest the candidate's diff requires (G146). Without a registry `all` holds no
/// approval: [`evidence::counted`] counts none (G245).
fn verdict(
    policy: &policy::Policy,
    required: &[policy::ManifestEntry],
    all: &[evidence::EvidenceRecord],
    named: &[&evidence::EvidenceRecord],
    commit: &str,
) -> Option<(Reason, &'static str, Vec<serde_json::Value>)> {
    // G163: a record from a producer the check's list leaves out never answers it. G245: nor
    // does a record of the other kind; an approval no registry verified was never counted.
    let listed: Vec<evidence::EvidenceRecord> = policy.answering(all.to_vec());
    let approval = |id: &evidence::CheckId| {
        policy
            .check(id)
            .is_some_and(policy::CheckEntry::is_approval)
    };
    // G30: coverage is judged against the frozen manifest, never against counts.
    // A flaky check whose entry says `allow_flaky` is no gap: the landing names it instead.
    let gaps: Vec<(evidence::CheckId, Combined)> = policy
        .gaps(&listed, required, commit)
        .into_iter()
        // G245: approvers who disagree on one tree are a rejection, never a flaky check.
        .map(|(id, combined)| match combined {
            Combined::Flaky if approval(&id) => (id, Combined::ProductFailure),
            _ => (id, combined),
        })
        .collect();
    // No gap is a pass. G245: an approval is asked for only once every other check passed, so
    // a check bay runs that is still unknown is named first.
    let (worst_id, worst) = gaps
        .iter()
        .find(|(_, combined)| *combined == Combined::Flaky)
        .or_else(|| {
            gaps.iter()
                .find(|(_, combined)| *combined == Combined::ProductFailure)
        })
        .or_else(|| gaps.iter().find(|(id, _)| !approval(id)))
        .or_else(|| gaps.first())?;
    let (code, word) = match worst {
        Combined::Flaky => (Reason::CheckFlaky, "flaky"),
        Combined::ProductFailure => (Reason::CheckFailed, "product_failure"),
        Combined::Unknown | Combined::Pass if approval(worst_id) => {
            (Reason::ApprovalPending, "unknown")
        }
        Combined::Unknown | Combined::Pass => (Reason::CheckUnknown, "unknown"),
    };
    let in_gap =
        |record: &evidence::EvidenceRecord| gaps.iter().any(|(id, _)| id.id == record.check.id);
    let mut why: Vec<serde_json::Value> = named
        .iter()
        .filter(|record| in_gap(record) && policy.listed(record))
        .map(|record| gap_row(record))
        .collect();
    // G245: each approver who said no to this tree, by name.
    why.extend(
        listed
            .iter()
            .filter(|record| {
                record.kind == evidence::KIND_APPROVAL
                    && record.manifest_digest == policy.manifest_digest
                    && record.verdict == evidence::Verdict::ProductFailure
                    && in_gap(record)
            })
            .map(|record| {
                json!({"check": record.check.id, "verdict": record.verdict,
                    "approver": record.producer.name, "why": record.why})
            }),
    );
    // G163: each record that would have answered a gap but for the check's producers list.
    // G245: or but for its kind: a check record never answers an approval, nor the reverse.
    why.extend(
        all.iter()
            .filter(|record| {
                evidence::combines(&record.kind)
                    && record.manifest_digest == policy.manifest_digest
                    && in_gap(record)
                    && !policy.listed(record)
            })
            .map(|record| {
                let kind_answers = policy
                    .check(&record.check)
                    .is_none_or(|check| check.kind.record_kind() == record.kind);
                let why = if kind_answers {
                    "unlisted_producer"
                } else {
                    "wrong_kind"
                };
                json!({"check": record.check.id, "producer": record.producer.name,
                    "verdict": record.verdict, "why": why})
            }),
    );
    Some((code, word, why))
}

/// What a refusal says of `record`, a run of a check in a gap: its verdict, why, and the reason
/// of each unit that did not pass. N-3: a unit that failed on the base too is no failure of
/// this candidate, so only the new ones are named under `reasons`; the pre-existing ones are
/// listed apart under `baseline_failures`, with the base record they were compared with.
fn gap_row(record: &evidence::EvidenceRecord) -> serde_json::Value {
    let preexisting = record.baseline_failures();
    let new: Vec<evidence::Unit> = record
        .units
        .iter()
        .filter(|unit| !preexisting.contains(&unit.id))
        .cloned()
        .collect();
    let mut row = json!({"check": record.check.id, "verdict": record.verdict, "why": record.why,
        "reasons": evidence::reasons(&new)});
    if let Some(baseline) = record.unknown.get(evidence::BASELINE) {
        row["baseline"] = baseline.clone();
        row["baseline_failures"] = json!(preexisting);
    }
    row
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
pub(crate) fn landed_markers(
    repo: &Repo,
) -> Result<std::collections::HashMap<String, String>, String> {
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
    if ctx.ahead_of_turn {
        return;
    }
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
            &format!("{beat}:{name}"),
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
    /// The candidate id its records are named by.
    candidate: &'a str,
    tree: &'a str,
    commit: &'a str,
    /// The base commit the candidate is judged on: its policy says what may stand for a run.
    base: &'a str,
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
        ctx.repo.fetch_evidence(&ctx.keys);
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
            continue;
        }
        // NEW-10: a check keyed by its read set carries the record of an earlier candidate of
        // this operation whose read set is the same, byte for byte (`crate::throughput`).
        if subject.is_none() && ctx.repo.config.footprint_lanes() {
            let carried = carry_over(ctx, op_id, policy, judged, check)?;
            if answered(&carried, &id, digest, None) {
                reused.extend(carried);
                finished.push(id);
            }
        }
    }
    Ok((finished, reused))
}

/// NEW-10 footprint-keyed evidence (`crate::throughput`): the records of check `entry`, which
/// declares `footprint = true`, on this operation's earlier candidates and pre-checks whose
/// stamp is the candidate's (the same read set, byte for byte, in its tree and in its base's),
/// under the same policy revision, provenance, manifest and fixtures. Each is carried onto the
/// candidate's tree as a record of this executor's producer that names the record it carries,
/// so evidence still names exactly the tree it covers, and the hook judges it as any other.
/// Returns the carried records; none for a check without `footprint = true`.
fn carry_over(
    ctx: &Ctx,
    op_id: &str,
    policy: &policy::Policy,
    judged: &Judged,
    entry: &policy::CheckEntry,
) -> Result<Vec<evidence::EvidenceRecord>, String> {
    let git = &ctx.repo.git;
    let op = ctx.op(op_id);
    let trees: BTreeSet<&str> = op
        .attempts
        .iter()
        .map(|attempt| attempt.tree.as_str())
        .chain(op.prechecks.iter().map(|precheck| precheck.tree.as_str()))
        .filter(|tree| *tree != judged.tree)
        .collect();
    if trees.is_empty() {
        return Ok(Vec::new());
    }
    let Some(stamp) = throughput::stamp(git, entry, judged.tree, judged.base_tree) else {
        return Ok(Vec::new());
    };
    let id = evidence::CheckId::new(&entry.id, &entry.environment);
    let index = policy
        .checks
        .iter()
        .position(|check| check.id == entry.id && check.environment == entry.environment)
        .map_or(0, |at| at + 1);
    let holds = |record: &&evidence::EvidenceRecord| {
        record.kind == evidence::KIND_CHECK
            && record.check == id
            && record.manifest_digest == policy.manifest_digest
            && record.policy_revision == policy.revision
            && record.policy_provenance.as_deref() == Some(judged.provenance.as_str())
            && record.fixtures().as_deref() == Some(entry.fixtures.as_slice())
            && throughput::same_stamp(record, &stamp)
    };
    let onto = throughput::Onto {
        tree: judged.tree,
        commit: judged.commit,
        base_tree: judged.base_tree,
    };
    let (producer, signer) = ctx.producer()?;
    let mut carried = Vec::new();
    for tree in trees {
        let records = policy.answering(evidence::counted(
            git,
            evidence::records_for(git, tree)?,
            ctx.registry.as_ref(),
        )?);
        for record in records.iter().filter(holds) {
            let name = format!(
                "{producer}/{}-carried-{index}-{}",
                judged.candidate,
                carried.len() + 1
            );
            let mut next = throughput::carry(record, &producer, name, &onto, &stamp);
            // G163: a producer the check's list leaves out never answers it, carried or not.
            if !policy.listed(&next) {
                continue;
            }
            if let Some(signer) = &signer {
                next.sign(&signer.key)?;
            }
            evidence::append(git, &producer, &next)?;
            carried.push(next);
        }
    }
    Ok(carried)
}

/// Whether evidence reuse applies to this judgement (G7): `[policy] evidence_reuse =
/// "exact-inputs"`, a record that may stand for a run on the judgement's base
/// ([`Ctx::reuse_trusted`]: a producer registry that signs everything, or `[policy]
/// reuse_unsigned = true` at the base where evidence counts unsigned, G318), and a request
/// without named inputs. A record does not hold the request's input versions, so it cannot
/// show they matched: a change with named inputs is always rechecked.
fn reuses(ctx: &Ctx, judged: &Judged) -> bool {
    ctx.repo.config.evidence_reuse() && ctx.reuse_trusted(judged.base) && judged.inputs.is_empty()
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

/// Main's commit message for a candidate of `revision`: bay's subject and trailers, with the
/// agent's own words between them and its own trailers after bay's (BACKLOG 58,
/// [`crate::change::agent_message`]).
fn candidate_message(
    ctx: &Ctx,
    revision: &str,
    request_id: &str,
    candidate_id: &str,
    registry_id: &str,
) -> String {
    let message = bay_message(ctx, revision, request_id, candidate_id, registry_id);
    let config = &ctx.repo.config;
    let reserved = [config.intent_trailer(), config.actor_trailer()];
    let Ok(Some((text, trailers))) =
        crate::change::agent_message(&ctx.repo.git, revision, &reserved)
    else {
        return message;
    };
    let Some((subject, bay_trailers)) = message.split_once("\n\n") else {
        return message;
    };
    let mut out = format!("{subject}\n\n");
    if !text.is_empty() {
        out.push_str(&text);
        out.push_str("\n\n");
    }
    out.push_str(bay_trailers);
    for trailer in trailers {
        out.push_str(&trailer);
        out.push('\n');
    }
    out
}

/// bay's own subject and trailers of a candidate of `revision`.
fn bay_message(
    ctx: &Ctx,
    revision: &str,
    request_id: &str,
    candidate_id: &str,
    registry_id: &str,
) -> String {
    let config = &ctx.repo.config;
    let record = &ctx.record;
    format!(
        "{reference}: change {id}\n\nChange-Id: {id}\nRevision-Id: {revision}\nRequest-Id: {request_id}\nCandidate-Id: {candidate_id}\nRegistry-Id: {registry_id}\nBay-Epoch: {epoch}\n{intent_key}: {reference}@{intent_revision}\n{actor_key}: {actor}\nBay-Session: {session}\n{supersedes}{reverts}{fixes}{after}",
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
        after = crate::after::trailers(&record.after),
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
                    "producer {producer} collides with another runner: the remote's {notes} holds records this runner never wrote; give each runner its own bay.producerName"
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
            "the remote does not hold the evidence for {notes}; nothing was merged"
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
    let done_lease =
        release.map(|(seq, _)| format!("--force-with-lease={}{seq}:", crate::queue::DONE_PREFIX));
    let mut args = vec!["push", "-q", "--atomic", landed_lease.as_str()];
    for lease in [&intent_lease, &done_lease].into_iter().flatten() {
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
/// is landed, or another revision landed (`CHANGE_ALREADY_MERGED`).
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
    // A batch member names the batch whose tip was checked: its own tree was not checked alone.
    let batch = ctx
        .op(op_id)
        .attempts
        .iter()
        .rev()
        .find(|attempt| attempt.commit == commit)
        .and_then(|attempt| attempt.batch.clone());
    if let (Some(detail), Some(batch)) = (result.detail.as_mut(), batch) {
        detail["batch"] = json!(batch);
    }
    // G292: each approval that answered from an earlier candidate is named with its record.
    if let Some(detail) = result.detail.as_mut() {
        match approvals_carried(ctx, op_id) {
            Ok(carried) if carried.is_empty() => {}
            Ok(carried) => detail["carried_approvals"] = json!(carried),
            Err(why) => detail["carried_approvals_unread"] = json!(why),
        }
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

/// G292: the approvals the landed candidate of `op_id` stands on that an approver gave an
/// earlier candidate of the change, judged by the policy at its base as its land was: each
/// names its check and `carried_from`, the record that answers. None for a batch member.
fn approvals_carried(ctx: &Ctx, op_id: &str) -> Result<Vec<serde_json::Value>, String> {
    let Some(attempt) = ctx
        .op(op_id)
        .attempts
        .last()
        .filter(|attempt| attempt.batch.is_none())
    else {
        return Ok(Vec::new());
    };
    let git = &ctx.repo.git;
    let Ok(Some(policy)) = policy::load_at(git, &attempt.base) else {
        return Ok(Vec::new());
    };
    let own = evidence::counted(
        git,
        evidence::records_for(git, &attempt.tree)?,
        ctx.registry.as_ref(),
    )?;
    let carried = throughput::carried_approvals(
        git,
        &policy,
        ctx.registry.as_ref(),
        (attempt.tree.as_str(), tree_of(git, &attempt.base)?.as_str()),
        &own,
        &throughput::candidates_of(&ctx.record),
    )?;
    Ok(carried.named())
}

/// G30: the checks the landing passed without testing code, each a coverage gap: a check
/// that passed on a `declared_empty` selection under a policy that judged it (§ 7), a check
/// whose runs on the tree were `flaky` and whose entry says `allow_flaky`, with every run, a
/// check with `baseline = "base"` that passed with units failing on the base too (N-3), with
/// those units, and each quarantined check that no policy judging the landing required
/// (ruling 8c).
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
    // A batch member's evidence is about the batch tip's tree (`crate::throughput`).
    let tree = attempt
        .batch
        .as_ref()
        .map_or(&attempt.tree, |batch| &batch.tree);
    let records = evidence::counted(
        git,
        evidence::records_for(git, tree)?,
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
    // N-3: a check judged against its base passed with units that failed on both: each is a
    // pre-existing failure, named with the base record it was compared with, never a pass.
    for gap in judged
        .iter()
        .flat_map(|policy| baseline_gaps(policy, &records, &attempt.commit))
    {
        if !gaps.contains(&gap) {
            gaps.push(gap);
        }
    }
    // A flaky check that landed under `allow_flaky` is named with every run, never as a pass.
    for gap in judged
        .iter()
        .flat_map(|policy| flaky_gaps(policy, &records, &attempt.commit))
    {
        if !gaps.contains(&gap) {
            gaps.push(gap);
        }
    }
    // N-9: a waivable check that landed on a person's waiver is named with the waiver and its
    // real verdict, never as a pass.
    let changed = check::changed_paths(git, &attempt.base, &attempt.commit)?;
    for gap in judged
        .iter()
        .flat_map(|policy| waiver_gaps(policy, &records, &changed, &attempt.commit))
    {
        if !gaps.contains(&gap) {
            gaps.push(gap);
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
            "quarantine": {"reason": quarantine.reason, "end": quarantine.end,
                "owner": quarantine.owner},
        });
        if !required && !gaps.contains(&gap) {
            gaps.push(gap);
        }
    }
    Ok(gaps)
}

/// N-3: the required checks of `policy` with `baseline = "base"` whose passing counted
/// `records` on the landed tree name units that failed on the candidate and on its base: each
/// with those units (`baseline_failures`) and what each record was compared with (`baseline`).
fn baseline_gaps(
    policy: &policy::Policy,
    records: &[evidence::EvidenceRecord],
    commit: &str,
) -> Vec<serde_json::Value> {
    let listed = policy.answering(records.to_vec());
    let mut gaps = Vec::new();
    for entry in &policy.manifest {
        let id = evidence::CheckId::new(&entry.id, &entry.environment);
        if policy
            .check(&id)
            .is_none_or(|check| check.baseline.is_none())
        {
            continue;
        }
        let subject = policy.commit_keyed(&id).then_some(commit);
        let mut failures: Vec<String> = Vec::new();
        let mut compared: Vec<serde_json::Value> = Vec::new();
        for record in listed.iter().filter(|record| {
            record.kind == evidence::KIND_CHECK
                && record.check == id
                && record.manifest_digest == policy.manifest_digest
                && record.verdict == evidence::Verdict::Pass
                && subject.is_none_or(|commit| record.subject.commit == commit)
        }) {
            let preexisting = record.baseline_failures();
            if preexisting.is_empty() {
                continue;
            }
            for unit in preexisting {
                if !failures.contains(&unit) {
                    failures.push(unit);
                }
            }
            if let Some(baseline) = record.unknown.get(evidence::BASELINE) {
                compared.push(baseline.clone());
            }
        }
        if failures.is_empty() {
            continue;
        }
        failures.sort();
        gaps.push(json!({
            "check": entry.id,
            "environment": entry.environment,
            "baseline_failures": failures,
            "baseline": compared,
        }));
    }
    gaps
}

/// N-9: the checks `policy` requires of a candidate whose diff changes `changed` that its
/// counted `records` do not pass for `commit` and that a valid waiver answers
/// ([`policy::Policy::waiver`]): each with its real verdict and the waiver, `waived_by` (the
/// approver), `why` and `record` (the waiver's id). A waiver is never a pass.
fn waiver_gaps(
    policy: &policy::Policy,
    records: &[evidence::EvidenceRecord],
    changed: &[String],
    commit: &str,
) -> Vec<serde_json::Value> {
    let listed = policy.answering(records.to_vec());
    let (required, _) = policy.required(changed);
    required
        .iter()
        .filter_map(|entry| {
            let id = evidence::CheckId::new(&entry.id, &entry.environment);
            let subject = policy.commit_keyed(&id).then_some(commit);
            let verdict = match evidence::combine(&listed, &id, &policy.manifest_digest, subject) {
                Combined::Pass => return None,
                Combined::ProductFailure => "product_failure",
                Combined::Flaky => "flaky",
                Combined::Unknown => "unknown",
            };
            let waiver = policy.waiver(&listed, &id, commit)?;
            Some(json!({
                "check": entry.id,
                "environment": entry.environment,
                "verdict": verdict,
                "waived_by": waiver.producer.name,
                "why": waiver.why,
                "record": waiver.id,
            }))
        })
        .collect()
}

/// N-9: the waivable checks the policy at `base` requires of candidate `commit` (tree `tree`)
/// that its counted records do not pass and no valid waiver answers yet, judged as its land
/// judges them: each `{"check", "environment", "verdict"}`, the verdict its records combine
/// to. What `bay waiting` lists and `bay approve --waive` answers. None without a valid policy.
///
/// # Errors
///
/// A git fault, an unreadable note, or a class file that cannot be written.
pub fn waivable_gaps(
    git: &Git,
    (base, commit, tree): (&str, &str, &str),
    registry: Option<&Registry>,
) -> Result<Vec<serde_json::Value>, String> {
    let policy = match policy::load_at(git, base) {
        Ok(Some(policy)) if policy.checks.iter().any(|entry| entry.waivable) => policy,
        Ok(_) | Err(policy::LoadError::Invalid(_)) => return Ok(Vec::new()),
        Err(policy::LoadError::Git(why)) => return Err(why),
    };
    let changed = check::changed_paths(git, base, commit)?;
    let (required, _) = policy.required(&changed);
    let records = policy.answering(evidence::counted(
        git,
        evidence::records_for(git, tree)?,
        registry,
    )?);
    Ok(policy
        .gaps(&records, &required, commit)
        .into_iter()
        .filter(|(id, _)| policy.check(id).is_some_and(|entry| entry.waivable))
        .map(|(id, combined)| {
            let verdict = match combined {
                Combined::ProductFailure => "product_failure",
                Combined::Flaky => "flaky",
                Combined::Unknown | Combined::Pass => "unknown",
            };
            json!({"check": id.id, "environment": id.environment, "verdict": verdict})
        })
        .collect())
}

/// The required checks of `policy` whose counted `records` on the landed tree combine to
/// `flaky` and whose entry says `allow_flaky`, each with every run that answered it.
fn flaky_gaps(
    policy: &policy::Policy,
    records: &[evidence::EvidenceRecord],
    commit: &str,
) -> Vec<serde_json::Value> {
    let listed = policy.answering(records.to_vec());
    let mut gaps = Vec::new();
    for entry in &policy.manifest {
        let id = evidence::CheckId::new(&entry.id, &entry.environment);
        let subject = policy.commit_keyed(&id).then_some(commit);
        if !policy.allows_flaky(&id)
            || evidence::combine(&listed, &id, &policy.manifest_digest, subject) != Combined::Flaky
        {
            continue;
        }
        let runs: Vec<serde_json::Value> = listed
            .iter()
            .filter(|record| {
                record.kind == evidence::KIND_CHECK
                    && record.check == id
                    && record.manifest_digest == policy.manifest_digest
                    && subject.is_none_or(|commit| record.subject.commit == commit)
            })
            .map(|record| json!({"id": record.id, "verdict": record.verdict, "at": record.at}))
            .collect();
        gaps.push(json!({
            "check": entry.id,
            "environment": entry.environment,
            "flaky": {"allowed": true, "runs": runs},
        }));
    }
    gaps
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
/// (G244: a later pass on trunk turns it green). Each such row also says whether the diff
/// `required` the check (NEW-6).
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
        let skipped = policy
            .check(&id)
            .and_then(|check| policy.unwoken(check, &changed));
        if recorded {
            // NEW-6: whether this landing requires the check, which no answer may yet say.
            row["required"] = json!(skipped.is_none());
        } else if let Some(skipped) = skipped {
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
        // A batch member's evidence is about the batch tip's tree.
        "evidence_tree": attempt.map(|attempt| {
            attempt
                .batch
                .as_ref()
                .map_or_else(|| attempt.tree.clone(), |batch| batch.tree.clone())
        }),
        "candidate": attempt.map(|attempt| attempt.candidate_id.clone()),
        "executor": executor,
        "policy_repair": ctx.repair,
    });
    if let Some(batch) = attempt.and_then(|attempt| attempt.batch.as_ref()) {
        record["batch"] = json!(batch);
    }
    // ROADMAP § 3: queue wait and check time, from the commit times of the chain's own states.
    if let Some(fields) = record.as_object_mut() {
        fields.extend(crate::timing::of(&ctx.repo.git, &ctx.head, op, outcome));
    }
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

/// The published commit a landed change's retention cut keeps, as [`landed_keeps`] finds it.
fn published_commit(record: &ChangeRecord) -> Option<&str> {
    record
        .operations
        .iter()
        .rev()
        .find(|op| op.state == OpState::Published)?
        .attempts
        .last()
        .map(|attempt| attempt.commit.as_str())
}

/// The committer time of a commit object's text, in Unix seconds.
fn committer_time(commit: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(commit).ok()?;
    let line = text
        .lines()
        .take_while(|line| !line.is_empty())
        .find(|line| line.starts_with("committer "))?;
    line.rsplit(' ').nth(1)?.parse().ok()
}

/// G200b: the ids of `chains` whose retention cut may be due (§ 5), so an executor pass costs
/// what its live work costs, not what its history holds: a chain already cut, one neither
/// landed nor abandoned, and one closed less than the retention window ago are left out
/// before any fetch. An abandoned change is closed since its chain head (`committed`), a
/// landed one since its published commit, whose times one `git cat-file --batch` reads. A
/// time that cannot be read leaves the chain in, and [`retain`] decides it on the chain it
/// fetches again.
#[must_use]
pub fn retention_due(repo: &Repo, chains: &[HeldChain]) -> BTreeSet<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let window = repo.config.retention_window().as_secs();
    let closed: Vec<&HeldChain> = chains
        .iter()
        .filter(|held| !held.record.retention_cut)
        .filter(|held| {
            held.record.state == ChangeState::Abandoned
                || (held.record.state == ChangeState::Landed
                    && published_commit(&held.record).is_some())
        })
        .collect();
    // One time per landed chain, in `closed`'s order, as the filter below takes them.
    let published: Vec<String> = closed
        .iter()
        .filter(|held| held.record.state == ChangeState::Landed)
        .filter_map(|held| published_commit(&held.record).map(str::to_owned))
        .collect();
    let mut times = repo
        .git
        .objects(&published)
        .unwrap_or_else(|_| vec![None; published.len()])
        .into_iter();
    closed
        .into_iter()
        .filter(|held| {
            let since = if held.record.state == ChangeState::Landed {
                times.next().flatten().as_deref().and_then(committer_time)
            } else {
                Some(held.committed).filter(|committed| *committed > 0)
            };
            since.is_none_or(|since| now.saturating_sub(since) >= window)
        })
        .map(|held| held.id.clone())
        .collect()
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
    // A cut the remote never takes leaves this box's chain ref with the context (G294).
    ctx.wrote(head.clone());
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
