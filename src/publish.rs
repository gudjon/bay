//! Publication: freeze → merge-tree → commit-tree → check → persist → push → derive
//! (`DESIGN` § 6).
//!
//! Only the trunk push makes a change landed. Before it, everything is retained in the
//! change chain and the evidence notes; after it, "landed" is derived from trunk. Every
//! attempt first scans `base_first..B` for an earlier publication of the change.
//!
//! `bay merge` does the agent's part and returns at once; `bay runner run` (or `land --wait`)
//! does the rest. After two moves of trunk the operation joins the publication queue (§ 6).

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::canonical::digest;
use crate::change::{
    Attempt, Chain, ChainError, ChangeRecord, ChangeState, HeldChain, KeptPlace, OpState,
    Operation, Precheck, Unavailable, change_ref, freeze_draft,
};
use crate::check::{self, Candidate};
use crate::evidence::{self, Combined, Key};
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

/// The step a publication is at, and when it entered each one: what a runner's journal line
/// says an operation's time went to (a fleet finding, 2026-10-27: merges took five to eight
/// minutes on a loaded box and no record said in which step).
pub struct Step {
    now: Cell<&'static str>,
    entered: RefCell<Vec<(&'static str, std::time::Instant)>>,
    /// G864: the step this thread's network Git commands counted toward before this one began
    /// (`crate::git::at_step`); they count toward it again once this one ends.
    outer: &'static str,
}

impl Step {
    #[must_use]
    pub fn new(first: &'static str) -> Self {
        let outer = crate::git::current_step();
        crate::git::at_step(first);
        Self {
            now: Cell::new(first),
            entered: RefCell::new(vec![(first, std::time::Instant::now())]),
            outer,
        }
    }

    pub fn set(&self, step: &'static str) {
        if self.now.get() != step {
            crate::git::at_step(step);
            self.now.set(step);
            self.entered
                .borrow_mut()
                .push((step, std::time::Instant::now()));
        }
    }

    #[must_use]
    pub fn get(&self) -> &'static str {
        self.now.get()
    }

    /// The whole seconds spent in each step, in the order first entered, each step once; steps
    /// under a second are left out.
    #[must_use]
    pub fn seconds(&self) -> Vec<(&'static str, u64)> {
        let entered = self.entered.borrow();
        let mut out: Vec<(&'static str, std::time::Duration)> = Vec::new();
        for (index, (step, at)) in entered.iter().enumerate() {
            let until = entered
                .get(index + 1)
                .map_or_else(std::time::Instant::now, |(_, next)| *next);
            let spent = until.duration_since(*at);
            match out.iter_mut().find(|(seen, _)| seen == step) {
                Some((_, total)) => *total += spent,
                None => out.push((step, spent)),
            }
        }
        out.into_iter()
            .filter(|(_, spent)| spent.as_secs() >= 1)
            .map(|(step, spent)| (step, spent.as_secs()))
            .collect()
    }
}

impl Drop for Step {
    fn drop(&mut self) {
        crate::git::at_step(self.outer);
    }
}

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
use crate::version::TEST_HOOKS;

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

/// G734: the reads of the remote a merge of `change` makes before it writes, started side by
/// side: the listing of chains and requests the receipts' sync reads, this actor's talks, the
/// change's chain and the registry. Each is taken by the call that makes it, in its place.
fn read_ahead(repo: &Repo, change: Option<&str>) {
    let remote = repo.remote().to_owned();
    let fetch = |refspec: String| -> Vec<String> {
        vec![
            "fetch".to_owned(),
            "-q".to_owned(),
            "--no-tags".to_owned(),
            remote.clone(),
            refspec,
        ]
    };
    let talks = format!(
        "{}{}/*",
        crate::talk::TALK_PREFIX,
        crate::change::actor_name()
    );
    let mut calls = vec![
        vec![
            "ls-remote".to_owned(),
            remote.clone(),
            format!("{}*", crate::change::CHANGES_PREFIX),
            format!("{}*", crate::names::REQUESTS),
        ],
        fetch(format!("{talks}:{talks}")),
        fetch(Repo::registry_refspec()),
    ];
    // The chain is read as `Repo::remote_chain` reads it, so the chain read takes this one.
    if let Some(change) = change {
        calls.push(repo.chain_fetch(change));
    }
    repo.git.read_ahead(&calls);
}

/// G754: what the follow-up of a revision submitted while an earlier one of the change landed
/// with its batch carries ([`follow_up`]): the newer `revision`, the intent, and the paths it
/// changes since its merge base with `trunk`. `None` unless this change's own operation of
/// that revision exists and is not on main, the change was neither abandoned nor taken over,
/// and the revision's tree differs from the landed one's (a revision that adds nothing is
/// answered as before).
fn carry(
    ctx: &Ctx,
    trunk: &str,
    revision: &str,
    landed_revision: &str,
) -> Result<Option<serde_json::Value>, String> {
    if matches!(
        ctx.record.state,
        ChangeState::Abandoned | ChangeState::Superseded
    ) {
        return Ok(None);
    }
    let git = &ctx.repo.git;
    let tree = |rev: &str| {
        git.run(&["rev-parse", &format!("{rev}^{{tree}}")])
            .map_err(|error| error.message())
    };
    let ours = tree(revision)?;
    if ours == tree(landed_revision)? {
        return Ok(None);
    }
    // The author sent these files as an operation of this change, one not on main: a merge
    // checkpoints its folder again, so the same files may come as another commit.
    let mut sent = false;
    for op in &ctx.record.operations {
        if op.state != OpState::Published && tree(&op.revision).is_ok_and(|theirs| theirs == ours) {
            sent = true;
            break;
        }
    }
    if !sent {
        return Ok(None);
    }
    let base = git
        .run(&["merge-base", trunk, revision])
        .map_err(|error| error.message())?;
    let paths = check::changed_paths(git, &base, revision)?;
    if paths.is_empty() {
        return Ok(None);
    }
    Ok(Some(json!({
        "revision": revision,
        "intent": ctx.record.intent.reference,
        "paths": paths,
    })))
}

/// G754: the author's `bay merge` of a revision submitted while an earlier revision of the
/// change landed with its batch opens the next change of the same intent from that revision,
/// in this process under the author's own key, as `bay save` opens one from a checkout's edits,
/// and merges it: the answer is that change's, naming the change it follows in
/// `detail.follow_up`. `None` when `result` carries nothing ([`carry`]).
fn follow_up(
    dir: &Path,
    id: &str,
    result: &mut BayResult,
    wait: bool,
    observe: Option<Duration>,
) -> Option<BayResult> {
    let carry = result.detail.as_mut()?.as_object_mut()?.remove("carry")?;
    let revision = carry["revision"].as_str()?.to_owned();
    let intent = carry["intent"].as_str()?.to_owned();
    let paths: Vec<String> = carry["paths"]
        .as_array()?
        .iter()
        .filter_map(|path| path.as_str().map(str::to_owned))
        .collect();
    let request = |new: bool| crate::start::StartRequest {
        intent: Some(intent.clone()),
        new,
        intent_revision: "1".to_owned(),
        paths: paths.clone(),
        inputs: Vec::new(),
        resume: None,
        reverts: None,
        fixes: None,
        lane: None,
        from: None,
        after: Vec::new(),
        saved: Some(crate::start::Saved {
            commit: revision.clone(),
            message: format!("{intent}: the edits made after {id} was checked"),
            checkout: format!("{id}'s newer revision"),
        }),
        adopt: false,
    };
    let mut started = crate::start::start(dir, &request(false));
    if started.reason == Some(Reason::ChangeAlreadyLanded) {
        started = crate::start::start(dir, &request(true));
    }
    let (Decision::Ok, Some(next)) = (started.decision, started.change.clone()) else {
        return Some(started);
    };
    let view = started.detail.as_ref()?["view"].as_str()?.to_owned();
    let mut landed = land(Path::new(&view), Some(&next), wait, observe, None, None);
    if let Some(detail) = landed.detail.as_mut().and_then(|d| d.as_object_mut()) {
        detail.insert(
            "follow_up".to_owned(),
            json!({"change": next, "from": id, "revision": revision}),
        );
    } else {
        landed.detail =
            Some(json!({"follow_up": {"change": next, "from": id, "revision": revision}}));
    }
    Some(landed)
}

#[must_use]
pub fn land(
    dir: &Path,
    change: Option<&str>,
    wait: bool,
    observe: Option<Duration>,
    abandon: Option<Option<&str>>,
    expedite: Option<&str>,
) -> BayResult {
    // G776 (a fleet finding, 2026-11-17: `bay merge --wait=90` returned after 151 s, its watch
    // timed from after the fetch, the submit and the start of the run): `--wait=<s>` bounds the
    // whole command, from here.
    let until = observe.map(|timeout| Instant::now() + timeout);
    // In a test build, `BAY_TEST_LAND_DELAY_MS` stands for a slow fetch and submit on a loaded box.
    if crate::version::TEST_HOOKS
        && let Some(ms) = std::env::var("BAY_TEST_LAND_DELAY_MS")
            .ok()
            .and_then(|text| text.parse::<u64>().ok())
    {
        std::thread::sleep(Duration::from_millis(ms));
    }
    let repo = match Repo::open_fetched(dir) {
        Ok(repo) => repo,
        Err(error) => return error.into_result(),
    };
    let id = change.map(str::to_owned).or_else(|| repo.view_change(dir));
    read_ahead(&repo, id.as_deref());
    let _ = receipts::sync(&repo);
    let Some(id) = id else {
        return BayResult::failed("run bay merge in a change's view, or name the change");
    };
    // G56 (ruling T9): the talks on this change, fetched before the landing and named in its
    // result. A talk never blocks a landing.
    let open_talks = crate::talk::open_on(&repo, &id);
    let step = Step::new("read");
    let mut result = Err(CHAIN_MOVED.to_owned());
    for _ in 0..3 {
        adopt_remote_chain(&repo, &id);
        if let Some(why) = abandon {
            result = abandon_change(&repo, &id, &step, why);
            break;
        }
        result = match observe.filter(|_| wait) {
            Some(timeout) => observe_land(
                &repo,
                dir,
                &id,
                &step,
                until.unwrap_or_else(Instant::now),
                timeout,
                expedite,
            ),
            None if wait => wait_land(&repo, &id, &step, expedite),
            None => submit_land(&repo, &id, &step, false, expedite),
        };
        if result.as_ref().is_err_and(|why| why == CHAIN_MOVED) {
            continue;
        }
        break;
    }
    let mut result = result.unwrap_or_else(BayResult::failed);
    // G754: an author's newer revision of a change an earlier revision merged goes in next.
    if abandon.is_none()
        && let Some(next) = follow_up(dir, &id, &mut result, wait, observe)
    {
        return next;
    }
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
fn wait_land(
    repo: &Repo,
    id: &str,
    step: &Step,
    expedite: Option<&str>,
) -> Result<BayResult, String> {
    loop {
        let result = submit_land(repo, id, step, true, expedite)?;
        // G811: this machine handed the operation to the runners: the wait goes on until a
        // runner's outcome, which is reported, never run again here. The run a watching
        // `merge --wait=<s>` started ends at once: its watcher reports ([`observe_land`]).
        if std::env::var_os(OBSERVED_RUN).is_none()
            && result.reason == Some(Reason::CheckUnavailable)
            && result
                .detail
                .as_ref()
                .is_some_and(|detail| detail["machine"].is_string())
            && let Some(op_id) = result.op_id.clone()
        {
            // G816: the watch ends at a queue head's progress deadline, or at once when the
            // repository is gone, and then answers as `merge --wait=<s>` does: the run continues.
            let deadline = repo.config.progress_deadline();
            let watch = Instant::now();
            if !watch_until_done(repo, id, &op_id, watch + deadline) {
                return Ok(still_running(repo, id, op_id, deadline, watch));
            }
            return submit_land(repo, id, step, false, None)
                .map(|result| checked_elsewhere(repo, id, &op_id, result));
        }
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

/// G811: set on the run a watching `merge --wait=<s>` starts ([`observe_land`]): its watcher
/// reports the outcome, so the run never watches the chain itself.
const OBSERVED_RUN: &str = "BAY_OBSERVED_RUN";

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

/// G600: a visit of an operation another process blocked writes nothing and runs nothing: it
/// answers with the operation's own outcome, `unchanged`, as its author's unchanged retry does
/// ([`retry_blocked`]).
fn blocked_elsewhere(id: &str, op: &Operation) -> BayResult {
    let mut result = match op.outcome.as_deref().and_then(Reason::from_code) {
        Some(reason) => BayResult::blocked(reason),
        // A lane change whose checks ran (`crate::lane::CHECKED`).
        None => BayResult::new(Decision::Ok),
    };
    result.state = Some("blocked".to_owned());
    result
        .with_change(id)
        .with_op(&op.op_id)
        .with_detail(json!({"unchanged": true, "condition": op.blocked_on, "owner": op.owner}))
}

/// `land --wait` with an observation timeout (§ 9a): the run happens in a detached process,
/// and the caller watches the change chain for at most `timeout`. When the timeout expires the
/// caller returns `queued`/`running` and the run continues on its own.
fn observe_land(
    repo: &Repo,
    dir: &Path,
    id: &str,
    step: &Step,
    until: Instant,
    timeout: Duration,
    expedite: Option<&str>,
) -> Result<BayResult, String> {
    let submitted = submit_land(repo, id, step, false, expedite)?;
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
        .env(OBSERVED_RUN, "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // On Windows the run is a detached process, so it outlives a caller that closes its console.
    crate::platform::detach_windows(&mut run);
    crate::platform::reap(
        run.spawn()
            .map_err(|error| format!("the run could not be started: {error}"))?,
    );
    let watch = Instant::now();
    if watch_until_done(repo, id, &op_id, until) {
        // The outcome is in the chain; reading it again returns the same result the run did.
        return submit_land(repo, id, step, false, None)
            .map(|result| checked_elsewhere(repo, id, &op_id, result));
    }
    Ok(still_running(repo, id, op_id, timeout, watch))
}

/// The answer of a wait that ended, after watching since `watch` for at most `timeout`, before
/// operation `op_id` of change `id` had its outcome (§ 9a): `queued`/`running`, and the run
/// continues on its own. G817: an operation that waits for a worker no live runner serves whole
/// is answered `CHECK_UNAVAILABLE`, saying so, never "nothing for you to do".
fn still_running(
    repo: &Repo,
    id: &str,
    op_id: String,
    timeout: Duration,
    watch: Instant,
) -> BayResult {
    let mut detail = json!({
        "observation_timeout_s": timeout.as_secs(),
        // G776: how long this command watched, the rest of its wait spent submitting.
        "watched_s": watch.elapsed().as_secs(),
        "the run continues": true,
    });
    if let Some(environments) = unserved_whole(repo, id, &op_id) {
        detail["environments"] = json!(environments);
        detail["served_together"] = json!(false);
        let mut result = BayResult::refused(Decision::Queued, Reason::CheckUnavailable)
            .with_change(id)
            .with_op(op_id);
        result.waiting_for = Some("worker".to_owned());
        return result.with_detail(detail);
    }
    let mut result = BayResult::new(Decision::Queued)
        .with_change(id)
        .with_op(op_id);
    result.state = Some("running".to_owned());
    result.waiting_for = Some("executor".to_owned());
    result.with_detail(detail)
}

/// G817: the environments operation `op_id` of change `id` waits for a worker for, when no box
/// served them (no adapter answered) and no live runner, on the presences fetched now, serves
/// them all together. A fetch that fails says nothing.
fn unserved_whole(repo: &Repo, id: &str, op_id: &str) -> Option<Vec<String>> {
    let (_, record) = Chain::new(&repo.git).read(id).ok()?;
    let waiting = record
        .operations
        .iter()
        .find(|op| op.op_id == op_id)?
        .unavailable
        .clone()?;
    if waiting.adapter.is_some()
        || !crate::presence::fetch(repo)
        || crate::presence::serving_until(&repo.git, "", &waiting.environments).is_some()
    {
        return None;
    }
    Some(waiting.environments)
}

/// Watches change `id`'s chain until operation `op_id` reached its outcome (or was blocked), at
/// most `until`: whether it did. G752 (a fleet finding, 2026-11-16: a `merge --wait=7200` ran
/// 61,303 Git commands, a `rev-parse` and a `cat-file` five times a second, and nineteen such
/// waits ended in fifteen minutes on one box at load 70): the chain's head is read once a second,
/// and its record only when the head moved.
///
/// G811: once the operation names this machine as one that handed it to the runners, no process
/// here writes its chain again: the chain is read from the remote, once a heartbeat (G749).
///
/// G816: a repository deleted under the watch can show no outcome: the watch ends at once.
///
/// G883 (a fleet finding, 2026-11-24: a `merge --wait=3600` still watched six minutes after a
/// runner on another machine merged its change, because nothing here fetched its chain): an
/// operation this clone's own run does not hold, one that waits in the line or one a runner
/// claimed, is also read from the remote once a heartbeat. A runner writes the chain in its own
/// clone. The local read stays once a second, so an own run that claims it is seen at once.
fn watch_until_done(repo: &Repo, id: &str, op_id: &str, until: Instant) -> bool {
    let chain = Chain::new(&repo.git);
    let machine = crate::live::machine(&repo.git).ok();
    let own = Keys::load(&repo.git).executor_name;
    let heartbeat = repo.config.heartbeat().max(Duration::from_secs(1));
    let common = repo.git.common_dir().ok().map(std::path::PathBuf::from);
    let mut seen: Option<String> = None;
    let mut handed = false;
    let mut elsewhere = false;
    let mut fetched = Instant::now();
    loop {
        let every = if handed {
            heartbeat
        } else {
            Duration::from_secs(1)
        };
        let Some(left) = until.checked_duration_since(Instant::now()) else {
            return false;
        };
        std::thread::sleep(every.min(left));
        if !repo.git.dir().is_dir() || common.as_ref().is_some_and(|dir| !dir.is_dir()) {
            return false;
        }
        if handed || (elsewhere && fetched.elapsed() >= heartbeat) {
            let _ = repo.remote_chain(id);
            fetched = Instant::now();
        }
        let Ok(Some(head)) = chain.head(id) else {
            continue;
        };
        if seen.as_deref() == Some(head.as_str()) {
            continue;
        }
        let Ok(record) = chain.record_at(&head) else {
            continue;
        };
        seen = Some(head);
        let Some(op) = record.operations.iter().find(|op| op.op_id == op_id) else {
            continue;
        };
        if op.state.is_terminal() || op.state == OpState::Blocked {
            return true;
        }
        handed = machine
            .as_ref()
            .is_some_and(|machine| op.unable.contains(machine));
        elsewhere = op.owner.as_deref() != Some(own.as_str());
    }
}

/// G811: `result`, the outcome of operation `op_id` of change `id`, naming
/// `detail.checked_elsewhere` when this machine handed the operation to the runners because its
/// check gave no answer here: a runner checked it.
fn checked_elsewhere(repo: &Repo, id: &str, op_id: &str, mut result: BayResult) -> BayResult {
    let Ok(machine) = crate::live::machine(&repo.git) else {
        return result;
    };
    let handed = Chain::new(&repo.git).read(id).is_ok_and(|(_, record)| {
        record
            .operations
            .iter()
            .any(|op| op.op_id == op_id && op.unable.contains(&machine))
    });
    if handed {
        let detail = result.detail.get_or_insert_with(|| json!({}));
        if let Some(detail) = detail.as_object_mut() {
            detail.insert("checked_elsewhere".to_owned(), json!({"machine": machine}));
        }
    }
    result
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
    let log = trunk_log(git, &[range.as_str()])?;
    let signatures = git
        .ref_target(REGISTRY_REF)
        .map_err(|error| error.message())?
        .is_some();
    Ok(scan_of(git, &log, signatures, id))
}

/// [`scan`] of many changes, `(base_first, id)` each, from one `git log` of trunk since the
/// oldest of their bases: a change's publication names it in its trailer, so one pass over the
/// union of their ranges answers every one (a fleet finding, 2026-10-26: `bay status` ran one
/// log per live change, about 140 on a fleet's queue).
///
/// # Errors
///
/// A git fault.
pub fn scan_many(
    git: &Git,
    trunk: &str,
    changes: &[(&str, &str)],
) -> Result<std::collections::HashMap<String, Scan>, String> {
    let mut out = std::collections::HashMap::new();
    if changes.is_empty() {
        return Ok(out);
    }
    let mut bases: Vec<&str> = changes.iter().map(|(base, _)| *base).collect();
    bases.sort_unstable();
    bases.dedup();
    let mut args = vec!["merge-base", "--octopus"];
    args.extend(bases.iter().copied());
    // One base needs no join. A base this store cannot join (not here, or on no common line)
    // falls back to one scan per change, as before.
    let joined = match bases.as_slice() {
        [only] => Ok((*only).to_owned()),
        _ => git.run(&args),
    };
    let Ok(oldest) = joined else {
        for (base, id) in changes {
            out.insert((*id).to_owned(), scan(git, base, trunk, id)?);
        }
        return Ok(out);
    };
    let range = format!("{oldest}..{trunk}");
    let log = trunk_log(git, &[range.as_str()])?;
    let signatures = git
        .ref_target(REGISTRY_REF)
        .map_err(|error| error.message())?
        .is_some();
    for (_, id) in changes {
        out.insert((*id).to_owned(), scan_of(git, &log, signatures, id));
    }
    Ok(out)
}

/// Trunk's commits in `range`, each as `scan` reads it.
fn trunk_log(git: &Git, range: &[&str]) -> Result<String, String> {
    let mut args = vec![
        "log",
        "--format=%H%x1f%(trailers:key=Change-Id,valueonly,separator=%x2c)%x1f%(trailers:key=Revision-Id,valueonly,separator=%x2c)%x1f%(trailers:key=Registry-Id,valueonly,separator=%x2c)%x1f%cn%x1e",
    ];
    args.extend(range.iter().copied());
    git.run(&args).map_err(|error| error.message())
}

/// The publications of change `id` in `log` ([`trunk_log`]).
fn scan_of(git: &Git, log: &str, signatures: bool, id: &str) -> Scan {
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
    scan
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
    step: &'a Step,
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
    /// G677: the red post checks this change's paths meet, run on its candidate before the merge,
    /// each with the units it failed on main, which that run asks again (G815).
    red_checks: Vec<(evidence::CheckId, Vec<String>)>,
    /// G551, BACKLOG 92: the claim watched another holder's lease before it won, so main may
    /// have moved since this visit read it.
    claim_waited: bool,
    /// G754: this context holds a member of a batch in its check (the head too): an operation
    /// of it its author revised meanwhile still lands its checked revision ([`revised`]).
    membership: Membership,
    /// G761: whether this context's chain pushes wait to go in one atomic push with the
    /// batch's other members ([`push_owed`]).
    push_mode: PushMode,
    /// BACKLOG 92: the producer whose evidence notes go in the chain's next push, atomically, so
    /// the remote holds them by the time the chain names the candidate checked.
    evidence_rides: Option<String>,
    /// G860: the executor whose log holds a terminal record that waits for the batch's one push
    /// ([`push_owed`]).
    owed_log: Option<String>,
    /// G897: the chain ref's value under this process's head while records ride toward a push
    /// that carries them ([`Ctx::save_riding`]): a checked candidate's record waits for its
    /// landing push. `None` when the chain ref is at this process's head, or another process
    /// moved it.
    riding_from: Option<String>,
    /// The head this process's first write not yet on the remote went on; `None` once every
    /// write it made is pushed or withdrawn. [`Ctx::take_back`] takes those writes off the
    /// local chain ref, and only those, at every re-read of the remote chain and when the
    /// context ends, whichever path wrote them: claims and state saves alike (G294). Any other
    /// local work the remote lacks is kept (G289).
    unpushed_from: Option<String>,
    /// The epoch this process claimed, when it acts as the executor.
    my_epoch: u64,
    /// The registry principal that signed the request, when it is a policy repair (§ 7).
    repair: Option<String>,
    /// Whether the remote answered this run's fetch.
    online: bool,
    /// G787: the lease this process beats while it holds the operation it claimed, released
    /// when the context ends ([`heartbeat`], `crate::lease`).
    lease: std::cell::RefCell<Option<crate::lease::Holding>>,
    /// B2: the last valid policy this operation was judged by, because main's own is invalid;
    /// every result of the operation names it.
    policy_used: std::cell::RefCell<Option<serde_json::Value>>,
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
    /// BACKLOG 99: the commit this process's own publishing push just landed, releasing the
    /// operation's place in that push, so its landing reads nothing a later visit's would need:
    /// no other runner can hold a terminal record of it, and its place is already released.
    pushed_landing: Option<String>,
    /// G284: the operation this process re-armed from `APPROVAL_PENDING` because the evidence
    /// on its candidate, or trunk, changed ([`retry_blocked`]): that candidate is checked
    /// again, never rebuilt, while trunk stands where it was merged onto
    /// ([`approved_candidate`]).
    approved: Option<String>,
    /// Who makes this visit, which says how far it goes.
    visit: Visit,
    /// G796: the move of main that stopped this visit's last check ([`run_policy`]), until
    /// [`after_stopped_check`] builds again on it.
    stopped: std::cell::RefCell<Option<Stopped>>,
    /// G796: the trees of this visit's candidates whose checks a move of main stopped: a check
    /// that finished there carries onto a later candidate ([`carry_over`]).
    stopped_trees: Vec<String>,
    /// G861: this visit's last check ahead of its turn gave way to a head nobody runs
    /// ([`HeadWatch`]): it is no verdict, and nothing of it is recorded.
    yielded: std::cell::Cell<bool>,
    /// G867: this visit's queue gate found a place ahead that another runner holds, whose lease
    /// names no batch yet ([`batch_behind`]): its wait ends at that runner's next beat.
    batch_unnamed: std::cell::Cell<bool>,
    /// G869: the changes this operation lands after that its dependency gate found on trunk with
    /// no landed marker on the remote, each with its publication ([`dependency_waiting`]): their
    /// markers ride the push that lands it ([`Riding::unmarked`]).
    unmarked: std::cell::RefCell<Vec<(String, String)>>,
}

/// G796: main moved past the base of a candidate while its checks ran.
struct Stopped {
    /// Main's tip once it moved.
    main: String,
    /// The candidate's tree.
    tree: String,
}

/// Who makes a visit of an operation ([`Ctx::visit`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Visit {
    /// A `merge --wait`: it runs its own operation to its outcome and never yields.
    Own,
    /// F11: a runner's pass over its queue ([`execute`]): a head refused by a moved trunk gives
    /// the pass back instead of rebuilding in place ([`yield_head`]).
    Pass,
    /// S1: a runner places the operation beside other work ([`place`]): the visit ends at the
    /// queue gate, and never claims, checks or publishes.
    Place,
}

impl<'a> Ctx<'a> {
    /// The ids of the red checks this change's candidate runs before its merge (G677).
    fn red_ids(&self) -> Vec<String> {
        self.red_checks
            .iter()
            .map(|(id, _)| id.id.clone())
            .collect()
    }

    fn load(repo: &'a Repo, step: &'a Step, id: &str) -> Result<Self, String> {
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
            red_checks: Vec::new(),
            claim_waited: false,
            membership: Membership::Alone,
            push_mode: PushMode::Now,
            evidence_rides: None,
            owed_log: None,
            riding_from: None,
            unpushed_from: None,
            my_epoch: 0,
            repair: None,
            online: true,
            lease: std::cell::RefCell::new(None),
            policy_used: std::cell::RefCell::new(None),
            deferred: std::cell::RefCell::new(Vec::new()),
            retries_here: 0,
            ahead_of_turn: false,
            pushed_landing: None,
            approved: None,
            visit: Visit::Own,
            stopped: std::cell::RefCell::new(None),
            stopped_trees: Vec::new(),
            yielded: std::cell::Cell::new(false),
            batch_unnamed: std::cell::Cell::new(false),
            unmarked: std::cell::RefCell::new(Vec::new()),
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
        // G438: in a runner's pass, what the pass read within the reuse window stands in.
        let trunk = self.repo.read_trunk().map_err(|error| error.message())?;
        self.repo.read_registry();
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
        if !self.flush_riding()? {
            return Err(RIDING_UNPUSHED.to_owned());
        }
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
        if !self.flush_riding()? {
            return Ok(false);
        }
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

    /// G875 (a fleet finding, 2026-11-23: a landing's record, on this box's chain ref before its
    /// push, was pushed by another bay process's sync of offline work; the landing's own push was
    /// then refused on a main that moved, and the remote kept a landing main never held):
    /// [`Ctx::save_cas`] for a record that stands only with the push that carries it. The state
    /// commit is made on this process's head, and the chain ref stays where it is until that push
    /// takes it ([`Ctx::catch_up_local`]), so no other process on this box can push it. `false`
    /// when the chain ref is no longer that head, and nothing was made.
    fn save_riding(
        &mut self,
        draft: &str,
        candidate: Option<&str>,
        message: &str,
    ) -> Result<bool, String> {
        let name = change_ref(&self.id);
        let local = self
            .repo
            .git
            .ref_target(&name)
            .map_err(|error| error.message())?;
        // G897: a record may already ride on this head, which the chain ref is then under.
        let expected = self.riding_from.as_deref().unwrap_or(self.head.as_str());
        if local.as_deref() != Some(expected) {
            return Ok(false);
        }
        let head = self
            .chain
            .commit_on(&self.head, &self.record, draft, candidate, message)
            .map_err(|error| error.message())?;
        if self.riding_from.is_none() {
            self.riding_from = local;
        }
        self.wrote(head);
        Ok(true)
    }

    /// G897: the records riding toward a push that carries them ([`Ctx::save_riding`]) go to the
    /// remote now, with the evidence their checked record names, before any write that is not
    /// theirs goes on them and before any read of the remote chain: a refused landing push leaves
    /// the candidate checked on the remote, as its own push did before. `true` when nothing rides
    /// or the remote took it; otherwise the records are taken back ([`Ctx::take_back`]).
    ///
    /// # Errors
    ///
    /// The evidence cannot be pushed ([`push_evidence`]), as a persist's own push failed before:
    /// the candidate never stands checked without it.
    fn flush_riding(&mut self) -> Result<bool, String> {
        if self.riding_from.is_none() {
            return Ok(true);
        }
        match self.push_chain_with_evidence() {
            Ok(true) => Ok(true),
            pushed => {
                self.take_back();
                pushed
            }
        }
    }

    /// The remote took this process's head: later pushes are leased on it, no write of this
    /// process is unpushed, and the chain ref catches up.
    fn took(&mut self) {
        self.pushed_head = Some(self.head.clone());
        self.unpushed_from = None;
        self.riding_from = None;
        self.catch_up_local();
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
        // G761: a batch member's push waits for the batch's one atomic push.
        if self.push_mode != PushMode::Now {
            self.push_mode = PushMode::Owed;
            return true;
        }
        // G897: a chain whose checked record has not yet carried its evidence carries it now.
        if self.evidence_rides.is_some() {
            return self.push_chain_with_evidence().unwrap_or(false);
        }
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
            self.took();
        }
        pushed
    }

    /// [`Ctx::push_chain`], carrying the evidence notes of [`Ctx::evidence_rides`] in the same
    /// atomic push (BACKLOG 92: one round trip, not two). A refused push falls back to the
    /// order it stands for: the notes alone ([`push_evidence`], whose refusal is this
    /// method's error), then the chain.
    ///
    /// # Errors
    ///
    /// The notes cannot be pushed.
    fn push_chain_with_evidence(&mut self) -> Result<bool, String> {
        // G761: the notes ride in the batch's one atomic push too ([`push_owed`]).
        if self.push_mode != PushMode::Now {
            self.push_mode = PushMode::Owed;
            return Ok(true);
        }
        let Some(producer) = self.evidence_rides.take() else {
            return Ok(self.push_chain());
        };
        // #123: no post piece writes the notes between the read of the local ref and the push.
        let writing = evidence::writing();
        let notes = evidence::notes_ref(&producer);
        if self
            .repo
            .git
            .ref_target(&notes)
            .map_err(|error| error.message())?
            .is_some()
        {
            let name = change_ref(&self.id);
            let lease = self
                .pushed_head
                .as_ref()
                .map(|expected| format!("--force-with-lease={name}:{expected}"));
            let mut args = vec!["push", "-q", "--atomic"];
            if let Some(lease) = &lease {
                args.push(lease);
            }
            let chain = format!("{}:{name}", self.head);
            let notes_spec = format!("{notes}:{notes}");
            args.extend([self.repo.remote(), notes_spec.as_str(), chain.as_str()]);
            if self.repo.git.run(&args).is_ok() {
                self.took();
                return Ok(true);
            }
        }
        drop(writing);
        if let Err(why) = push_evidence(self.repo, &producer) {
            // The chain names the candidate checked only once the remote holds its evidence.
            self.evidence_rides = Some(producer);
            return Err(why);
        }
        Ok(self.push_chain())
    }

    /// G789: [`Ctx::push_chain`], carrying this executor's log with `record` written on it in the
    /// same atomic push: a landing's own record and its terminal record take one round trip, not
    /// two. A refused push falls back to the order it stands for: the chain alone, then the log
    /// ([`crate::log::append_riding`]). A batch member's chain waits for its batch's push.
    ///
    /// # Errors
    ///
    /// The record cannot be written on the local log.
    fn push_chain_with_record(&mut self, record: &serde_json::Value) -> Result<(), String> {
        let repo = self.repo;
        let executor = self.executor_name();
        let signer = self.executor_signer().ok().flatten();
        // G860: a batch member's record waits for the batch's one push too ([`push_owed`]).
        if self.push_mode != PushMode::Now {
            self.push_chain();
            crate::log::write_local(repo, &executor, signer.as_ref(), record)?;
            self.owed_log = Some(executor);
            return Ok(());
        }
        crate::log::append_riding(repo, &executor, signer.as_ref(), record, |log| {
            let name = change_ref(&self.id);
            let lease = self
                .pushed_head
                .as_ref()
                .map(|expected| format!("--force-with-lease={name}:{expected}"));
            let mut args = vec!["push", "-q", "--atomic"];
            if let Some(lease) = &lease {
                args.push(lease);
            }
            let chain = format!("{}:{name}", self.head);
            args.extend([repo.remote(), chain.as_str(), log]);
            if repo.git.run(&args).is_ok() {
                self.took();
                return crate::log::Riding::Took;
            }
            // G918 (a fleet census, 2026-11-26: 510 of 11,186 published operations carried two
            // published terminal records; one second record said no claim and no attempt, as a
            // chain read before its lander's claim says): a chain the remote refuses was moved by
            // another process, as a lander's push moves it, and the record is that process's to
            // write. It goes alone only when the chain stands without it. A remote bay cannot read
            // keeps the order: the record waits on this box, never lost to a fault of the network.
            if !self.push_chain()
                && self.remote_tip(&name).is_some_and(|tip| {
                    !self.repo.git.is_ancestor(&self.head, &tip).unwrap_or(false)
                })
            {
                return crate::log::Riding::Void;
            }
            if repo.git.run(&["push", "-q", repo.remote(), log]).is_ok() {
                crate::log::Riding::Took
            } else {
                crate::log::Riding::Apart
            }
        })
        .map(|_| ())
    }

    /// Pushes the chain and moves `refs/bay/requests/<id>` to `request` in one atomic push
    /// (§ 10a). The requests ref always names the latest land request. Like
    /// [`Ctx::push_chain`] it pushes this process's own objects, never the local refs.
    fn push_chain_and_request(&mut self, request: &str) -> Result<(), Option<String>> {
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
        match self.repo.git.run(&args) {
            Ok(_) => {
                self.took();
                Ok(())
            }
            // G388: the remote took this very head from another push of this box's records
            // after it advertised the older tip, so the chain is pushed: the request goes alone.
            Err(_) if self.remote_tip(&name).as_deref() == Some(self.head.as_str()) => {
                let alone = ["push", "-q", self.repo.remote(), request_spec.as_str()];
                match self.repo.git.run(&alone) {
                    Ok(_) => {
                        self.took();
                        Ok(())
                    }
                    Err(error) => Err(self.refused(&name, &error.message())),
                }
            }
            Err(error) => Err(self.refused(&name, &error.message())),
        }
    }

    /// The remote's tip of the chain `name`, when bay can read it.
    fn remote_tip(&self, name: &str) -> Option<String> {
        let listed = self
            .repo
            .git
            .run(&["ls-remote", self.repo.remote(), name])
            .ok()?;
        listed.split_whitespace().next().map(str::to_owned)
    }

    /// Why the push of the chain `name` failed, when the remote's chain did not move (its tip,
    /// if any, is under the head pushed, so the push was a fast-forward): then the remote
    /// refused the push (its hook, a size limit, its rights), and `why` is its first line.
    /// `None` when it moved, or when bay cannot tell: the caller reads it again.
    fn refused(&self, name: &str, why: &str) -> Option<String> {
        let listed = self
            .repo
            .git
            .run(&["ls-remote", self.repo.remote(), name])
            .ok()?;
        // Moved: the remote's tip is not under the head pushed, so that push was no fast-forward.
        if let Some(tip) = listed.split_whitespace().next()
            && !self.repo.git.is_ancestor(tip, &self.head).unwrap_or(false)
        {
            return None;
        }
        let first = why
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("no reason given");
        Some(first.to_owned())
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
        // G897: while records ride, the chain ref is under this process's head and never took
        // them: it goes back only when `before` is under it too.
        let local = self
            .riding_from
            .clone()
            .unwrap_or_else(|| self.head.clone());
        if self.riding_from.is_none() || self.repo.git.is_ancestor(before, &local).unwrap_or(false)
        {
            let _ = self.repo.git.run(&["update-ref", &name, before, &local]);
            self.riding_from = None;
        }
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

    /// Reads the remote's chain ([`Repo::remote_record`]) and goes on from its head. Records riding
    /// toward a landing push go to the remote first ([`Ctx::flush_riding`], G897). This
    /// process's own unpushed writes are taken back first ([`Ctx::take_back`]); the local
    /// chain ref is then never forced (G289): it moves only forward, and keeps local work the
    /// remote lacks.
    fn refetch(&mut self) -> Result<(), String> {
        self.flush_riding()?;
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
        // G787: the holder's visit ends, and its lease with it.
        if let Some(mut lease) = self.lease.get_mut().take() {
            lease.release(self.repo);
        }
    }
}

/// The agent's part of `land` (§ 9a): freeze, scan, and append the request. Returns at once
/// with the operation id; with `wait` it also runs the executor's part inline.
#[allow(clippy::too_many_lines)]
fn submit_land(
    repo: &Repo,
    id: &str,
    step: &Step,
    wait: bool,
    expedite: Option<&str>,
) -> Result<BayResult, String> {
    let mut ctx = Ctx::load(repo, step, id)?;
    // G688: a change that ended with nothing to add (or behind a newer one) is refused in plain
    // words before its view is read: bay removes an ended change's view.
    let ended = match ctx.record.state {
        ChangeState::NoEffect => Some(Reason::ChangeAlreadyLanded),
        ChangeState::Superseded => Some(Reason::ChangeSuperseded),
        _ => None,
    };
    if let Some(reason) = ended {
        return Ok(BayResult::blocked(reason)
            .with_change(id)
            .with_detail(json!({"state": ctx.record.state})));
    }
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
    let me = crate::change::current_actor(&repo.common_dir)?;
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
    // G520: a view is its actor's; another actor's merge freezes nothing of it.
    if let Some(refusal) = view::not_yours(repo, id) {
        return Ok(refusal);
    }
    if let Some(refusal) = actor_unproven(repo, id, &trunk)? {
        return Ok(refusal);
    }
    let agent = ctx.agent_signer()?;
    ctx.chain.sign_with(agent);
    // G824: a release of the view under way in another process ends before the view is read, and
    // none starts until this merge has read it.
    let held = view::hold(repo, id);
    // G820 (a fleet finding, 2026-11-20): bay removes a merged change's view once it ends, and a
    // wait asked again after that read the gone view as a step left half done. With no view to
    // read, the record answers: the change's landing, as for G727.
    if ctx.record.state == ChangeState::Landed
        && !view::journal(repo, id).is_some_and(view::Journal::is_complete)
        && let Some((op_id, revision)) = ctx
            .record
            .operations
            .iter()
            .rev()
            .find(|op| op.state == OpState::Published)
            .map(|op| (op.op_id.clone(), op.revision.clone()))
        && let Some(commit) = published_commit(&ctx.record).map(str::to_owned)
    {
        // G931: a landing main never took opens the change again, and this land goes on.
        if let Some(result) = landed_if_on_main(&mut ctx, &op_id, &revision, &commit)? {
            return Ok(result);
        }
    }

    step.set("freeze");
    stand_on_view_base(&mut ctx, &trunk);
    let revision = match freeze(&ctx)? {
        Ok(revision) => revision,
        Err(refusal) => return Ok(refusal),
    };
    observe_inputs(&mut ctx)?;
    drop(held);

    if let Some(mut result) = scanned(&mut ctx, &trunk, &revision)? {
        // G754: main holds an earlier revision of this change; the author's newer one goes in
        // next ([`follow_up`]).
        let landed = result
            .detail
            .as_ref()
            .and_then(|detail| detail["landed_revision"].as_str())
            .map(str::to_owned);
        if result.reason == Some(Reason::ChangeAlreadyLanded)
            && let Some(landed) = landed
            && let Some(carried) = carry(&ctx, &trunk, &revision, &landed)?
            && let Some(detail) = result.detail.as_mut()
        {
            detail["carry"] = carried;
        }
        return Ok(result);
    }
    // G682 (a fleet finding, 2026-11-09): main read before a landing and the chain read after it
    // let a new revision of a merged change in as an operation no runner answers. The record
    // decides too: a merged operation of another revision ends the change. BACKLOG 238 (a fleet
    // finding, 2026-11-14): one of this revision is the change's landing, whatever main this
    // process read; a main read before the landing reached it held no publication, and the
    // same revision was submitted again (3xq4d7ow.2, 2026-11-12, the change set back to ready).
    let landed_op = ctx
        .record
        .operations
        .iter()
        .find(|op| op.state == OpState::Published)
        .map(|op| (op.op_id.clone(), op.revision.clone()));
    if let Some((op_id, landed_revision)) = landed_op {
        // G931: a landing main never took opens the change again, and this land goes on.
        let reopened = if landed_revision == revision
            && let Some(commit) = published_commit(&ctx.record).map(str::to_owned)
        {
            match landed_if_on_main(&mut ctx, &op_id, &revision, &commit)? {
                Some(result) => return Ok(result),
                None => true,
            }
        } else {
            false
        };
        if !reopened {
            let mut detail = json!({"landed_revision": landed_revision, "op_id": op_id});
            if let Some(carry) = carry(&ctx, &trunk, &revision, &landed_revision)? {
                detail["carry"] = carry;
            }
            return Ok(BayResult::blocked(Reason::ChangeAlreadyLanded)
                .with_change(id)
                .with_detail(detail));
        }
    }
    // NEW-4: a change of no lane whose view reaches a lane's commits off trunk never lands them.
    if ctx.record.lane.is_none()
        && let Some(refusal) = lane_leak(&ctx, &trunk, &revision)?
    {
        return Ok(refusal);
    }
    if let Some(refusal) = markers_left_at_submit(&ctx, &trunk, &revision)? {
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

    // BACKLOG 326: an expedite the line can grant now rides in the request; one it cannot is said
    // in the result, and the change goes in line as a plain arrival.
    let (ask, asked) = expedite_ask(&ctx, expedite)?;
    let op_id = match ctx
        .record
        .operations
        .iter()
        .find(|op| op.revision == revision && !op.state.is_terminal())
    {
        // An expedite asked for an operation in line that no runner holds yet is a new request
        // of the same revision; the runner that grants it releases the older place.
        Some(op)
            if ask.is_none()
                || op.owner.is_some()
                || !matches!(op.state, OpState::Submitted | OpState::Admitted)
                || op
                    .request_id
                    .as_deref()
                    .and_then(|request| crate::expedite::asked(&repo.git, request))
                    .is_some() =>
        {
            op.op_id.clone()
        }
        _ => submit(&mut ctx, &revision, ask)?,
    };
    if ctx.op(&op_id).state == OpState::Blocked
        && let Some(refusal) = retry_blocked(&mut ctx, &op_id, &revision, &trunk)?
    {
        return Ok(refusal);
    }
    // G672, G674: a conflict or an undo is told here, not after a turn in the queue. A runner
    // still decides at its own turn, on main as it is then. Before any input provider runs (G428).
    if ctx.op(&op_id).state == OpState::Submitted {
        let tree = match merged(&mut ctx, &op_id, &revision, &trunk)? {
            Ok(tree) => tree,
            Err(refusal) => return Ok(refusal),
        };
        if let Some(refusal) = undoes_merged(&mut ctx, &op_id, &revision, &trunk, &tree)? {
            return Ok(refusal);
        }
        step.set("freeze");
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
    // BACKLOG 67 item 9: said before any check runs, so the agent reads main's change first.
    let moved = main_changed(&repo.git, &trunk, &revision);
    if wait && online {
        let result = execute_op(&mut ctx, &op_id, trunk.clone());
        ctx.tell_deferred();
        return result.map(|result| with_main_changed(result, moved));
    }
    // G657 (a fleet finding, 2026-11-05: a document reached main 45 s after its request, 40 s of
    // them waiting for a runner): a change no pre-merge check covers leaves a runner nothing to
    // check, so this command pushes it. A machine that may not write as a runner, or any fault,
    // leaves it submitted, for a runner.
    // A test of a runner's visit to such a change asks for it to be left to the runner.
    let left_to_a_runner = TEST_HOOKS && std::env::var_os("BAY_TEST_LEAVE_TO_RUNNER").is_some();
    if online
        && !left_to_a_runner
        && nothing_to_check(&repo.git, &trunk, &revision).unwrap_or(false)
        && let Ok(result) = execute_op(&mut ctx, &op_id, trunk.clone())
    {
        ctx.tell_deferred();
        return Ok(with_main_changed(result, moved));
    }
    let op = ctx.op(&op_id);
    let refused = asked
        .as_ref()
        .is_some_and(|detail| !detail["refused"].is_null());
    let mut result = if refused {
        BayResult::refused(Decision::Queued, Reason::ExpediteRefused)
    } else {
        BayResult::new(Decision::Queued)
    }
    .with_change(id)
    .with_op(op_id.clone());
    result.state = serde_json::to_value(op.state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned));
    result.waiting_for = Some(if online { "executor" } else { "remote" }.to_owned());
    // A fleet finding (2026-10-26): a change 34th in line read "adds it to main next", and its
    // seat took the wait for a refusal. Its place is said, read from the queue it holds.
    if let Some(place) = place_in_line(ctx.repo, &op_id) {
        result.detail = Some(place);
    }
    if let Some(expedite) = asked {
        let detail = result.detail.get_or_insert_with(|| json!({}));
        detail["expedite"] = expedite;
    }
    Ok(with_main_changed(
        with_progress(&ctx.repo.git, ctx.op(&op_id), result),
        moved,
    ))
}

/// BACKLOG 326: what `bay merge --expedite <why>` writes and says. The first is the reason a new
/// request carries: `Some` when the line can grant an expedite now and this change does not
/// head it. The second is `detail.expedite` of a queued result, its `refused` set when the
/// line cannot grant one.
fn expedite_ask<'a>(
    ctx: &Ctx,
    why: Option<&'a str>,
) -> Result<(Option<&'a str>, Option<serde_json::Value>), String> {
    let Some(why) = why else {
        return Ok((None, None));
    };
    let queue = crate::queue::Queue::read(ctx.repo)?;
    if queue.head().is_none_or(|head| head.change == ctx.id) {
        return Ok((None, None));
    }
    let per_day = ctx.repo.config.expedite_per_day();
    let now = crate::live::unix_now();
    let refused = crate::expedite::refusal(per_day, &queue, now);
    let detail = crate::expedite::detail(why, refused, per_day, &queue, now);
    Ok((refused.is_none().then_some(why), Some(detail)))
}

/// `{"place", "in_line"}`: where operation `op_id` stands among the queue's live places, 1 for
/// the head; `None` when it holds no place (no queue is live, or it was not admitted yet).
fn place_in_line(repo: &Repo, op_id: &str) -> Option<serde_json::Value> {
    let queue = crate::queue::Queue::read(repo).ok()?;
    let mine = queue.of(op_id)?.seq;
    Some(json!({"place": queue.position(mine), "in_line": queue.line().len()}))
}

/// BACKLOG 67 item 9 (owner ruling 2026-10-24): the files this change edits that main changed
/// since the change's base (where `revision` meets `trunk`), and main's newest commit that
/// changed them, as `{"paths", "commit"}`; `None` when main changed none of them. The check
/// still runs on the merged result: this only says it first.
fn main_changed(git: &Git, trunk: &str, revision: &str) -> Option<serde_json::Value> {
    let base = git.run(&["merge-base", trunk, revision]).ok()?;
    if base == trunk {
        return None;
    }
    let names = |from: &str, to: &str| -> Vec<String> {
        git.run(&["diff", "--name-only", "--no-renames", "-z", from, to])
            .map(|out| {
                out.split('\0')
                    .filter(|path| !path.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mine = names(&base, revision);
    let theirs = names(&base, trunk);
    let paths: Vec<String> = mine
        .into_iter()
        .filter(|path| theirs.contains(path))
        .collect();
    if paths.is_empty() {
        return None;
    }
    let range = format!("{base}..{trunk}");
    let mut args = vec!["log", "-1", "--format=%H", range.as_str(), "--"];
    args.extend(paths.iter().map(String::as_str));
    let commit = git.run(&args).ok().filter(|commit| !commit.is_empty());
    Some(json!({"paths": paths, "commit": commit}))
}

/// `result` with `moved` ([`main_changed`]) as its `detail.main_changed`, when there is one.
fn with_main_changed(mut result: BayResult, moved: Option<serde_json::Value>) -> BayResult {
    let Some(moved) = moved else {
        return result;
    };
    match result.detail.as_mut() {
        Some(serde_json::Value::Object(detail)) => {
            detail.insert("main_changed".to_owned(), moved);
        }
        None => result.detail = Some(json!({"main_changed": moved})),
        Some(_) => {}
    }
    result
}

/// BACKLOG 90: `MERGE_CONFLICT` at submit when the change's own edits, from where `revision`
/// meets `trunk`, still add a conflict marker a refresh wrote in its view: `<<<<<<< <commit>` or
/// `>>>>>>> <commit>`, the labels `git merge-tree` gives the two sides. A line of any other shape
/// is the change's own text. Nothing is written.
fn markers_left_at_submit(
    ctx: &Ctx,
    trunk: &str,
    revision: &str,
) -> Result<Option<BayResult>, String> {
    let git = &ctx.repo.git;
    let from = git
        .run(&["merge-base", trunk, revision])
        .unwrap_or_else(|_| trunk.to_owned());
    // A content diff reads the blobs both sides changed: in a blob-less store, Git fetches them.
    let diff = git
        .fetching()
        .run(&[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-renames",
            "-U0",
            &from,
            revision,
        ])
        .map_err(|error| error.message())?;
    let paths = marked_paths(&diff);
    if paths.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        BayResult::blocked(Reason::MergeConflict)
            .with_change(ctx.id.clone())
            .with_detail(json!({
                "conflicts": paths,
                "trunk": trunk,
                "next": "the view still holds conflict markers in the listed paths: resolve \
                    them, then bay merge",
            })),
    ))
}

/// The paths a `git diff -U0` adds a `git merge-tree` conflict marker to, in diff order.
fn marked_paths(diff: &str) -> Vec<String> {
    let is_commit = |label: &str| {
        matches!(label.len(), 40 | 64) && label.bytes().all(|byte| byte.is_ascii_hexdigit())
    };
    let mut paths: Vec<String> = Vec::new();
    let mut path: Option<&str> = None;
    let mut header = false;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            (header, path) = (true, None);
        } else if header {
            if line.starts_with("@@") {
                header = false;
            } else if let Some(name) = line.strip_prefix("+++ ") {
                path = name.strip_prefix("b/");
            }
        } else if let Some(name) = path
            && ["+<<<<<<< ", "+>>>>>>> "]
                .iter()
                .any(|open| line.strip_prefix(open).is_some_and(is_commit))
            && !paths.iter().any(|known| known == name)
        {
            paths.push(name.to_owned());
        }
    }
    paths
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

/// The executor's part: runs operation `op_id` of change `id` to a stated outcome. The visit
/// began at `began`: now, or G789 when the runner began to read for its take of the queue's
/// head, so what that take read (the queue, the head's chain) is the visit's own.
///
/// # Errors
///
/// A fault of bay's own; refusals are results.
pub fn execute(
    repo: &Repo,
    id: &str,
    op_id: &str,
    step: &Step,
    began: Option<std::time::Instant>,
) -> Result<BayResult, String> {
    // G438: a visit runs on the pass's shared reads; one whose operation may run reads afresh
    // and starts again, so a head never claims or builds on what an earlier visit read. G694:
    // it reads again only what was read before it began.
    repo.begin_visit(began.unwrap_or_else(std::time::Instant::now));
    kill_point("visit-begins");
    // An approval the first try re-armed is the second's too: the chain no longer says it.
    let mut approved = None;
    match execute_once(repo, id, op_id, step, Visit::Pass, &mut approved) {
        Err(why) if why == READ_AGAIN => {
            repo.read_again();
            execute_once(repo, id, op_id, step, Visit::Pass, &mut approved)
        }
        other => other,
    }
}

/// G438: the sentinel by which a visit on shared reads asks [`execute`] to read afresh.
const READ_AGAIN: &str = "this visit read shared state and may run its operation: read again";

/// S1 (bay 0.6.0): [`execute`]'s visit of operation `op_id` of change `id` as far as the queue
/// gate, made by a runner while its own work runs (`crate::executor`): the operation gets its
/// place in line, or its refusal or its wait, each written as a pass writes it. `None` when the
/// operation may run now (it is the head, or no queue is live): only a pass's own visit claims,
/// checks and publishes.
///
/// # Errors
///
/// A fault of bay's own; refusals are results.
pub fn place(repo: &Repo, id: &str, op_id: &str, step: &Step) -> Result<Option<BayResult>, String> {
    match execute_once(repo, id, op_id, step, Visit::Place, &mut None) {
        Err(why) if why == LEFT_FOR_PASS => Ok(None),
        other => other.map(Some),
    }
}

/// S1: the sentinel by which a visit that only places says its operation may run now.
const LEFT_FOR_PASS: &str = "this operation may run now; a pass runs it";

fn execute_once(
    repo: &Repo,
    id: &str,
    op_id: &str,
    step: &Step,
    visit: Visit,
    approved: &mut Option<String>,
) -> Result<BayResult, String> {
    let mut ctx = Ctx::load(repo, step, id)?;
    ctx.visit = visit;
    ctx.approved = approved.take();
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
    // G799: what the pass already read answers a visit that could do nothing, before it reads
    // main again.
    if let Some(answer) = settled_by_what_the_pass_read(&mut ctx, op_id)? {
        return Ok(answer);
    }
    let trunk = match ctx.fetch_trunk()? {
        Ok(trunk) => trunk,
        Err(refusal) => return Ok(refusal),
    };
    let revision = ctx.op(op_id).revision.clone();
    if let Some(result) = scanned(&mut ctx, &trunk, &revision)? {
        return Ok(existing(result));
    }
    // G567 (a fleet finding, 2026-10-29: grjioybr.1 landed over grjioybr.2): a pass lists its
    // operations when it begins and visits each one later, so it visits one that ended
    // meanwhile. An operation that ended is no one's to place or run: a place in line would
    // make it live again.
    let op = ctx.op(op_id);
    if op.state == OpState::Superseded {
        return Ok(superseded(id, op_id, op.epoch, op.owner.as_ref()));
    }
    if op.state.is_terminal() {
        let visited = BayResult::new(Decision::Ok).with_op(op_id);
        return Ok(ended_elsewhere(repo, id, &visited, Some(op)));
    }
    // G284: an operation waiting for a person's approval is asked again on every pass, as its
    // author's retry asks it: once the evidence on its candidate, or trunk, changed it is
    // re-armed and run below; unchanged, it keeps waiting, with nothing written and no event.
    // G574: so is one a check gave no clear answer ([`asked_again`]).
    if asked_again(ctx.op(op_id)) {
        let executor = ctx.executor_signer()?;
        ctx.chain.sign_with(executor);
        if let Some(waiting) = retry_blocked(&mut ctx, op_id, &revision, &trunk)? {
            return Ok(waiting);
        }
    }
    let result = execute_op(&mut ctx, op_id, trunk);
    // G694: a visit that re-armed an approved operation and then reads again (G438) runs the
    // candidate that was approved, never a new one: the re-arm left no mark on the chain.
    if matches!(&result, Err(why) if why == READ_AGAIN) {
        approved.clone_from(&ctx.approved);
    }
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
    let _ = landed_wake(&ctx, op_id, result.decision);
    Ok(result)
}

/// G799 (a fleet measurement, 2026-11-19: in 28 h one box's two runners spent 5,882 s on 434
/// visits to a head another runner held, and 19,118 s on 1,542 visits to a head that waited for
/// a runner of another environment; each fetched main, judged main's red, and read the chain
/// again before it found what its pass had already read): a runner's visit that this store's
/// reads already answer ends before it reads main. `None` when they do not.
///
/// - The queue's head, waiting or held, whose checks under the policy at this store's main run
///   in an environment this box does not serve while a live runner's does: left to that runner,
///   unclaimed and in its place, as [`served_elsewhere`] leaves it after the queue gate. A change
///   only main's red routes elsewhere is still judged there (G797).
/// - A head another runner holds, on its chain read again as the claim reads it, whose lease
///   this looping runner saw beat within the lease (G696): held, as the claim would find it,
///   with no fetch of main, red judgement or gate.
fn settled_by_what_the_pass_read(ctx: &mut Ctx, op_id: &str) -> Result<Option<BayResult>, String> {
    if ctx.visit == Visit::Own || ctx.record.lane.is_some() || ctx.record.ended() {
        return Ok(None);
    }
    let op = ctx.op(op_id);
    if op.state.is_terminal() || op.state == OpState::Blocked {
        return Ok(None);
    }
    // G800: the line says its place was released `published`: it landed since the pass read its
    // chain, so neither the routing nor the lease below speaks for it any more. A visit that only
    // places (S1) asks too: the line no longer holds a place for it, and it would take another.
    if crate::queue::Queue::read_shared(ctx.repo)
        .is_ok_and(|queue| queue.released_as(op_id, "published"))
    {
        return landed_elsewhere(ctx, op_id);
    }
    if ctx.visit != Visit::Pass {
        return Ok(None);
    }
    let me = ctx.executor_name();
    let others = op.owner.as_deref().is_none_or(|owner| owner != me);
    let held = op.owner.is_some()
        && others
        && matches!(op.state, OpState::Running | OpState::Checked)
        && op.unavailable.is_none();
    let waits = op.owner.is_none();
    // The head this box cannot serve is left as the gates would leave it, held or not.
    if (waits || held)
        && let Some(trunk) = crate::queue::Queue::read_shared(ctx.repo)
            .ok()
            .and_then(|queue| queue.head().map(|head| head.op_id == op_id))
            .filter(|head| *head)
            .and_then(|_| {
                ctx.repo
                    .git
                    .ref_target(&ctx.repo.trunk_tracking_ref())
                    .ok()
                    .flatten()
            })
    {
        let revision = ctx.op(op_id).revision.clone();
        if let Some(left) = served_elsewhere(ctx, op_id, &revision, &trunk)? {
            return Ok(Some(left));
        }
    }
    if !(held && watches_leases_across_passes()) {
        return Ok(None);
    }
    // As the claim does, the lease is judged on the chain as the remote holds it now: the pass
    // read it before the holder went on, and a change's own run holds a lease of a few
    // heartbeats (G679) and records its wait for a worker as soon as it finds none, which no
    // lease then holds against a box that serves it (a gate finding, 2026-11-19: on a stale read
    // a looping runner answered such a wait held, and never took it).
    if ctx.refetch().is_err() {
        return Ok(None);
    }
    // G809: one the chain still says checked, while the line released it `published`, landed:
    // never held.
    match after_release(ctx, op_id)? {
        AfterRelease::Landed(landed) => return Ok(Some(*landed)),
        AfterRelease::Unanswered => return Ok(None),
        AfterRelease::Stands => {}
    }
    // G800: one that landed since is answered from that chain.
    if let Some(landed) = landed_on_chain(ctx, op_id, false)? {
        return Ok(Some(landed));
    }
    let still = ctx.find_op(op_id).is_some_and(|op| {
        op.owner.as_deref().is_some_and(|owner| owner != me)
            && matches!(op.state, OpState::Running | OpState::Checked)
            && op.unavailable.is_none()
    });
    if !still {
        return Ok(None);
    }
    ctx.registry = Registry::current(&ctx.repo.git)?;
    let quiet = holder_lease(&ctx.repo.config, ctx.op(op_id).owner.as_deref());
    if holder_revoked(ctx, op_id)? || lease_expired(ctx, quiet) {
        return Ok(None);
    }
    let op = ctx.op(op_id);
    let mut result = BayResult::new(Decision::Ok)
        .with_change(ctx.id.clone())
        .with_op(op_id);
    result.state = Some("running".to_owned());
    Ok(Some(result.with_detail(json!({
        "lease": "held",
        "owner": op.owner,
        "epoch": op.epoch,
    }))))
}

/// G809: what the line says of an operation whose chain, as this visit just read it, says checked.
enum AfterRelease {
    /// The chain does not say checked, or the line has not released the place `published`.
    Stands,
    /// It landed: the landing row, as a scan of main finds it.
    Landed(Box<BayResult>),
    /// It landed, and nothing this visit read answers it yet.
    Unanswered,
}

/// G809 (a finding, 2026-11-20: a landing pushes main and the line's `published` release first,
/// and its chain's record after; a runner that read the chain between the two answered the landed
/// head held, a lease long when the record's push failed): when `op_id`'s chain says checked, the
/// line, read again in that window alone. Released `published`, it landed: answered from the
/// chain ([`landed_on_chain`]), with main fetched once when the landing commit is not in this
/// store yet, else by a scan of that main.
fn after_release(ctx: &mut Ctx, op_id: &str) -> Result<AfterRelease, String> {
    let checked = ctx
        .find_op(op_id)
        .is_some_and(|op| op.state == OpState::Checked);
    if !checked
        || !crate::queue::Queue::read(ctx.repo)
            .is_ok_and(|queue| queue.released_as(op_id, "published"))
    {
        return Ok(AfterRelease::Stands);
    }
    if let Some(landed) = landed_on_chain(ctx, op_id, true)? {
        return Ok(AfterRelease::Landed(Box::new(landed)));
    }
    let Ok(main) = ctx.repo.fetch_trunk() else {
        return Ok(AfterRelease::Unanswered);
    };
    if let Some(landed) = landed_on_chain(ctx, op_id, true)? {
        return Ok(AfterRelease::Landed(Box::new(landed)));
    }
    let revision = ctx.op(op_id).revision.clone();
    Ok(match scanned(ctx, &main, &revision)? {
        Some(landed) => AfterRelease::Landed(Box::new(existing(landed))),
        None => AfterRelease::Unanswered,
    })
}

/// G800 (a fleet measurement, 2026-11-19: in 28 h one box's two runners made 524 visits to an
/// operation another runner had claimed and landed a median 59 s earlier, 10,353 s in all;
/// each fetched main and the registry, scanned main for the landing, fetched the logs twice for
/// a terminal record the landing runner had written, and pushed a chain it had not changed):
/// the result of a visit to `op_id`, whose queue place the line released `published`, read from
/// the change's own chain alone. Its landing commit links the published commit, so one fetch
/// brings what the landing row is derived from: the same row as a scan of main finds
/// ([`derive_landed`]), marked `existing`, with no terminal record and no push of its own. `None`
/// sends the visit the long way: under a registry that signs landings, whose scan verifies the
/// publication's signature, or when the chain does not say the operation published.
fn landed_elsewhere(ctx: &mut Ctx, op_id: &str) -> Result<Option<BayResult>, String> {
    if ctx.refetch().is_err() {
        return Ok(None);
    }
    // G863 (a fleet finding, 2026-11-22: a batch member's chain still said checked 3.7 h after its
    // landing, and two runners visited it about once a minute each, about 320 visits, each
    // answered from the chain with nothing written): a chain its landing runner left checked is
    // recorded from main, the long way, as a visit under a registry that signs landings records it.
    if chain_left_checked(ctx, op_id)
        && let Ok(main) = ctx.repo.fetch_trunk()
    {
        let revision = ctx.op(op_id).revision.clone();
        if let Some(landed) = scanned(ctx, &main, &revision)? {
            return Ok(Some(existing(landed)));
        }
    }
    landed_on_chain(ctx, op_id, true)
}

/// G863: whether `op_id`'s chain, as this visit just read it, still says checked after its
/// holder's chain and lease stood unmoved for a lease on this looping runner's watch
/// ([`sighted`]): its landing runner stopped before it recorded the landing, and will not.
fn chain_left_checked(ctx: &Ctx, op_id: &str) -> bool {
    let Some(op) = ctx.find_op(op_id).filter(|op| op.state == OpState::Checked) else {
        return false;
    };
    let lease = holder_lease(&ctx.repo.config, op.owner.as_deref());
    sighted(&ctx.repo.git, &ctx.id, &ctx.head).is_some_and(|seen| seen.elapsed() >= lease)
}

/// G800: [`landed_elsewhere`]'s answer from the change's chain as this visit just read it:
/// `None` under a registry that signs landings, or when the chain does not say `op_id`
/// published, with its commit in this store. When the line `released` its place `published`,
/// in the push that moved main, the operation published its checked candidate, though its
/// landing runner may not have recorded so on the chain yet (a test finding, 2026-11-19: a
/// visit in that window read the chain as checked, found main as the pass first read it, and
/// placed the landed operation in line again): a chain that says it checked is enough.
fn landed_on_chain(
    ctx: &mut Ctx,
    op_id: &str,
    released: bool,
) -> Result<Option<BayResult>, String> {
    ctx.registry = Registry::current(&ctx.repo.git)?;
    if ctx.signing().is_some() {
        return Ok(None);
    }
    let Some(op) = ctx.find_op(op_id) else {
        return Ok(None);
    };
    let Some(commit) = op
        .attempts
        .last()
        .map(|attempt| attempt.commit.clone())
        .filter(|_| {
            (op.state == OpState::Published && ctx.record.state == ChangeState::Landed)
                || (released && op.state == OpState::Checked)
        })
    else {
        return Ok(None);
    };
    if ctx
        .repo
        .git
        .run(&["cat-file", "-e", &format!("{commit}^{{commit}}")])
        .is_err()
    {
        return Ok(None);
    }
    // G875: a chain's landing whose commit main does not hold is no landing (a record an older
    // client pushed before its landing push was refused), and is never answered merged.
    let revision = op.revision.clone();
    if !main_holds(ctx.repo, &commit) {
        return Ok(None);
    }
    // The landing runner recorded it, its terminal record and its wake: this visit writes none.
    Ok(Some(existing(landing_row(ctx, op_id, &revision, &commit))))
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
            intent: &ctx.record.intent.reference,
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
/// on trunk, and a wake is no outcome of its own. G825: each member of a merged batch is woken
/// so too, by its own context ([`land_batch`]).
///
/// G868 (a fleet measurement, 2026-11-23: each landing's wake made two fetches and a push of its
/// own after the publishing push, about 8 s of the runner's time): after this process's own
/// push landed the operation, no log can hold its wake yet, so none is fetched to look for one,
/// and the event goes on this box's log only. The next push of the log carries it: the next
/// landing's, or the one at the end of the pass ([`crate::executor`]).
fn landed_wake(ctx: &Ctx, op_id: &str, decision: Decision) -> Result<(), String> {
    if decision != Decision::Landed || !crate::talk::wake::configured(ctx.repo) {
        return Ok(());
    }
    let id = digest(&json!({"op_id": op_id, "outcome": "landed"}))?;
    let id = format!("evt_{}", &id.trim_start_matches("sha256:")[..20]);
    let ours = ctx.pushed_landing.is_some();
    let held = if ours {
        crate::log::held_events(&ctx.repo.git)?.contains(&id)
    } else {
        crate::log::holds_event(ctx.repo, &id)?
    };
    if held {
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
    if ours {
        return crate::log::write_local(ctx.repo, &executor, signer.as_ref(), &event).map(|_| ());
    }
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
    if crate::log::holds_event(ctx.repo, &id)? {
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
/// `git log` over the executor logs on this box, a fetch of the logs only when they lack one of
/// the events (G887), and one append per event written.
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
    let mut events = Vec::with_capacity(blocked.len());
    for (record, op_id, reason) in blocked {
        events.push(blocked_event(
            record,
            op_id,
            reason,
            &serde_json::Value::Null,
        )?);
    }
    let id = |event: &serde_json::Value| event["id"].as_str().unwrap_or_default().to_owned();
    // G887: the logs on this box are read first, and fetched only when they lack an event.
    let mut held = crate::log::held_events(&repo.git)?;
    if events.iter().all(|event| held.contains(&id(event))) {
        return Ok(0);
    }
    crate::log::fetch(repo);
    held = crate::log::held_events(&repo.git)?;
    let keys = Keys::load(&repo.git);
    // N-21: executor logs are signed only where the registry signs everything.
    let signer = match Registry::current(&repo.git)?.filter(Registry::signs_all) {
        None => None,
        Some(_) => Some(keys.executor.clone().ok_or(
            "the repository has a key registry and this machine has no runner key (git config bay.runnerKey)",
        )?),
    };
    let mut written = 0;
    for event in events {
        if held.contains(&id(&event)) {
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

/// G551 (a fleet finding, 2026-10-29: a head took over a silent holder's claim after a full
/// lease, planned its batch on the main it read before the wait, and its push was refused for a
/// landing made during the wait): the claim can wait out a lease, so main is read again once it
/// is won. A main that is no descendant of `trunk` keeps `trunk`; its push is refused, and the
/// next pass reads main again. BACKLOG 92: a claim that waited for no one keeps the main this
/// visit read moments before, one fetch fewer a merge; a main that moved since refuses the
/// publishing push, and the operation builds again on the main it reads then.
fn trunk_after_claim(ctx: &mut Ctx, trunk: String) -> String {
    if !ctx.claim_waited {
        return trunk;
    }
    match ctx.repo.refetch_trunk() {
        Ok(now) if now != trunk && ctx.repo.git.is_ancestor(&trunk, &now).unwrap_or(false) => {
            ctx.record.observed_remote.clone_from(&now);
            now
        }
        _ => trunk,
    }
}

/// Steps 2–7 for one operation, rebuilding when trunk moves.
fn execute_op(ctx: &mut Ctx, op_id: &str, trunk: String) -> Result<BayResult, String> {
    let revision = ctx.op(op_id).revision.clone();
    let executor = ctx.executor_signer()?;
    ctx.chain.sign_with(executor);
    if let Some(refusal) = verify_request(ctx, op_id)? {
        return Ok(refusal);
    }
    if let Some(waiting) = waits_for_worker(ctx, op_id, &revision, &trunk) {
        return Ok(waiting);
    }
    // B3: a change whose own config does not load never waits in line for a merge that would
    // stop every bay command (a fleet finding, 2026-10-26).
    if let Some(refused) = config_gate(ctx, op_id, &revision, &trunk)? {
        return Ok(refused);
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
        queue_gate(ctx, op_id, &trunk)?
    };
    let mut seq = match gate {
        // No check ahead of its turn for a visit beside other work (S1), which ends at its place.
        Gate::Wait(result) if ctx.visit == Visit::Place => {
            return Ok(*result);
        }
        Gate::Wait(result) => return Ok(ahead_of_turn(ctx, op_id, &trunk, *result)),
        _ if ctx.visit == Visit::Place => return Err(LEFT_FOR_PASS.to_owned()),
        Gate::Head(seq) => Some(seq),
        Gate::Free => None,
    };
    kill_point("visit-gated");
    if ctx.repo.took_shared_read() {
        return Err(READ_AGAIN.to_owned());
    }
    if let Some(waiting) = served_elsewhere(ctx, op_id, &revision, &trunk)? {
        return Ok(waiting);
    }
    if let Some(waiting) = policy_unread_here(ctx, op_id, &trunk) {
        return Ok(waiting);
    }
    if let Some(waiting) = box_busy(ctx, op_id) {
        return Ok(waiting);
    }
    if let Some(report) = claim(ctx, op_id, &revision, &trunk)? {
        return Ok(report);
    }
    kill_point("after-claim");
    let trunk = trunk_after_claim(ctx, trunk);
    // G173: whatever the outcome, the operation's anchor goes with it.
    let repo = ctx.repo;
    // A claim this process gives up before any attempt is given back, never left `running`.
    let _anchored = match Anchored::new(&repo.git, op_id) {
        Ok(anchored) => anchored,
        Err(why) => {
            release_after_fault(ctx, op_id, &revision, &host_said(&why));
            return Err(why);
        }
    };
    let mut outcome = run_attempts(ctx, op_id, &revision, trunk, &mut seq);
    // B2: every result of an operation judged by the last valid policy names it.
    if let (Ok(result), Some(used)) = (outcome.as_mut(), ctx.policy_used.borrow().clone()) {
        let detail = result.detail.get_or_insert_with(|| json!({}));
        if detail.is_object() {
            detail["policy_used"] = used;
        }
    }
    if let Err(why) = &outcome {
        release_after_fault(ctx, op_id, &revision, &host_said(why));
    }
    // The head always releases: an outcome other than its own publication (which released in
    // the publishing push) passes the head on. Not an operation given up on `REGISTRY_CHANGED`:
    // its key can sign no release, and the place stays with the operation, which the executor
    // that takes it over publishes as the head (§ 10a).
    // F11: nor a head that gave its pass back: it keeps its place for a later pass.
    // G797: nor one that keeps its place for another runner ([`keeps_place`]).
    let given_up = outcome.as_ref().is_ok_and(|result| {
        result.reason == Some(Reason::RegistryChanged)
            || result
                .detail
                .as_ref()
                .is_some_and(|detail| detail["yielded"] == true || detail["kept"].is_u64())
    });
    // BACKLOG 99: a landing of this process's own push released the place in that push.
    if let Some(at) = seq
        && !given_up
        && ctx.pushed_landing.is_none()
        && holds_place(ctx, op_id)
        && !crate::queue::Queue::read(ctx.repo)?.released.contains(&at)
    {
        let signer = ctx.executor_signer().ok().flatten();
        let reason = release_reason(&outcome);
        let _ = crate::queue::release(ctx.repo, signer.as_ref(), at, op_id, &reason);
    }
    outcome
}

/// The reason a head's place is released with after `outcome`, its run's outcome.
fn release_reason(outcome: &Result<BayResult, String>) -> String {
    match outcome {
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
    }
}

/// Whether this process may give the head's place back after its run of `op_id`: the remote's
/// operation has ended, or this process's claim still stands (its epoch and its name). A fleet
/// finding (2026-10-26): a runner whose head another runner had taken over released the place
/// as `failed` when its own run ended, and a head whose check had passed was never published.
/// The runner that holds the claim releases it.
fn holds_place(ctx: &mut Ctx, op_id: &str) -> bool {
    if ctx.refetch().is_err() {
        return true;
    }
    let name = ctx.executor_name();
    ctx.find_op(op_id).is_none_or(|op| {
        op.state.is_terminal()
            || op.state == OpState::Blocked
            || (op.epoch == ctx.my_epoch && op.owner.as_deref() == Some(name.as_str()))
    })
}

/// A fault ended this executor's run of `op_id` after its claim (§ 9a): the claim is given
/// back on the remote chain, so the operation never stays `running` under an owner that has
/// stopped. The next executor takes it at once, with no lease to wait out. Best effort: when
/// the chain moved to another epoch, or the remote cannot be reached, that state stands.
/// G553 (a fleet finding, 2026-10-29: twelve changes were released "after a fault" two to
/// seven times each in 90 minutes, and their records did not say why): the release names its
/// `cause`.
fn release_after_fault(ctx: &mut Ctx, op_id: &str, revision: &str, cause: &str) {
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
    let message = format!("released {op_id} epoch {mine} after a fault: {cause}");
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
            // G796: a batch, or a candidate, whose checks main's move stopped.
            if ctx.stopped.borrow().is_some() {
                if let Some(result) = after_stopped_check(ctx, op_id, &revision, &mut trunk, seq)? {
                    return Ok(result);
                }
                continue;
            }
            // G845: the lease names this operation alone as what its check covers.
            names_batch(ctx, op_id, vec![op_id.to_owned()]);
            match build_and_check(ctx, op_id, &revision, &trunk, approved)? {
                Ok(attempt) => attempt,
                Err(_) if ctx.stopped.borrow().is_some() => {
                    if let Some(result) =
                        after_stopped_check(ctx, op_id, &revision, &mut trunk, seq)?
                    {
                        return Ok(result);
                    }
                    continue;
                }
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

        if let Some(result) = publish_checked(ctx, op_id, &revision, &mut trunk, seq, &attempt)? {
            return Ok(result);
        }
    }
}

/// Step 6 for `attempt`, a candidate checked on `trunk`: its publishing push, and the landing's
/// records. Returns the operation's outcome, or `None` when the head goes round again: main moved
/// and the candidate is built anew, or the same candidate is pushed again.
fn publish_checked(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    trunk: &mut String,
    seq: &mut Option<u64>,
    attempt: &Attempt,
) -> Result<Option<BayResult>, String> {
    ctx.step.set("publish");
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
    // G864: the landing's records ride in its publishing push, whose chain, on the head this
    // process last pushed, stands for a read of the chain before it.
    let refused = match land_with_records(
        ctx,
        op_id,
        revision,
        &attempt.commit,
        release.as_ref(),
        claim.as_ref(),
    )? {
        Ok(landed) => return Ok(Some(landed)),
        Err(refused) => refused,
    };
    // BACKLOG 294 (a fleet measurement, 2026-11-25: solo landings made four publishing pushes, not
    // one, and no record said why the first was refused): the runner's journal names the refusal.
    if !records_apart() {
        eprintln!(
            "bay runner: {op_id} landing push refused: {}",
            host_said(&refused)
        );
    }
    // G897: the checked record and evidence that rode the refused push go to the remote now, so
    // nothing lands without them. A chain the remote moved meanwhile took them back.
    let flushed = ctx.flush_riding();
    // A refused landing reads the chain again first, as a landing before G864 read it before its
    // push. A refusal on a main that moved, at the hook's time bound, or for a ref lock another
    // process holds on the remote (G128; under reftable one lock holds every ref, so it refused the
    // flush too) is answered as any refused publishing push is: a second push would be a second
    // such refusal, uncounted. On an unmoved main the landing goes as before G864: trunk pushed
    // alone, and the records after it.
    if let Some(report) = still_owner(ctx, op_id, revision)? {
        return Ok(Some(report));
    }
    if refused.contains(Reason::HookTimeout.code())
        || held_lock(&refused)
        || !ctx.repo.fetch_trunk().is_ok_and(|now| now == *trunk)
    {
        // The push may have landed before its answer was lost, whatever the flush said: main tells.
        return after_refused_push(ctx, op_id, revision, trunk, seq, attempt, &refused);
    }
    // Main is unmoved, so the landing did not land. Evidence the remote refuses fails the
    // operation, as a persist's own push failed it before; a remote chain that lacks the checked
    // record sends the head round again to check anew. Trunk never goes alone without them.
    if !flushed? {
        return Ok(None);
    }
    kill_point("before-publish");
    match publish_push(
        ctx.repo,
        &ctx.id,
        &attempt.commit,
        release.as_ref(),
        claim.as_ref(),
        &Riding::default(),
    ) {
        Ok(()) => {
            kill_point("after-publish");
            ctx.pushed_landing = Some(attempt.commit.clone());
            derive_landed(ctx, op_id, revision, &attempt.commit).map(Some)
        }
        Err(error) => after_refused_push(ctx, op_id, revision, trunk, seq, attempt, &error),
    }
}

/// Names a landed batch in its landing's `detail.batch`: its checks, why it ended and its
/// members. G588: `at`, the change that ended it, only while the plan's reason stands.
fn name_batch(
    detail: &mut serde_json::Value,
    checks: &[serde_json::Value],
    stopped: &str,
    at: Option<&str>,
    landing: &[throughput::Member],
) {
    detail["batch"]["checks"] = json!(checks);
    detail["batch"]["stopped"] = json!(stopped);
    if let Some(at) = at {
        detail["batch"]["stopped_at"] = json!(at);
    }
    detail["batch"]["members"] = json!(
        landing
            .iter()
            .map(|member| json!({"change": member.change, "op_id": member.op_id}))
            .collect::<Vec<_>>()
    );
}

/// Batching with bisection (`crate::throughput`): the head `op_id`, at queue place `at`, and
/// the live entries behind it that fold onto it, checked once as one tree on `trunk`. On a pass
/// they land together; on a failure the bisection finds the largest passing prefix, which lands
/// together when it holds two members or more, and notes the first failing member. `None` when
/// no batch forms or none lands: the head runs its ordinary path, on the records of its own
/// prefix when the bisection checked it, since that prefix is its own candidate's tree on its
/// own base.
/// G760 (a gate finding, 2026-11-16): the changes of the batch this process lands, from its plan
/// until it ends. A runner places a change's newer revision in line while its head runs
/// (`crate::executor`); for a change of that batch, each such write raced the batch's own claim
/// and persist on the change's chain, and under load the batch lost its member. A member's newer
/// revision is placed by a later pass, after the batch.
static BATCHED: std::sync::Mutex<BTreeSet<String>> = std::sync::Mutex::new(BTreeSet::new());

/// G760: whether `change` is a member of the batch this process is landing ([`BATCHED`]).
#[must_use]
pub fn in_running_batch(change: &str) -> bool {
    BATCHED.lock().is_ok_and(|batched| batched.contains(change))
}

/// G789: the landings of the members a batch landed beside its head, as [`land_batch`] derived
/// them: (change, operation, outcome).
static MEMBERS_LANDED: std::sync::Mutex<Vec<(String, String, BayResult)>> =
    std::sync::Mutex::new(Vec::new());

/// G789: the landings of the members of the batches this process landed since the last call, so
/// a runner names each in its pass and journal without a visit. A pass that takes the head
/// before it reads the chains finds the members ended; one that read them before used to visit
/// each again, only to find it on main (on one fleet a visit took p50 16 s, 2026-11-18).
#[must_use]
pub fn members_landed() -> Vec<(String, String, BayResult)> {
    MEMBERS_LANDED
        .lock()
        .map(|mut landings| std::mem::take(&mut *landings))
        .unwrap_or_default()
}

/// G760: the members of a batch, registered in [`BATCHED`] while it lands.
struct Batched(Vec<String>);

impl Batched {
    fn new(members: &[throughput::Member]) -> Self {
        let changes: Vec<String> = members.iter().map(|member| member.change.clone()).collect();
        if let Ok(mut batched) = BATCHED.lock() {
            batched.extend(changes.iter().cloned());
        }
        Self(changes)
    }
}

impl Drop for Batched {
    fn drop(&mut self) {
        if let Ok(mut batched) = BATCHED.lock() {
            for change in &self.0 {
                batched.remove(change);
            }
        }
    }
}

fn batch(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    trunk: &str,
    at: u64,
) -> Result<Option<BayResult>, String> {
    let repo = ctx.repo;
    // E21: each part of a head's work names its own step, so a line says where the time went.
    ctx.step.set("batch");
    let Some((policy, members, mut stopped, stopped_at)) =
        batch_plan(ctx, op_id, revision, trunk, at)?
    else {
        return Ok(None);
    };
    let planned = stopped;
    let _batched = Batched::new(&members);
    // N-8: a held trunk publishes no batch, whose members need not all be fixes; the head
    // runs its ordinary path, judged by the hold alone.
    if crate::hold::held(repo, trunk).is_some() {
        return Ok(None);
    }
    red_gate(ctx, revision, trunk)?;
    batch_reds(ctx, &members, trunk)?;
    // G677: each red post check runs on every prefix the batch checks, as a pre check judged
    // against main, as it runs on a lone candidate.
    let policy = policy.judging_red(&ctx.red_ids());
    ctx.step.set("check");
    let id = throughput::batch_id(op_id, trunk)?;
    let Some((search, mut checks)) = bisect_batch(ctx, op_id, trunk, &policy, &members, &id)?
    else {
        return taken_from(ctx, op_id, revision);
    };
    // G910: a head another runner took claims no member, and checks nothing more.
    if let Some(taken) = taken_from(ctx, op_id, revision)? {
        return Ok(Some(taken));
    }
    if let Some(culprit) = search.culprit().filter(|culprit| *culprit >= 2) {
        let member = &members[culprit - 1];
        let _ = note_culprit(repo, &member.change, &member.op_id, &id);
    }
    if search.passed() < 2 {
        return Ok(None);
    }
    let mut others = Vec::new();
    let Some(size) = join_or_cut(
        ctx,
        (op_id, trunk, &policy, &id),
        &members[..search.passed()],
        &mut others,
        (&mut checks, &mut stopped),
    )?
    else {
        return Ok(None);
    };
    let landing = &members[..size];
    let landed = land_batch(ctx, &mut others, landing, trunk, &id);
    // G754: once its batch is done, a head that runs on alone keeps the lone rule.
    ctx.membership = Membership::Alone;
    match landed {
        Ok(Some(mut result)) => {
            if let Some(detail) = result.detail.as_mut() {
                let at = stopped_at.as_deref().filter(|_| stopped == planned);
                name_batch(detail, &checks, stopped, at, landing);
            }
            Ok(Some(result))
        }
        Err(why) => {
            // F11: a batch refused because trunk moved is no fault. The runner's head yields
            // with its place kept, and the next pass forms the batch again on the moved trunk.
            // B4: so is a batch the host refused while trunk stood still (a transient refusal
            // such as GitHub's GH009); it counts one uncounted publish retry, then yields.
            let now = if ctx.visit == Visit::Own {
                None
            } else {
                ctx.repo.fetch_trunk().ok()
            };
            let fault = match &now {
                Some(now) if now.as_str() != trunk => "main moved under the batch".to_owned(),
                _ => format!("the batch did not land ({})", host_said(&why)),
            };
            give_back(&mut others, landing, &fault);
            if let Some(now) = now {
                if now.as_str() == trunk
                    && !judged_by_bay(&why)
                    && let Some(blocked) = uncounted_retry(
                        ctx,
                        op_id,
                        revision,
                        &format!("the host refused the batch's push ({})", host_said(&why)),
                    )?
                {
                    return Ok(Some(blocked));
                }
                let cause = if now.as_str() == trunk {
                    "the host refused its batch's push"
                } else {
                    // G552: counted as a single push's refusal is.
                    ctx.op_mut(op_id).trunk_moved += 1;
                    "main moved"
                };
                return yield_head(ctx, op_id, revision, at, cause).map(Some);
            }
            Err(why)
        }
        other => {
            give_back(
                &mut others,
                landing,
                "a member left the batch before its push",
            );
            other
        }
    }
}

/// G938 (a load measurement, 2026-11-27: while main was red, its batches held one to three
/// members where five were planned, and the line grew from 4 to 113): the red post checks on the
/// paths the members behind the head change join the head's own (`red_gate`). Each runs on the
/// batch's checks, judged against main, so a member on a red's paths lands in the batch unless it
/// makes that red worse, as it would alone ("merge unless worse", G677). The batch used to end
/// before the first such member, a rule from when a change on a red's paths waited for main.
fn batch_reds(ctx: &mut Ctx, members: &[throughput::Member], trunk: &str) -> Result<(), String> {
    let (Some(head), Some(last)) = (members.first(), members.last()) else {
        return Ok(());
    };
    if head.op_id == last.op_id {
        return Ok(());
    }
    let git = &ctx.repo.git;
    let reds = journaled(ctx, || {
        crate::red::holding(git, trunk, ctx.registry.as_ref())
    })?;
    if reds.is_empty() {
        return Ok(());
    }
    // What the members behind the head change, stacked on it. Only the head's names repair a
    // red here: a member's own Fixes: makes it a repair, which runs from its own place.
    let changed = check::changed_paths(git, &head.commit, &last.commit)?;
    for red in crate::red::blocking(&reds, &changed, &[]) {
        let (id, units) = crate::bisect::red_units(git, ctx.registry.as_ref(), red)?;
        if !ctx.red_checks.iter().any(|(asked, _)| *asked == id) {
            ctx.red_checks.push((id, units));
        }
    }
    Ok(())
}

/// Whether a refused push was refused by bay's own host hook, which judged it (its lines read
/// `remote: bay: ...`): a judgement, never a fault to retry.
fn judged_by_bay(error: &str) -> bool {
    error.contains("remote: bay:")
}

/// Whether a refused push was refused because this machine may not update the ref (GitHub's
/// GH013 rule violation, a protected ref or branch, an HTTP 403): a retry from here is refused
/// the same way.
fn refused_for_permission(error: &str) -> bool {
    let said = host_said(error).to_lowercase();
    [
        "gh013",
        "protected ref",
        "protected branch",
        "error: 403",
        "permission to ",
    ]
    .iter()
    .any(|sign| said.contains(sign))
}

/// BACKLOG 81: what the host said when it refused a push, for a one-line record: git's own
/// refusal lines (`remote: ...`, ` ! [remote rejected] ...`, `error: ...`, `fatal: ...`), each
/// once, at most 300 characters. A batch's push names thirty refs or more, so the command line
/// filled the record and the host's reason was lost (2026-10-27/28). The lines are read from
/// git's stderr, after the `failed: ` of [`crate::git::GitError::message`], whose first line
/// shares the command's line. Without such lines, the first line of `text`.
fn host_said(text: &str) -> String {
    let stderr = text
        .split_once(" failed: ")
        .map_or(text, |(_, stderr)| stderr);
    let mut said: Vec<&str> = Vec::new();
    for line in stderr.lines().map(str::trim) {
        let refusal = ["remote:", "! [", "error:", "fatal:"]
            .iter()
            .any(|start| line.starts_with(start))
            && line.trim_start_matches("remote:").trim() != "";
        if refusal && !said.contains(&line) {
            said.push(line);
        }
    }
    if said.is_empty() {
        return first_line(text);
    }
    said.join("; ").chars().take(300).collect()
}

/// The first non-empty line of `text`, at most 160 characters, for a one-line record.
fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .take(160)
        .collect()
}

/// A planned batch: the policy it is judged by, its members, why it ended (R7), and the change
/// it ended at (G588).
type Planned = (
    policy::Policy,
    Vec<throughput::Member>,
    &'static str,
    Option<String>,
);

/// G420, G560: the journal line of a head that lands alone because its diff (`revision`
/// folded onto `trunk`) wakes a pre-merge check keyed by its commit, naming the check.
fn say_commit_keyed(
    git: &Git,
    policy: &policy::Policy,
    (trunk, revision): (&str, &str),
    op_id: &str,
) -> Result<(), String> {
    if let throughput::Fold::Applied { changed, .. } =
        throughput::fold(git, policy, trunk, revision)?
        && let Some(entry) = policy.commit_keyed_woken(&changed)
    {
        eprintln!(
            "bay runner: {op_id} no batch: check {} is keyed by its commit",
            entry.id
        );
    }
    Ok(())
}

/// G788 (a fleet finding, 2026-11-18: 20 of 55 batch plans in 12 hours passed over one to three
/// places "not read here"): [`throughput::read_line`] of `entries`, after one fetch of the
/// chains of those this store cannot read yet, such as a place another runner took after this
/// pass fetched the chains. A batch, and a check of the next one ahead, passes over only what
/// the remote cannot give, so the head and every runner that checks ahead plan alike.
fn read_line_fetched<'a>(
    repo: &Repo,
    entries: &[&'a crate::queue::Entry],
) -> (Vec<throughput::Queued>, Vec<&'a crate::queue::Entry>) {
    let (line, unread) = throughput::read_line(&repo.git, entries.iter().copied());
    if unread.is_empty() {
        return (line, unread);
    }
    let changes: Vec<String> = unread.iter().map(|entry| entry.change.clone()).collect();
    let _ = repo.remote_chains(&changes);
    throughput::read_line(&repo.git, entries.iter().copied())
}

/// G712 (a fleet finding, 2026-11-13: a code change's batch took in two notes changes, each
/// merged from its own place by G703 while the batch was checked, and the batch was cut and
/// checked again twice, 189 s and 370 s lost; changes behind it waited 16 to 35 minutes): a
/// change that runs from its own place past this head joins no batch of it. It lands alone in
/// seconds, and the batch's check stays the one that lands. Returns the places it passed over.
fn pass_over_passing(
    ctx: &Ctx,
    queue: &crate::queue::Queue,
    trunk: &str,
    behind: &mut [throughput::Queued],
) -> Result<Vec<u64>, String> {
    let mut passed = Vec::new();
    for queued in behind {
        if queued.live
            && queued.batchable
            && !queue.expedite_ahead(queued.seq)
            && passes_unread(ctx, queue, queued.seq, trunk, &queued.revision)?
            && !repair_moved_under(ctx, queue, trunk, &queued.op_id)?
        {
            queued.batchable = false;
            passed.push(queued.seq);
        }
    }
    Ok(passed)
}

/// G867 (a fleet finding, 2026-11-23: a runner took a code change from its own place 18 s after
/// another runner claimed a notes head; the head's batch took that change in, checked for about
/// 1,280 s, and was cut when the change landed, about 21 minutes more for the head): a place
/// another runner holds lands from its own place past this head, so it joins no batch of it;
/// its claim would fail after the batch's check. Nor does a member whose check reads what such a
/// place changes, unless the head's check reads it too: that landing would stop the batch's
/// check. Returns the places passed over: those held, then those whose check reads them.
fn pass_over_held(
    ctx: &Ctx,
    policy: &policy::Policy,
    trunk: &str,
    head: &throughput::Queued,
    behind: &mut [throughput::Queued],
) -> Result<(Vec<u64>, Vec<u64>), String> {
    let git = &ctx.repo.git;
    let me = ctx.executor_name();
    let mut taken = Vec::new();
    let mut theirs = throughput::Own::default();
    let mut unknown = false;
    for queued in behind.iter_mut() {
        if !queued.live || queued.holder.as_ref().is_none_or(|holder| *holder == me) {
            continue;
        }
        queued.batchable = false;
        taken.push(queued.seq);
        match own_change(git, trunk, &queued.revision)? {
            Some(own) => theirs.absorb(own),
            None => unknown = true,
        }
    }
    // A held place of unknown paths reads as changing everything, which the head reads too.
    if taken.is_empty()
        || unknown
        || !reads_none_of(git, policy, trunk, [head.revision.as_str()], &theirs)?
    {
        return Ok((taken, Vec::new()));
    }
    let mut readers = Vec::new();
    for queued in behind.iter_mut() {
        if queued.live
            && queued.batchable
            && !reads_none_of(git, policy, trunk, [queued.revision.as_str()], &theirs)?
        {
            queued.batchable = false;
            readers.push(queued.seq);
        }
    }
    Ok((taken, readers))
}

/// G852 (a fleet finding, 2026-11-22: a code change was passed over as one that passes a head on
/// notes, and the head's batch then took in three code changes from behind it; the batch's code
/// check read it, so it could not pass that batch (G846), and it waited the whole batch out and
/// was checked alone after it, 63 minutes in the line): joins the batch again each of
/// `passers`, places of `behind` passed over by [`pass_over_passing`], that the check of the
/// planned `members` reads. True when it readmits one, and the batch is planned again.
fn readmit_read(
    git: &Git,
    policy: &policy::Policy,
    trunk: &str,
    members: &[throughput::Member],
    behind: &mut [throughput::Queued],
    passers: &mut Vec<u64>,
) -> Result<bool, String> {
    let mut readmitted = Vec::new();
    for queued in behind.iter_mut() {
        if !passers.contains(&queued.seq) {
            continue;
        }
        let ours = own_change(git, trunk, &queued.revision)?.unwrap_or_default();
        let revisions = members.iter().map(|member| member.revision.as_str());
        if !reads_none_of(git, policy, trunk, revisions, &ours)? {
            queued.batchable = true;
            readmitted.push(queued.seq);
        }
    }
    passers.retain(|seq| !readmitted.contains(seq));
    Ok(!readmitted.is_empty())
}

/// G905 (a fleet measurement, 2026-11-25: a speculative check passed a batch of four on this
/// trunk, the batch grew to seven by its turn, and the head ran a 32-minute check again): the
/// head keeps the longest prefix of `members` a check ahead of its turn already passed on
/// `trunk`, and the members after it plan a batch of their own. The change the batch now ends
/// at, when it is shorter.
fn keep_checked_prefix(
    ctx: &Ctx,
    op_id: &str,
    trunk: &str,
    members: &mut Vec<throughput::Member>,
) -> Result<Option<String>, String> {
    let trunk_tree = tree_of(&ctx.repo.git, trunk)?;
    let Some(size) = throughput::checked_prefix(&ctx.op(op_id).prechecks, &trunk_tree, members)
        .filter(|size| *size < members.len())
    else {
        return Ok(None);
    };
    let at = members[size].change.clone();
    members.truncate(size);
    Ok(Some(at))
}

/// The batch the head `op_id` at place `at` would form on `trunk`, with the policy at
/// `trunk`: `None` when batching is off, reuse cannot stand for a run (§ 7,
/// [`Ctx::reuse_trusted`]), the head runs ahead of its turn (G554), is a repair, a culprit or
/// names inputs, its diff wakes a pre-merge check keyed by its commit (G560: the batch's
/// candidate commit was never checked), fewer members than `max(min, 2)` fold, or the lane's
/// recent failure rate is above `max_failure_rate`.
#[allow(clippy::too_many_lines)]
fn batch_plan(
    ctx: &Ctx,
    op_id: &str,
    revision: &str,
    trunk: &str,
    at: u64,
) -> Result<Option<Planned>, String> {
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
    // R7, G420: a rule that keeps every batch off says so in the journal, as a batch that ends
    // does (measured 2026-10-27: one changed-only check kept a fleet's batches off unseen).
    if let Some(why) = no_batch(ctx, &policy, trunk) {
        eprintln!("bay runner: {op_id} no batch: {why}");
        return Ok(None);
    }
    // G789: the line as the remote holds it now. The head's visit may stand on the queue its
    // take read, and a place that joined since is in the batch.
    let queue = crate::queue::Queue::read(repo)?;
    // G554 (a fleet finding, 2026-10-29: a change that needed no check ran from its own place
    // while the head checked, and planned a batch of ten from the line behind it; the two
    // batches claimed the same members, one landed, and the other gave every member back): a
    // head that runs ahead of its turn, a repair or a change that needs no check, lands alone.
    if queue.head().is_some_and(|head| head.seq != at) {
        eprintln!("bay runner: {op_id} no batch: it runs ahead of its turn");
        return Ok(None);
    }
    // BACKLOG 326: an expedite runs alone.
    if queue.entries.get(&at).is_some_and(|entry| entry.expedite) {
        eprintln!("bay runner: {op_id} no batch: it is an expedite");
        return Ok(None);
    }
    let head = throughput::Queued {
        seq: at,
        change: ctx.id.clone(),
        op_id: op_id.to_owned(),
        revision: revision.to_owned(),
        live: true,
        batchable: true,
        checked: None,
        fails_on: Vec::new(),
        screen: throughput::screened(&ctx.op(op_id).prechecks),
        holder: Some(ctx.executor_name()),
        expedite: false,
    };
    let waiting = queue.behind(at);
    // G867: the chains and leases as the remote holds them now, so a place another runner took
    // since this pass fetched them reads held. One fetch for the whole line (G788 fetched only
    // the places this store could not read).
    let chains: Vec<String> = waiting.iter().map(|entry| entry.change.clone()).collect();
    let _ = repo.remote_chains(&chains);
    let (mut behind, unread) = throughput::read_line(git, waiting.iter().copied());
    let elsewhere = pass_over_held(ctx, &policy, trunk, &head, &mut behind)?;
    let mut passers = pass_over_passing(ctx, &queue, trunk, &mut behind)?;
    let by_lane = repo.config.footprint_lanes();
    let size = batch_size(git, &queue, &policy, bounds.max);
    // G852: a passer the planned batch's check reads joins it, and the batch is planned again.
    // Each round readmits one passer at least, so the rounds end.
    let (mut members, mut stopped, mut stopped_at) = loop {
        let planned = throughput::plan_batch(git, &policy, trunk, &head, &behind, size, by_lane)?;
        if !readmit_read(git, &policy, trunk, &planned.0, &mut behind, &mut passers)? {
            break planned;
        }
    };
    // G420, G560: a head whose diff wakes a pre-merge check keyed by its commit lands alone,
    // and the journal names the check.
    if stopped == "commit_keyed" {
        say_commit_keyed(git, &policy, (trunk, revision), op_id)?;
        return Ok(None);
    }
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
        if within < members.len() {
            stopped = "size_limits";
            stopped_at = Some(members[within].change.clone());
        }
        members.truncate(within);
    }
    if let Some(at) = keep_checked_prefix(ctx, op_id, trunk, &mut members)? {
        stopped = "checked";
        stopped_at = Some(at);
    }
    // R7: one journal line per batch decision, naming why the batch ended, and each place passed
    // over.
    let passed_over = passed_over_note(&unread, &behind, &passers, &elsewhere);
    // G588: and the change the batch ended at, when one entry ended it.
    let at_change = stopped_at
        .as_deref()
        .map_or_else(String::new, |change| format!(" at change {change}"));
    let said = |how: &str, stopped: &str, at: &str| {
        eprintln!(
            "bay runner: {op_id} batch of {} {how}, stopped: {stopped}{at}{passed_over}",
            members.len()
        );
    };
    if members.len() < bounds.min.max(2) {
        said("not formed", stopped, &at_change);
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
        said("not formed", "failure_rate", "");
        return Ok(None);
    }
    said("planned", stopped, &at_change);
    Ok(Some((policy, members, stopped, stopped_at)))
}

/// The batch line's note of the places passed over: G581, each whose operation does not read
/// here; G852, each of `passers` that lands from its own place past the head; G867, each of
/// `held` ([`pass_over_held`]): another runner holds it, or its check reads what one changes.
fn passed_over_note(
    unread: &[&crate::queue::Entry],
    behind: &[throughput::Queued],
    passers: &[u64],
    held: &(Vec<u64>, Vec<u64>),
) -> String {
    let mut note = Vec::new();
    if !unread.is_empty() {
        let changes: Vec<&str> = unread.iter().map(|entry| entry.change.as_str()).collect();
        note.push(format!(
            "; passed over, not read here: {}",
            changes.join(", ")
        ));
    }
    let passing: Vec<&str> = behind
        .iter()
        .filter(|queued| passers.contains(&queued.seq))
        .map(|queued| queued.change.as_str())
        .collect();
    if !passing.is_empty() {
        note.push(format!(
            "; passed over, lands from its own place: {}",
            passing.join(", ")
        ));
    }
    for (places, why) in [
        (&held.0, "another runner holds it"),
        (&held.1, "its check reads what a held place changes"),
    ] {
        let changes: Vec<&str> = behind
            .iter()
            .filter(|queued| places.contains(&queued.seq))
            .map(|queued| queued.change.as_str())
            .collect();
        if !changes.is_empty() {
            note.push(format!("; passed over, {why}: {}", changes.join(", ")));
        }
    }
    note.concat()
}

/// Why no batch forms on `trunk` under `policy`, whatever the change: an unchecked policy, or
/// unsigned evidence the policy does not reuse. A pre-merge check keyed by its commit keeps out
/// only the members whose diff wakes it ([`throughput::plan_batch`], G560), and a post check
/// judges main's tip after a landing, never a batch's candidate, so its keying keeps no member
/// out (G420). The head and every speculating runner read this one rule, so they plan alike.
fn no_batch(ctx: &Ctx, policy: &policy::Policy, trunk: &str) -> Option<String> {
    if policy.unchecked {
        return Some("the policy is unchecked".to_owned());
    }
    (!ctx.reuse_trusted(trunk)).then(|| "the policy does not reuse unsigned evidence".to_owned())
}

/// How many members a batch is planned with: `max`, or fewer while recent checks fail
/// ([`throughput::sized_batch`] of the failure rate over the last
/// [`throughput::RATE_WINDOW`] outcomes). The head and every speculating runner size it alike,
/// so they plan the same batch.
fn batch_size(
    git: &Git,
    queue: &crate::queue::Queue,
    policy: &policy::Policy,
    max: usize,
) -> usize {
    throughput::sized_batch(lane_failure_rate(git, queue, policy, None), max)
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
/// first and then the bisection's prefixes, each its predicted tip, and pushes each one's
/// evidence as it finishes (G802), so a runner that stops mid-search keeps what it checked. A
/// prefix whose tree a search before this one already failed ([`failed_before`]) fails with no
/// run. Returns the search and what each check found; `None` when no worker answered.
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
    ctx.repo.fetch_evidence_once(&ctx.keys);
    while let Some(prefix) = search.next_prefix() {
        let tip = &members[prefix - 1];
        if failed_before(ctx, policy, &tip.tree)? {
            search.record(prefix, false);
            checks.push(json!({"members": prefix, "tree": tip.tree, "verdict": "product_failure"}));
            continue;
        }
        // G845: the lease names the members this check covers.
        let covered = members[..prefix].iter().map(|member| member.op_id.clone());
        names_batch(ctx, op_id, covered.collect());
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
        // G796: a batch main's move stopped is no batch; its head builds again on the new main.
        // G910: nor is one whose head another runner took.
        if ctx.stopped.borrow().is_some()
            || taken_over(ctx, op_id)
            || refusal
                .as_ref()
                .is_some_and(|refusal| refusal.reason == Some(Reason::CheckUnavailable))
        {
            return Ok(None);
        }
        search.record(prefix, refusal.is_none());
        checks.push(json!({"members": prefix, "tree": tip.tree, "verdict": verdict}));
        kill_point("before-batch-evidence-push");
        push_evidence(ctx.repo, &ctx.producer()?.0)?;
    }
    Ok(Some((search, checks)))
}

/// G802: whether a check `policy` requires already failed on `tree`, on any base, in a record
/// the policy counts: a batch's prefix that a search before this one found red, whose members
/// came back to the line once the prefix before them landed. Only a pass lands, so a failure read
/// here, never run again, lets nothing land that was not checked.
fn failed_before(ctx: &Ctx, policy: &policy::Policy, tree: &str) -> Result<bool, String> {
    let git = &ctx.repo.git;
    let counted = policy.answering(evidence::counted(
        git,
        evidence::records_for(git, tree)?,
        ctx.registry.as_ref(),
    )?);
    Ok(counted.iter().any(|record| {
        record.kind == evidence::KIND_CHECK
            && record.verdict == evidence::Verdict::ProductFailure
            && record.manifest_digest == policy.manifest_digest
            && record.policy_revision == policy.revision
            && policy.check(&record.check).is_some()
    }))
}

/// E59 (a fleet finding, 2026-10-30): the members of the checked prefix `landing` behind the
/// head join it in turn (claimed into `others`). A member that cannot join, such as one whose
/// owner sent a new revision while the batch was checked, used to end the batch: every claim
/// went back and the head ran alone, so the check and each join before it were lost. Now the
/// batch is cut to the members before it, which keep their claims; that prefix is checked
/// again as its own tree, and the members past the prefix that passes go back. The journal names the member and why. The member that could not join keeps its
/// place for its own turn. Returns the members that land, or `None` when fewer than two do
/// (every claim given back).
fn join_or_cut<'a>(
    ctx: &mut Ctx<'a>,
    (op_id, trunk, policy, id): (&str, &str, &policy::Policy, &str),
    landing: &[throughput::Member],
    others: &mut Vec<Ctx<'a>>,
    (checks, stopped): (&mut Vec<serde_json::Value>, &mut &'static str),
) -> Result<Option<usize>, String> {
    ctx.step.set("join");
    // G761: every member's claim goes in one atomic push; a lost lease joins them one by one.
    let mut cut = join_members(ctx, landing, trunk, others, PushMode::Defer);
    let mut owners: Vec<&mut Ctx> = others.iter_mut().collect();
    let flushed = push_owed(&mut owners)?;
    drop(owners);
    if flushed {
        for other in others.iter_mut() {
            other.push_mode = PushMode::Now;
        }
    } else {
        // Each dropped context takes its unpushed claim back off the local chain ref (G294).
        others.clear();
        cut = join_members(ctx, landing, trunk, others, PushMode::Now);
    }
    let Some((size, why)) = cut else {
        return Ok(Some(landing.len()));
    };
    eprintln!(
        "bay runner: {op_id} batch of {} cut to {size}: {} could not join ({why})",
        landing.len(),
        landing[size].change
    );
    *stopped = "not_joined";
    if size < 2 {
        give_back(others, landing, "a member could not join the batch");
        return Ok(None);
    }
    ctx.step.set("check");
    let again = match bisect_batch(ctx, op_id, trunk, policy, &landing[..size], id) {
        Ok(Some((search, more))) => {
            checks.extend(more);
            search.passed()
        }
        Ok(None) => 0,
        Err(error) => {
            give_back(others, landing, "the cut batch could not be checked");
            return Err(error);
        }
    };
    ctx.step.set("join");
    if again < 2 {
        // G796: main's move stopped the cut batch's check.
        let cause = if ctx.stopped.borrow().is_some() {
            "main moved under the batch"
        } else {
            "the cut batch did not pass its check"
        };
        give_back(others, landing, cause);
        return Ok(None);
    }
    let mut past = others.split_off(again - 1);
    give_back(
        &mut past,
        &landing[again - 1..],
        "the cut batch did not pass its check",
    );
    Ok(Some(again))
}

/// The members of `landing` after the head, joined in order ([`joined`]) into `others` until one
/// cannot join: where and why, or `None` when all joined. With [`PushMode::Defer`] each claim
/// waits for one atomic push ([`push_owed`]).
fn join_members<'a>(
    ctx: &Ctx<'a>,
    landing: &[throughput::Member],
    trunk: &str,
    others: &mut Vec<Ctx<'a>>,
    mode: PushMode,
) -> Option<(usize, String)> {
    for (at, member) in landing.iter().enumerate().skip(1) {
        match joined(ctx, member, trunk, mode) {
            Ok(Ok(other)) => others.push(other),
            Ok(Err(why)) => return Some((at, why)),
            Err(error) => return Some((at, format!("it could not be read: {error}"))),
        }
    }
    None
}

/// A batch member other than the head, loaded, verified and claimed by this executor (§ 9a),
/// or, when it cannot join, why: its request no longer verifies, it is a repair or waits for
/// a change not on trunk, or it is not this executor's to run.
fn joined<'a>(
    ctx: &Ctx<'a>,
    member: &throughput::Member,
    trunk: &str,
    mode: PushMode,
) -> Result<Result<Ctx<'a>, String>, String> {
    let mut other = Ctx::load(ctx.repo, ctx.step, &member.change)?;
    other.membership = Membership::Batch;
    other.push_mode = mode;
    other.registry.clone_from(&ctx.registry);
    let signer = other.executor_signer()?;
    other.chain.sign_with(signer);
    if verify_request(&mut other, &member.op_id)?.is_some() {
        return Ok(Err("its request does not verify".to_owned()));
    }
    // A repair is judged by its own policy alone, never in a batch (§ 7). G273: a member that
    // lands after a change not yet on trunk never joins one.
    if other.repair.is_some() {
        return Ok(Err("a repair runs alone".to_owned()));
    }
    if dependency_waiting(&other, &member.op_id, trunk)?.is_some() {
        return Ok(Err("it lands after a change not on main".to_owned()));
    }
    if let Some(refused) = claim(&mut other, &member.op_id, &member.revision, trunk)? {
        return Ok(Err(match refused.state.as_deref() {
            Some("superseded") => "its operation was superseded".to_owned(),
            Some("running") => "another runner holds it".to_owned(),
            Some(state) => format!("it is {state}"),
            None => "it was not claimed here".to_owned(),
        }));
    }
    Ok(Ok(other))
}

/// The claims of a batch's other members (`others`, for `members` after the head) go back, so
/// each runs as its own head with no lease to wait out (§ 9a).
fn give_back(others: &mut [Ctx], members: &[throughput::Member], cause: &str) {
    for (other, member) in others.iter_mut().zip(&members[1..]) {
        release_after_fault(other, &member.op_id, &member.revision, cause);
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
/// to the last member with every member's landed marker, intent marker, queue release and records
/// ([`batch_with_records`]). A push refused goes again without the records, each chain read
/// first, and each landing is derived from trunk; a refusal the host made, other than bay's own
/// judgement, counts as the batch's first push. `None` when a member is no longer this executor's: nothing was pushed.
fn land_batch<'a>(
    ctx: &mut Ctx<'a>,
    others: &mut [Ctx<'a>],
    members: &[throughput::Member],
    trunk: &str,
    id: &str,
) -> Result<Option<BayResult>, String> {
    let repo = ctx.repo;
    ctx.membership = Membership::Batch;
    let (built, anchors) = build_batch(ctx, others, members, trunk, id)?;
    if !persist_batch(ctx, others, members, &built, trunk, id)? {
        return Ok(None);
    }
    ctx.step.set("publish");
    let signer = ctx.executor_signer().ok().flatten();
    let first = match batch_with_records(ctx, others, members, &built, signer.as_ref())? {
        WithRecords::Landed(mut landings) => {
            drop(anchors);
            let head = landings.remove(0);
            let landed = landings
                .into_iter()
                .enumerate()
                .filter_map(|(index, landing)| landing.ok().map(|landing| (index + 1, landing)))
                .collect();
            tell_members(others, members, landed);
            return head.map(Some);
        }
        WithRecords::Unsent => None,
        WithRecords::Refused(error) => Some(error),
    };
    for (index, member) in members.iter().enumerate() {
        let owner = member_ctx(ctx, others, index);
        if still_owner(owner, &member.op_id, &member.revision)?.is_some() {
            return Ok(None);
        }
    }
    let pushes = batch_pushes(ctx, others, members, &built, signer.as_ref())?;
    let tip = &built[members.len() - 1].commit;
    let bare = Riding::default();
    let mut answer = if let Some(error) = first {
        Err(error)
    } else {
        kill_point("before-publish");
        publish_batch_push(repo, tip, &pushes, &bare)
    };
    // BACKLOG 83 (2026-10-28: the host refused one batch twice with main unmoved, and each
    // refusal gave the batch back to be built and checked again, 4.5 min each; the third push of
    // the same refs landed): a refusal on an unmoved main, other than bay's own judgement, is
    // pushed again as built, as a single change's is (B4), before the batch is given back.
    let mut again = 0;
    while let Err(error) = &answer
        && again < BATCH_REPUSH_LIMIT
        && !judged_by_bay(error)
        && repo.fetch_trunk().is_ok_and(|now| now == trunk)
    {
        again += 1;
        std::thread::sleep(HOOK_RETRY_WAIT);
        answer = publish_batch_push(repo, tip, &pushes, &bare);
    }
    // G881 (a fleet finding, 2026-11-24: the host took a batch of eight with its records and
    // answered the push with a fault; the head read its chain again, found it landed, and yielded
    // "main moved" over its own landing, so its record read admitted on a change main held): a
    // push the host took can answer a fault. Main holding the batch's tip is its landing, whatever
    // the push answered, and each member's landing is derived from main as after any other.
    if let Err(error) = answer
        && !main_holds(repo, tip)
    {
        return Err(format!("batch {id} was refused: {error}"));
    }
    kill_point("after-publish");
    drop(anchors);
    // G860: every member's landed record goes in one push ([`push_landed`]).
    ctx.push_mode = PushMode::Defer;
    for other in others.iter_mut() {
        other.push_mode = PushMode::Defer;
    }
    let mut landed = Vec::new();
    for (index, member) in members.iter().enumerate().skip(1) {
        let other = &mut others[index - 1];
        other.pushed_landing = Some(built[index].commit.clone());
        // A landing whose record could not be written now is derived from trunk on a later run.
        if let Ok(landing) =
            derive_landed(other, &member.op_id, &member.revision, &built[index].commit)
        {
            landed.push((index, landing));
        }
    }
    ctx.pushed_landing = Some(built[0].commit.clone());
    let head = derive_landed(
        ctx,
        &members[0].op_id,
        &members[0].revision,
        &built[0].commit,
    );
    push_landed(ctx, others);
    tell_members(others, members, landed);
    head.map(Some)
}

/// G865 (a fleet measurement, 2026-11-23: a batch of three read each member's chain again before
/// its publishing push, and pushed every member's records after it): a batch's records ride its
/// publishing push, as a single landing's do ([`land_with_records`]). Each member's chain,
/// recorded landed on the head this process last pushed and leased on it, and this executor's
/// log, with each member's terminal record, move in the one atomic push that moves trunk to the
/// batch's tip. Each chain's lease stands for the read of that member's chain ([`still_owner`]).
///
/// Each record is taken back off this box when the push does not land, so the caller reads each
/// chain again and pushes the batch without its records ([`land_batch`]). A refusal by bay's hook
/// may judge the records themselves (a host whose hook is older), so it is not a first push.
///
/// # Errors
///
/// A queue release cannot be written.
fn batch_with_records<'a>(
    ctx: &mut Ctx<'a>,
    others: &mut [Ctx<'a>],
    members: &[throughput::Member],
    built: &[Built],
    signer: Option<&Signer>,
) -> Result<WithRecords, String> {
    if records_apart() {
        return Ok(WithRecords::Unsent);
    }
    let repo = ctx.repo;
    let pushes = batch_pushes(ctx, others, members, built, signer)?;
    kill_point("before-publish");
    let mut marked = Vec::new();
    for (index, member) in members.iter().enumerate() {
        let owner = member_ctx(ctx, others, index);
        let Ok(mark) = mark_landed(owner, &member.op_id, &member.revision, &built[index].commit)
        else {
            take_all_back(ctx, others, marked);
            return Ok(WithRecords::Unsent);
        };
        marked.push(mark);
    }
    let mut riding = Riding::default();
    for index in 0..members.len() {
        riding.chain(member_ctx(ctx, others, index));
        riding.unmarked(member_ctx(ctx, others, index));
    }
    let leased = riding.lease(ctx, &members[0].op_id);
    let terminals: Vec<serde_json::Value> =
        marked.iter().map(|mark| mark.terminal.clone()).collect();
    let tip = &built[members.len() - 1].commit;
    let executor = ctx.executor_name();
    let answer = crate::log::ride(repo, &executor, signer, &terminals, |log| {
        riding.specs.push(log.to_owned());
        publish_batch_push(repo, tip, &pushes, &riding)
    });
    if !matches!(answer, Ok(Ok(_))) {
        take_all_back(ctx, others, marked);
        return Ok(match answer {
            Ok(Err(error)) if !crate::git::ref_moved(&error) && !judged_by_bay(&error) => {
                WithRecords::Refused(error)
            }
            _ => WithRecords::Unsent,
        });
    }
    kill_point("after-publish");
    if leased {
        lease_released(ctx);
    }
    for index in 0..members.len() {
        chain_pushed(member_ctx(ctx, others, index));
    }
    let mut landings = Vec::new();
    for (index, (member, mark)) in members.iter().zip(marked).enumerate() {
        landings.push(recorded(
            member_ctx(ctx, others, index),
            &member.op_id,
            &member.revision,
            &built[index].commit,
            &mark.behind,
        ));
    }
    Ok(WithRecords::Landed(landings))
}

/// What [`batch_with_records`] made of a batch's publishing push.
enum WithRecords {
    /// The push landed the batch: each member's landing, the head's first.
    Landed(Vec<Result<BayResult, String>>),
    /// No push the host judged for itself: the records go apart, a record could not be written, a
    /// ref moved under a lease, or bay's hook refused the push.
    Unsent,
    /// The host refused the push for a reason of its own, as it said: the batch's first push
    /// (BACKLOG 83).
    Refused(String),
}

/// A batch member's context: the head's, at `index` 0, or another member's.
fn member_ctx<'c, 'a>(
    ctx: &'c mut Ctx<'a>,
    others: &'c mut [Ctx<'a>],
    index: usize,
) -> &'c mut Ctx<'a> {
    if index == 0 {
        ctx
    } else {
        &mut others[index - 1]
    }
}

/// G865: the records [`mark_landed`] wrote for each member, taken back off this box.
fn take_all_back<'a>(ctx: &mut Ctx<'a>, others: &mut [Ctx<'a>], marked: Vec<Marked>) {
    for (index, mark) in marked.into_iter().enumerate() {
        taken_back(member_ctx(ctx, others, index), &mark.before, mark.record);
    }
}

/// Each member's part of the batch's publishing push ([`publish_batch_push`]): its queue
/// release and its intent claim, as its record holds them.
fn batch_pushes(
    ctx: &Ctx,
    others: &[Ctx],
    members: &[throughput::Member],
    built: &[Built],
    signer: Option<&Signer>,
) -> Result<Vec<BatchPush>, String> {
    let git = &ctx.repo.git;
    let mut pushes = Vec::new();
    for (index, member) in members.iter().enumerate() {
        let owner: &Ctx = if index == 0 { ctx } else { &others[index - 1] };
        let release =
            crate::queue::release_commit(git, signer, member.seq, &member.op_id, "published")?;
        pushes.push(BatchPush {
            change: member.change.clone(),
            commit: built[index].commit.clone(),
            release: (member.seq, release),
            claim: owner.record.claim.clone(),
        });
    }
    Ok(pushes)
}

/// G825: each other member's landing, kept for the run's answer, its deferred events told, and
/// its author woken once, as the head's is in `execute` (G307), after every record is written,
/// so a slow adapter holds no record back.
fn tell_members(others: &[Ctx], members: &[throughput::Member], landed: Vec<(usize, BayResult)>) {
    let woken: Vec<_> = landed
        .iter()
        .map(|(index, landing)| (*index, landing.decision))
        .collect();
    if let Ok(mut kept) = MEMBERS_LANDED.lock() {
        kept.extend(landed.into_iter().map(|(index, landing)| {
            (
                members[index].change.clone(),
                members[index].op_id.clone(),
                landing,
            )
        }));
    }
    for other in others {
        other.tell_deferred();
    }
    for (index, decision) in woken {
        let _ = landed_wake(&others[index - 1], &members[index].op_id, decision);
    }
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
    // G761: every member's checked state, the head's last, goes in one atomic push.
    ctx.push_mode = PushMode::Defer;
    for other in others.iter_mut() {
        other.push_mode = PushMode::Defer;
    }
    let written = persist_each(ctx, others, members, built, trunk, id);
    let flushed = if matches!(written, Ok(true)) {
        kill_point("before-batch-persist-push");
        let mut owners: Vec<&mut Ctx> = std::iter::once(&mut *ctx)
            .chain(others.iter_mut())
            .collect();
        push_owed(&mut owners)
    } else {
        Ok(false)
    };
    let fast = matches!(flushed, Ok(true));
    for owner in std::iter::once(&mut *ctx).chain(others.iter_mut()) {
        // A write the atomic push did not carry leaves this box's chain ref and the record
        // (G294), so no later push of this context carries it.
        if !fast && owner.push_mode == PushMode::Owed {
            owner.push_mode = PushMode::Now;
            owner.refetch()?;
        }
        owner.push_mode = PushMode::Now;
    }
    if fast {
        return Ok(true);
    }
    written?;
    flushed?;
    persist_each(ctx, others, members, built, trunk, id)
}

/// [`persist_batch`] member by member: each persisted by a push of its own, the head's last.
fn persist_each<'a>(
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
            (&attempt.commit, &message),
            false,
            |op| {
                op.attempts.push(attempt.clone());
                op.unavailable = None;
                op.state = OpState::Checked;
                op.outcome = None;
                op.blocked_on = None;
                op.checked_on = None;
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
        let commit = commit_candidate(owner, &member.tree, &parent, &message, &member.revision)?;
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
/// member, and creates every member's landed marker, intent marker and queue release, with what
/// rides it (G865). Never forced; each marker and release is create-only.
fn publish_batch_push(
    repo: &Repo,
    tip: &str,
    members: &[BatchPush],
    riding: &Riding,
) -> Result<(), String> {
    let mut leases = riding.leases.clone();
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
    refspecs.extend(riding.specs.iter().cloned());
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
    let step = Step::new("batch");
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

/// BACKLOG 216 (a fleet finding, 2026-11-02: cotffvzj): a host that refuses this box's push for
/// permission refuses each retry the same way, so a change's own run gives the operation to the
/// runners at once, with no retry counted, and says what the host said.
fn left_to_the_runners(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    error: &str,
) -> Result<BayResult, String> {
    let mut queued = enqueue(ctx, op_id, revision, Reason::Queued)?;
    if let Some(detail) = queued.detail.as_mut() {
        detail["yielded"] = json!(true);
        detail["host_said"] = json!(host_said(error));
        detail["recovery"] =
            json!("the host does not let this machine push main: a runner merges this change");
    }
    Ok(queued)
}

/// Whether a head whose place was released with `reason` while it published rejoins (§ 6):
/// after its progress deadline, or G691 (a fleet finding, 2026-11-09) after `unavailable`. A
/// runner whose box could not serve the head releases its place a push or two after recording
/// the wait, and a serving runner may take the head in between; its checks passed, so the wait
/// is over.
fn rejoins_mid_publish(reason: &str) -> bool {
    matches!(reason, "deadline" | crate::queue::UNAVAILABLE)
}

/// Another change landed this generation of the intent: never a retry (§ 6). The marker is
/// the ref or its shard index record (§ 10a); an unread remote is an error, never "absent".
/// BACKLOG 235: the generation may be one this change never won, whose merge is another
/// change's. That change superseded this one, whose work is not on main: never "already
/// merged", whose guidance says never to retry a change that has work still to land. `None`
/// when no merge holds the generation.
fn generation_landed(ctx: &Ctx, op_id: &str) -> Result<Option<BayResult>, String> {
    let Some(claim) = &ctx.record.claim else {
        return Ok(None);
    };
    let Some(landed) = crate::compact::remote_marker(ctx.repo, &claim.landed_ref())? else {
        return Ok(None);
    };
    let merged_by = crate::start::merged_change(ctx.repo, &landed, &claim.landed_ref())?;
    if merged_by != ctx.id {
        return Ok(Some(
            BayResult::blocked(Reason::ChangeSuperseded)
                .with_change(ctx.id.clone())
                .with_op(op_id)
                .with_detail(json!({
                    "landed_intent": claim.landed_ref(),
                    "superseded_by": merged_by,
                    "generation": claim.generation,
                    "why": format!("generation {} of the intent of change {} merged as change \
                        {merged_by}; the work of {} is not on main",
                        claim.generation, ctx.id, ctx.id),
                    "recovery": format!("bay start {} claims the next generation of its intent, \
                        then bay merge", ctx.id),
                })),
        ));
    }
    Ok(Some(
        BayResult::blocked(Reason::ChangeAlreadyLanded)
            .with_change(ctx.id.clone())
            .with_op(op_id)
            .with_detail(json!({"landed_intent": claim.landed_ref()})),
    ))
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
    if let Some(refused) = generation_landed(ctx, op_id)? {
        return Ok(Some(refused));
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
    // A release created while this head was publishing ([`rejoins_mid_publish`]), whether or not
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
    if let Some(reason) = &reason
        && !rejoins_mid_publish(reason)
    {
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
        if let Some(blocked) = uncounted_retry(ctx, op_id, revision, "a release mid-publish")? {
            return Ok(Some(blocked));
        }
        return enqueue(ctx, op_id, revision, Reason::Queued).map(Some);
    }
    // G128: another process on the remote holds a ref lock. Trunk did not move, so the same
    // candidate is pushed again after a short wait, uncounted; bay never unlinks the lock.
    if now == *trunk && held_lock(error) {
        return lock_held(ctx, op_id, revision);
    }
    // B4 (a fleet finding, 2026-10-26/27: GitHub refused a passed head's push with a bare
    // "(failed)" and with GH009, both cleared on a retry, and bay released the place as
    // `failed`): trunk did not move, so the host's refusal is a fault of the push, never of the
    // change. The same candidate is pushed again after a short wait, uncounted toward
    // `MAIN_MOVED`, and the head keeps its place; five end it as `MERGE_RETRY_EXHAUSTED`.
    if now == *trunk && ctx.visit == Visit::Own && refused_for_permission(error) {
        return left_to_the_runners(ctx, op_id, revision, error).map(Some);
    }
    if now == *trunk && !judged_by_bay(error) {
        if let Some(blocked) = uncounted_retry(
            ctx,
            op_id,
            revision,
            &format!(
                "the host refused the publishing push ({})",
                host_said(error)
            ),
        )? {
            return Ok(Some(blocked));
        }
        std::thread::sleep(HOOK_RETRY_WAIT);
        return Ok(None);
    }
    // G936: a runner that came while this operation ran free of an empty line, before this one
    // read its presence, gave it a place (`queue_gate`), so that what came meanwhile waits behind
    // it, and the host holds the line for it. It takes that place and pushes the same candidate
    // again, releasing it.
    if now == *trunk && takes_given_place(ctx.repo, op_id, seq, error)? {
        return Ok(None);
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
        stopped: false,
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
/// How many times a batch's push the host refused on an unmoved main is pushed again as built
/// before the batch is given back (BACKLOG 83).
const BATCH_REPUSH_LIMIT: u32 = 2;

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
/// says so; the next executor pass (G621, [`asked_again`]) or the change's next land reads trunk
/// again and finds the landing or builds anew, never a second publication (`DESIGN` § 9).
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
    /// G796: the runner saw the move while the candidate was checked, and stopped its checks.
    stopped: bool,
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
    ctx.op_mut(op_id).trunk_moved += 1;
    if let Some(at) = *seq
        && ctx.visit != Visit::Own
    {
        // G768 (a fleet measurement, 2026-11-17: a head that yielded "main moved" waited 277 s
        // at the median for its next pass, which then ran no check on the new main: of 33 such
        // merges in three hours, one ran a check after the move, and 7,301 s were that wait):
        // a head no pre-merge check covers on the new main is built again and published in
        // this visit, at most [`PASSES_PER_ENTRY`] times; a head a check must run for yields.
        // G796: so is a head whose check the move stopped, which lost no finished work.
        if ctx.op(op_id).trunk_moved <= PASSES_PER_ENTRY
            && (moved.stopped || needs_no_check(&ctx.repo.git, moved.trunk, revision)?)
        {
            return Ok(None);
        }
        return yield_head(ctx, op_id, revision, at, "main moved").map(Some);
    }
    // § 6: bay retries once more, then the operation joins the publication queue.
    if ctx.op(op_id).trunk_moved >= TRUNK_MOVED_LIMIT && seq.is_none() {
        return enqueue(ctx, op_id, revision, Reason::Queued).map(Some);
    }
    Ok(None)
}

/// G861: how often a check ahead of its turn asks the remote who holds the line's head.
const HEAD_PROBE: Duration = Duration::from_secs(30);

/// [`HEAD_PROBE`]; a test build honours `BAY_TEST_HEAD_PROBE_S`, a release build never does.
fn head_probe() -> Duration {
    if crate::version::TEST_HOOKS
        && let Some(secs) = std::env::var("BAY_TEST_HEAD_PROBE_S")
            .ok()
            .and_then(|secs| secs.parse().ok())
    {
        return Duration::from_secs(secs);
    }
    HEAD_PROBE
}

/// G861 (BACKLOG 248, fix A; a fleet finding, 2026-10-26: a head nobody held waited twenty
/// minutes while all three runners checked changes behind it): what a check ahead of its turn
/// watches of the line, from its beat. Each [`HEAD_PROBE`] it makes one `ls-remote` of the line
/// and the leases ([`crate::queue::remote_head_leased`]). When two probes in a row find a head
/// other than the one the check began behind, with no lease on it, nobody runs that head: every
/// check stops, and the runner takes the head between two operations (`crate::executor`). The
/// check never gives way to the head it began behind: the pass took that one then, or left it to
/// wait for what it waits for. Nor does it give way to its own change as the head: the check
/// is that head's own, and its record lands it.
struct HeadWatch {
    own: String,
    began: Option<u64>,
    free: Option<u64>,
    probed: Option<Instant>,
}

impl HeadWatch {
    fn new(git: &Git, own: &str) -> Self {
        Self {
            own: own.to_owned(),
            began: crate::queue::Queue::local(git)
                .ok()
                .and_then(|queue| queue.head().map(|head| head.seq)),
            free: None,
            probed: None,
        }
    }

    /// The change of the head the check gives way to, once it stopped every check; `None` while
    /// the check goes on.
    fn gives_way(&mut self, repo: &Repo, halt: &check::Halt) -> Option<String> {
        if halt.moved() || self.probed.is_some_and(|at| at.elapsed() < head_probe()) {
            return None;
        }
        self.probed = Some(Instant::now());
        let Some((seq, change, leased)) = crate::queue::remote_head_leased(repo) else {
            self.free = None;
            return None;
        };
        if leased || self.began == Some(seq) || change == self.own {
            self.free = None;
            return None;
        }
        if self.free != Some(seq) {
            self.free = Some(seq);
            return None;
        }
        halt.stop_all_but(Vec::new());
        Some(change)
    }
}

/// G796 (a fleet measurement, 2026-11-19: in one day 124 heads ran 5.5 h of checks on
/// candidates main had already moved past, learning it only when the merging push was refused;
/// one 4-minute check went on 224 s after a change that passed its head landed 28 s into it):
/// one beat's look at main, while a candidate merged onto `trunk` is checked. One `ls-remote`
/// of main; only once it moved, one fetch of main, which reads what the move changed. Then every
/// check stops but those whose record a candidate on the new main carries ([`carried_across`]),
/// and the new tip is returned. `None` while main stands, once the checks already stopped, or
/// when the remote cannot be read.
fn watch_main(
    ctx: &Ctx,
    (policy, reusing): (&policy::Policy, bool),
    trunk: &str,
    halt: &check::Halt,
) -> Option<String> {
    if halt.moved() {
        return None;
    }
    let main = format!("refs/heads/{}", ctx.repo.trunk());
    let listed = ctx
        .repo
        .git
        .run(&["ls-remote", ctx.repo.remote(), &main])
        .ok()?;
    let tip = listed
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .find_map(|(tip, name)| (name == main).then_some(tip))?;
    if tip == trunk {
        return None;
    }
    let now = ctx.repo.refetch_trunk().ok().filter(|now| now != trunk)?;
    // A main that was not built on the candidate's base changed everything it reads.
    let changed = ctx
        .repo
        .git
        .is_ancestor(trunk, &now)
        .unwrap_or(false)
        .then(|| check::changed_paths(&ctx.repo.git, trunk, &now).ok())
        .flatten();
    let carried = match changed {
        Some(changed) if reusing => carried_across(ctx, policy, &changed),
        _ => Vec::new(),
    };
    halt.stop_all_but(carried);
    Some(now)
}

/// G796: the checks of `policy` whose record on a candidate a move of main changing `changed`
/// may leave standing: a candidate on the new main carries it byte for byte ([`carry_over`]). A
/// check that is keyed by its commit is never carried; one with an observed footprint may be,
/// unless the move changes a path G703 predicts it reads (its `paths`, or `when_paths`, read in
/// order as [`throughput::list_reads`] reads a list, so a `!` exclusion holds), and one
/// that declares neither is read only once it ends, so it runs on; one with a declared read set
/// is when footprint lanes are on and the move changes none of it.
fn carried_across(
    ctx: &Ctx,
    policy: &policy::Policy,
    changed: &[String],
) -> Vec<evidence::CheckId> {
    policy
        .checks
        .iter()
        .filter(|entry| {
            let reads = match entry.footprint {
                policy::FootprintMode::Observed => Some(
                    entry
                        .paths
                        .as_deref()
                        .or(entry.when_paths.as_deref())
                        .unwrap_or_default(),
                ),
                policy::FootprintMode::Declared if ctx.repo.config.footprint_lanes() => {
                    entry.read_set()
                }
                _ => None,
            };
            entry.tree_keyed()
                && reads.is_some_and(|reads| {
                    !changed
                        .iter()
                        .any(|path| throughput::list_reads(reads, path))
                })
        })
        .map(|entry| evidence::CheckId::new(&entry.id, &entry.environment))
        .collect()
}

/// G796: after main's move stopped the checks of a candidate (or of a head's batch) of
/// `op_id`, what follows, as for a merging push main's move refused ([`after_trunk_moved`]):
/// `None` builds again on the new main, now `trunk`, in this visit; a head builds again at most
/// [`PASSES_PER_ENTRY`] times, as it lost no finished work, then yields. One that builds again
/// from its place records the move on its chain. The checks that finished on the stopped
/// candidate carry onto the next one when their reads stand.
fn after_stopped_check(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    trunk: &mut String,
    seq: &mut Option<u64>,
) -> Result<Option<BayResult>, String> {
    let Some(stopped) = ctx.stopped.take() else {
        return Ok(None);
    };
    ctx.stopped_trees.push(stopped.tree);
    let base = std::mem::replace(trunk, stopped.main);
    ctx.record.observed_remote.clone_from(trunk);
    let moved = Moved {
        base: &base,
        trunk,
        held: false,
        stopped: true,
    };
    let next = after_trunk_moved(ctx, op_id, revision, seq, &moved)?;
    // G552, G577, G791: an operation that builds again from its place says on its chain that
    // main moved under it, so the changes that could pass it wait now, never one per check.
    if next.is_none()
        && let Some(at) = *seq
    {
        let short = trunk.get(..10).unwrap_or(trunk);
        let message = format!("main moved under {op_id} at {at}: built again on {short}");
        if ctx.save_cas(revision, None, &message)? {
            ctx.push_chain();
        }
    }
    Ok(next)
}

/// F11 (a fleet finding, 2026-10-25: one head rebuilt twelve times in one pass while main moved
/// outside bay, and no other change was given a place for an hour): the head gives its claim
/// back and keeps its place, so the runner's pass goes on to every other change. The next pass
/// runs it again, as the head, with a batch of the places behind it when `batch` is set.
fn yield_head(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    at: u64,
    cause: &str,
) -> Result<BayResult, String> {
    let op = ctx.op_mut(op_id);
    op.state = OpState::Admitted;
    op.owner = None;
    op.outcome = Some(Reason::Queued.code().to_owned());
    ctx.save(revision, None, &format!("yielded {op_id} at {at}: {cause}"))?;
    ctx.push_chain();
    let mut result = BayResult::new(Decision::Queued)
        .with_change(ctx.id.clone())
        .with_op(op_id)
        .with_detail(json!({"yielded": true, "seq": at}));
    result.waiting_for = Some("executor".to_owned());
    Ok(result)
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
fn queue_gate(ctx: &mut Ctx, op_id: &str, trunk: &str) -> Result<Gate, String> {
    let signer = ctx.executor_signer().ok().flatten();
    let mut queue = crate::queue::Queue::read_shared(ctx.repo)?;
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
    // BACKLOG 326: an operation that asks an expedite and holds no place takes one right behind
    // the head, before a newer revision would take its change's older place.
    if queue.of(op_id).is_none() && expedite(ctx, op_id, trunk, &queue)? {
        queue = crate::queue::Queue::read(ctx.repo)?;
    }
    if queue.of(op_id).is_none()
        && let Some(older) = queue.of_change(&ctx.id).cloned()
    {
        // G567: a place that names a newer operation of the change is that one's. An older
        // operation, read from a chain behind the queue, neither takes it nor joins behind it.
        if crate::queue::newer(&older.op_id, op_id) {
            let op = ctx.op(op_id);
            return Ok(Gate::Wait(Box::new(superseded(
                &ctx.id,
                op_id,
                op.epoch,
                op.owner.as_ref(),
            ))));
        }
        // A newer revision takes its entry's place, the head's too (the trust-agents ruling,
        // 2026-10-26: a head revised to repair it was sent behind 120 changes). A head revised
        // again and again is still bounded by its deadline releases.
        // A place released since the read is not replaced (G424): this revision joins behind.
        let _ = crate::queue::replace(ctx.repo, signer.as_ref(), &older, op_id)?;
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
    let head = match queue.head().cloned() {
        Some(head) => head,
        None if !races_free(ctx) => return unless_red(ctx, op_id, trunk, Gate::Free),
        // G936: with another runner live, an operation takes a place even in an empty line, so
        // a change that comes while it runs waits behind it instead of publishing beside it.
        None => {
            crate::queue::join(ctx.repo, signer.as_ref(), op_id, &ctx.id)?;
            queue = crate::queue::Queue::read(ctx.repo)?;
            queue
                .head()
                .cloned()
                .ok_or("the queue reservation vanished")?
        }
    };
    if head.op_id == op_id {
        return unless_red(ctx, op_id, trunk, Gate::Head(head.seq));
    }
    // Repair first: while main is red, a change that names the red's owning change (`Fixes:`,
    // `Reverts:`, `Supersedes:`) runs as a head from its own place, ahead of the line it
    // unblocks. Main's state chooses it, never a priority; once main is green, the line is in
    // arrival order again. BACKLOG 119: so does a change past places that main's red holds,
    // which wait in arrival order.
    // G552: a change that needs no check runs from its own place too, unless main moved under
    // the head once already: then it keeps its place until that head leaves the head.
    // BACKLOG 139: neither it nor a change past the red's places runs from its own place while
    // main moved under a running repair: it keeps its place until that repair leaves the line.
    // A repair with no place yet takes one and runs from it in this visit, never a pass later.
    let repairs = repairs_red(ctx, trunk)?;
    ctx.batch_unnamed.set(false);
    if repairs && queue.of(op_id).is_none() {
        crate::queue::join(ctx.repo, signer.as_ref(), op_id, &ctx.id)?;
        queue = crate::queue::Queue::read(ctx.repo)?;
    }
    if let Some(entry) = queue.of(op_id).cloned()
        && (repairs
            || (needs_no_check(&ctx.repo.git, trunk, &ctx.op(op_id).revision)?
                && !queue.expedite_ahead(entry.seq)
                && !main_moved_under(ctx.repo, &head)
                && !head_check_long(ctx.repo, &head)
                && !repair_moved_under(ctx, &queue, trunk, op_id)?)
            || (!queue.expedite_ahead(entry.seq)
                && passes_fresh(ctx, &queue, entry.seq, trunk, &ctx.op(op_id).revision)?
                && !head_check_long(ctx.repo, &head)
                && !repair_moved_under(ctx, &queue, trunk, op_id)?))
    {
        return unless_red(ctx, op_id, trunk, Gate::Head(entry.seq));
    }
    let revision = ctx.op(op_id).revision.clone();
    let mut queued = enqueue(ctx, op_id, &revision, Reason::Queued)?;
    // G867: a looping runner visits this wait again at the holder's next beat, which names its
    // batch, not only once the line or main moves (`executor::Parked`).
    if ctx.batch_unnamed.take()
        && let Some(detail) = queued.detail.as_mut()
    {
        detail["batch_unnamed"] = json!(true);
    }
    Ok(Gate::Wait(Box::new(queued)))
}

/// G936 (BACKLOG 349; a load test, 2026-11-27: with the line empty, two runners each published
/// a different change free on one main; the second landing push was refused and its change
/// checked again, twelve times in a row in one minute, each runner losing to the other): whether
/// another runner's presence this store holds reads live, so that a publication free of the line
/// here would race its own on main.
fn races_free(ctx: &Ctx) -> bool {
    let me = crate::presence::presence_ref(&ctx.executor_name());
    crate::presence::serving_until(&ctx.repo.git, &me, &[]).is_some()
}

/// G936: whether `error`, the host's refusal of a free publishing push while the line is held,
/// was for the place another runner gave `op_id` meanwhile. `seq` takes that place then.
fn takes_given_place(
    repo: &Repo,
    op_id: &str,
    seq: &mut Option<u64>,
    error: &str,
) -> Result<bool, String> {
    if seq.is_some() || !error.contains(crate::hook::QUEUE_HELD_BY) {
        return Ok(false);
    }
    *seq = placed_meanwhile(repo, op_id)?;
    Ok(seq.is_some())
}

/// G936: the place another runner gave `op_id` while it ran free, when that place heads the line
/// on the remote now.
fn placed_meanwhile(repo: &Repo, op_id: &str) -> Result<Option<u64>, String> {
    let queue = crate::queue::Queue::read(repo)?;
    Ok(queue
        .head()
        .filter(|head| head.op_id == op_id)
        .map(|head| head.seq))
}

/// BACKLOG 326: grants the expedite `op_id`'s request asks, when the operation never held a place:
/// a place right behind the head (`crate::queue::Grant`), within `[expedite]`'s limits, and gives
/// back the older place of its change. Says whether the operation took a place. An ask it does not
/// need (no line, its change holds the head, it needs no check, or it repairs a red main) is let
/// go; one the limits refuse is logged, and the operation joins as a plain arrival.
fn expedite(
    ctx: &Ctx,
    op_id: &str,
    trunk: &str,
    queue: &crate::queue::Queue,
) -> Result<bool, String> {
    use crate::expedite::Decided;
    let git = &ctx.repo.git;
    let request = ctx.op(op_id).request_id.clone().unwrap_or_default();
    let Some(why) = crate::expedite::asked(git, &request) else {
        return Ok(false);
    };
    let Some(head) = queue.head().cloned() else {
        return Ok(false);
    };
    if head.change == ctx.id
        || queue.entries.values().any(|entry| entry.op_id == op_id)
        || needs_no_check(git, trunk, &ctx.op(op_id).revision)?
        || repairs_red(ctx, trunk)?
    {
        return Ok(false);
    }
    let per_day = ctx.repo.config.expedite_per_day();
    let now = crate::live::unix_now();
    let by = ctx.executor_name();
    let actor = ctx.record.actor.name.clone();
    let signer = ctx.executor_signer().ok().flatten();
    let mut decided = Decided {
        change: &ctx.id,
        op_id,
        actor: &actor,
        why: &why,
        by: &by,
        refused: crate::expedite::refusal(per_day, queue, now),
        place: None,
        after: None,
    };
    let mut took = false;
    if decided.refused.is_none() {
        let grant = crate::queue::Grant {
            rank: head.rank,
            after: head.seq,
            expedite: crate::expedite::grant_entry(&why, &actor, &request, &by),
        };
        // A head that moved on before the push leaves the grant nothing to stand behind: the
        // operation joins as a plain arrival, at its turn.
        if crate::queue::join_as(ctx.repo, signer.as_ref(), op_id, &ctx.id, Some(&grant)).is_err() {
            return Ok(false);
        }
        took = true;
        let line = crate::queue::Queue::read(ctx.repo)?;
        let Some(own) = line.of(op_id).cloned().filter(|own| own.expedite) else {
            return Ok(true);
        };
        // Two runners that grant at once both push; the earlier place keeps its grant.
        decided.refused = line.over(own.seq, per_day.unwrap_or(0), now);
        if decided.refused.is_some() {
            crate::queue::release(
                ctx.repo,
                signer.as_ref(),
                own.seq,
                op_id,
                crate::queue::EXPEDITE_OVER,
            )?;
            took = false;
        } else {
            decided.place = Some(own.seq);
            decided.after = Some(head.seq);
            if let Some(older) = line
                .of_change(&ctx.id)
                .filter(|older| older.seq != own.seq)
                .cloned()
            {
                crate::queue::release(
                    ctx.repo,
                    signer.as_ref(),
                    older.seq,
                    &older.op_id,
                    "superseded",
                )?;
            }
        }
    }
    let event = crate::expedite::event(&decided)?;
    crate::log::append_event(ctx.repo, &by, signer.as_ref(), &event)?;
    Ok(took)
}

/// G867: [`passes_unread`], once this store's chains say the operation at `place` passes, read
/// again on the chains and leases of the entries ahead as the remote holds them now: a claim
/// made since this pass fetched them, or a lease that named its batch since, decides it.
fn passes_fresh(
    ctx: &Ctx,
    queue: &crate::queue::Queue,
    place: u64,
    trunk: &str,
    revision: &str,
) -> Result<bool, String> {
    if !passes_unread(ctx, queue, place, trunk, revision)? {
        return Ok(false);
    }
    let changes: Vec<String> = queue
        .ahead_of(place)
        .iter()
        .map(|entry| entry.change.clone())
        .collect();
    let _ = ctx.repo.remote_chains(&changes);
    passes_unread(ctx, queue, place, trunk, revision)
}

/// How often main may move under one queue entry before no change passes it any more (G703):
/// each move costs that entry a rebuild of its candidate, and a check only where its record is
/// not carried.
const PASSES_PER_ENTRY: u32 = 3;

/// G703 (owner ruling 2026-11-12; a fleet finding the same day: changes to notes waited 213 to
/// 395 s behind a code change's check that never read them): whether the operation at queue
/// place `place`, of `revision`, runs from its own place, ahead of every live entry before it.
/// It does when evidence reuse is on, it changes no `.bay/` path, and for each live entry ahead
/// the pre-merge checks that entry requires read, as far as the policy at `trunk` says, none of
/// its paths, and main moved under that entry fewer than [`PASSES_PER_ENTRY`] times. A check's
/// reads are its changed files and `paths` for `materialize = "changed"`, else for
/// `footprint = "observed"`, whose record is carried across an unrelated merge byte for byte,
/// what its records on main near `trunk` read and listed (G907), or with none its `paths` (or
/// `when_paths`). Any other check, or one that reads history, is read as reading everything:
/// footprint lanes (`footprint = true`) keep their own order, a pre-check and queue order (G257).
///
/// It is a prediction of cost, never of correctness: the entry passed rebuilds its candidate
/// on the new main and lands only on records of that candidate, carried byte for byte or run
/// again.
fn passes_unread(
    ctx: &Ctx,
    queue: &crate::queue::Queue,
    place: u64,
    trunk: &str,
    revision: &str,
) -> Result<bool, String> {
    let git = &ctx.repo.git;
    if !ctx.repo.config.evidence_reuse() {
        return Ok(false);
    }
    let Ok(Some(policy)) = policy::load_at(git, trunk) else {
        return Ok(false);
    };
    if policy.unchecked || policy.checks.is_empty() {
        return Ok(false);
    }
    let Some(ours) = own_change(git, trunk, revision)? else {
        return Ok(false);
    };
    let ahead = queue.ahead_of(place);
    if ahead.is_empty() {
        return Ok(false);
    }
    let me = ctx.executor_name();
    for entry in ahead {
        let Some(queued) = throughput::Queued::read(git, entry) else {
            return Ok(false);
        };
        if !queued.live {
            continue;
        }
        if moves_under(ctx.repo, queue, entry) >= PASSES_PER_ENTRY {
            return Ok(false);
        }
        // G846: an entry its holder checks in a batch reads what the whole batch changes.
        let holder = (queued.holder.as_deref(), me.as_str());
        let Some(batch) = batch_behind(ctx.repo, queue, entry, holder) else {
            ctx.batch_unnamed
                .set(held_batch(git, &entry.change, &entry.op_id).is_none());
            return Ok(false);
        };
        let revisions = std::iter::once(queued.revision.as_str())
            .chain(batch.iter().map(|member| member.revision.as_str()));
        if !reads_none_of(git, &policy, trunk, revisions, &ours)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether the pre-merge checks of the changes at `revisions` on `trunk` read none of what
/// `ours` changes, as far as `policy` and their records near main say (see [`passes_unread`]):
/// false when one of them changes nothing or a `.bay/` path, or one of the checks may read
/// anything.
fn reads_none_of<'r>(
    git: &Git,
    policy: &policy::Policy,
    trunk: &str,
    revisions: impl IntoIterator<Item = &'r str>,
    ours: &throughput::Own,
) -> Result<bool, String> {
    let mut theirs = Vec::new();
    for revision in revisions {
        let Some(paths) = own_paths(git, trunk, revision)? else {
            return Ok(false);
        };
        theirs.extend(paths);
    }
    let Some(reads) = predicted_reads(git, policy, trunk, &theirs) else {
        return Ok(false);
    };
    Ok(!reads.meets(ours))
}

/// The paths `revision` changes from where it meets `trunk`; `None` when it changes none, or a
/// `.bay/` path (a policy edit passes nothing).
fn own_paths(git: &Git, trunk: &str, revision: &str) -> Result<Option<Vec<String>>, String> {
    let Ok(base) = git.run(&["merge-base", trunk, revision]) else {
        return Ok(None);
    };
    let changed = check::changed_paths(git, &base, revision)?;
    if changed.is_empty() || changed.iter().any(|path| path.starts_with(".bay/")) {
        return Ok(None);
    }
    Ok(Some(changed))
}

/// G907: [`own_paths`] of `revision` with the directories whose entry names it changes on
/// `trunk` ([`throughput::Own`]); `None` as for [`own_paths`].
fn own_change(git: &Git, trunk: &str, revision: &str) -> Result<Option<throughput::Own>, String> {
    let Ok(base) = git.run(&["merge-base", trunk, revision]) else {
        return Ok(None);
    };
    let own = throughput::Own::of(git, (&base, revision), trunk)?;
    if own.paths.is_empty() || own.paths.iter().any(|path| path.starts_with(".bay/")) {
        return Ok(None);
    }
    Ok(Some(own))
}

/// What the pre-merge checks a change of `changed` requires under `policy` read; `None` when
/// one of them may read anything (see [`passes_unread`]). G907: an observed check with records
/// on main near `trunk` reads what they read and listed, exactly, and the changed paths; one
/// with none reads its `paths` (or `when_paths`) in order, as [`throughput::list_reads`] reads
/// a list (G500: a `!` exclusion holds).
fn predicted_reads(
    git: &Git,
    policy: &policy::Policy,
    trunk: &str,
    changed: &[String],
) -> Option<throughput::Reads> {
    let (required, _) = policy.required(changed);
    let mut reads = throughput::Reads::default();
    for entry in required {
        let id = crate::evidence::CheckId::new(&entry.id, &entry.environment);
        let check = policy.check(&id)?;
        if check.stage == policy::Stage::Post {
            continue;
        }
        if check.reads_history() {
            return None;
        }
        if check.changed_only() {
            reads.globs.extend(changed.iter().cloned());
            reads.lists.extend(check.paths.iter().cloned());
            continue;
        }
        if check.footprint != policy::FootprintMode::Observed {
            return None;
        }
        if let Some(seen) = throughput::observed_near(git, trunk, &id) {
            reads.globs.extend(changed.iter().cloned());
            reads.observed.push(seen);
            continue;
        }
        reads
            .lists
            .push(check.paths.as_ref().or(check.when_paths.as_ref())?.clone());
    }
    Some(reads)
}

/// How often a publishing push of queue place `entry`'s operation was refused because main
/// moved, as this box last fetched its chain.
fn moves_under(repo: &Repo, queue: &crate::queue::Queue, entry: &crate::queue::Entry) -> u32 {
    let yielded = Chain::new(&repo.git)
        .read(&entry.change)
        .ok()
        .and_then(|(_, record)| {
            record
                .operations
                .iter()
                .find(|op| op.op_id == entry.op_id)
                .map(|op| op.trunk_moved)
        })
        .unwrap_or(0);
    yielded.max(passed(queue, entry))
}

/// G753 (a fleet finding, 2026-11-16: four changes merged past one head in 23 minutes while it
/// checked and then waited for another box's runner, and it yielded once: the cap counted its
/// yields only): how many changes landed past `entry` while it was live, read from the queue:
/// each later place released `published`. A head that checks, or waits for a runner of another
/// environment, counts every change that passed it, not only those it yielded to.
fn passed(queue: &crate::queue::Queue, entry: &crate::queue::Entry) -> u32 {
    let count = queue
        .release_reasons
        .range(entry.seq + 1..)
        .filter(|(_, reason)| reason.as_str() == "published")
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// BACKLOG 119 (a fleet finding, 2026-10-28: zynsylhi was claimed eight times in half an hour
/// and queued again at the back each time, at places 1687 to 1720; rlkohjff eleven times in 25
/// minutes): `gate`, unless main's red holds the operation ([`red_gate`]), which then waits in
/// its place before any runner claims it.
fn unless_red(ctx: &mut Ctx, op_id: &str, trunk: &str, gate: Gate) -> Result<Gate, String> {
    let revision = ctx.op(op_id).revision.clone();
    red_gate(ctx, &revision, trunk)?;
    Ok(gate)
}

/// G791: how long the line's head may have been checked before no change passes it any more: a
/// change that lands past it then costs it at most this much work done again.
pub const PASSER_HOLD_S: u64 = 120;

/// G791 (owner ruling 2026-11-19, "hold passers on long checks"; fleet findings: overnight 21 of
/// 207 landings were checked again because main moved, nearly all 10-minute heads checked two
/// to four times while changes that passed them landed; the next morning one 18-minute head
/// ran three times, and 13 changes waited behind it): whether the queue's `head` is under a long
/// check, read from its change chain as this box last fetched it. It is when its operation runs
/// and has run for [`PASSER_HOLD_S`] or longer since its claim (the chain's newest commit:
/// beats keep their own ref, G787), or main already moved under it once. A change that could
/// pass the head (G552, G703) then waits its turn and lands right after it, so the head's
/// checked result stays the one it merges. A repair of a red main still runs from its place.
fn head_check_long(repo: &Repo, head: &crate::queue::Entry) -> bool {
    let chain = Chain::new(&repo.git);
    let Ok((tip, record)) = chain.read(&head.change) else {
        return false;
    };
    let Some(op) = record.operations.iter().find(|op| op.op_id == head.op_id) else {
        return false;
    };
    if !matches!(op.state, OpState::Running | OpState::Checked) {
        return false;
    }
    if op.trunk_moved > 0 {
        return true;
    }
    // A test build reads the window from the clone's `bay.testPasserHoldS`, so a test need not
    // wait two minutes.
    let hold = TEST_HOOKS
        .then(|| {
            repo.git
                .run(&["config", "--get", "bay.testPasserHoldS"])
                .ok()
        })
        .flatten()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(PASSER_HOLD_S);
    repo.git
        .run(&["log", "-1", "--format=%ct", &tip])
        .ok()
        .and_then(|at| at.trim().parse::<u64>().ok())
        .is_some_and(|at| crate::live::unix_now().saturating_sub(at) >= hold)
}

/// G552 (a fleet finding, 2026-10-29: one head yielded "main moved" pass after pass while
/// changes that need no check landed past it, each one under its batch's check): whether a
/// publishing push of the queue's `head` was refused because main moved, read from its change
/// chain as this box last fetched it. A change that needs no check then waits its turn, so one
/// move costs the head one check, never one per change behind it. A head that waits on a repair
/// (`Blocked`) holds nothing.
fn main_moved_under(repo: &Repo, head: &crate::queue::Entry) -> bool {
    moved_under(repo, head).is_some()
}

/// The change record of queue place `entry`, as this box last fetched its chain, when a
/// publishing push of its operation was refused because main moved and the operation still runs.
fn moved_under(repo: &Repo, entry: &crate::queue::Entry) -> Option<ChangeRecord> {
    let (_, record) = Chain::new(&repo.git).read(&entry.change).ok()?;
    record
        .operations
        .iter()
        .any(|op| {
            op.op_id == entry.op_id
                && op.trunk_moved > 0
                && matches!(
                    op.state,
                    OpState::Admitted | OpState::Running | OpState::Checked
                )
        })
        .then_some(record)
}

/// BACKLOG 139 (a fleet finding, 2026-10-30: the repair 3t7jrfsw of a red post check was refused
/// "main moved" on four tries, while documents and changes past the red's places landed under
/// it; the head, waiting on the red, held none of them): whether main moved under a running
/// repair of a red post check on `trunk` at a live place other than `op_id`'s. A change that
/// needs no check, or passes the red's places, then waits its turn, as G552 makes it wait for the
/// head ([`main_moved_under`]), so one move costs the repair one check.
fn repair_moved_under(
    ctx: &Ctx,
    queue: &crate::queue::Queue,
    trunk: &str,
    op_id: &str,
) -> Result<bool, String> {
    let git = &ctx.repo.git;
    if journaled(ctx, || {
        crate::red::holding(git, trunk, ctx.registry.as_ref())
    })?
    .is_empty()
    {
        return Ok(false);
    }
    for entry in queue.ahead_of(u64::MAX) {
        if entry.op_id == op_id {
            continue;
        }
        if let Some(record) = moved_under(ctx.repo, entry)
            && crate::red::repairs(git, trunk, ctx.registry.as_ref(), &record.repair_names())?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Nothing to check, nothing to wait (owner, 2026-10-26: a document must reach main at once):
/// whether no pre-merge check of `trunk`'s policy is required for `revision`'s changes, so it
/// can change no other change's result. A change to `.bay/`, an unreadable policy, or a policy
/// that checks nothing at all is never this case.
///
/// # Errors
///
/// A git fault while listing the change's paths.
pub fn needs_no_check(git: &Git, trunk: &str, revision: &str) -> Result<bool, String> {
    let Ok(Some(policy)) = policy::load_at(git, trunk) else {
        return Ok(false);
    };
    if policy.unchecked || policy.checks.is_empty() {
        return Ok(false);
    }
    let Ok(base) = git.run(&["merge-base", trunk, revision]) else {
        return Ok(false);
    };
    let changed = check::changed_paths(git, &base, revision)?;
    if changed.is_empty() || changed.iter().any(|path| path.starts_with(".bay/")) {
        return Ok(false);
    }
    let (required, _) = policy.required(&changed);
    Ok(required.iter().all(|entry| {
        let id = crate::evidence::CheckId::new(&entry.id, &entry.environment);
        policy
            .check(&id)
            .is_some_and(|check| check.stage == policy::Stage::Post)
    }))
}

/// G666: whether a runner has nothing to check of this change, so its own merge may push it: no
/// pre-merge check covers it, or the policy at `trunk` has no checks (`unchecked = true`). A change
/// to `.bay/` still goes to a runner.
fn nothing_to_check(git: &Git, trunk: &str, revision: &str) -> Result<bool, String> {
    if needs_no_check(git, trunk, revision)? {
        return Ok(true);
    }
    let Ok(Some(policy)) = policy::load_at(git, trunk) else {
        return Ok(false);
    };
    let Ok(base) = git.run(&["merge-base", trunk, revision]) else {
        return Ok(false);
    };
    let changed = check::changed_paths(git, &base, revision)?;
    Ok(policy.unchecked
        && !changed.is_empty()
        && !changed.iter().any(|path| path.starts_with(".bay/")))
}

/// Whether this change names, by `Fixes:`, `Reverts:` or `Supersedes:`, what answers a red post
/// check on `trunk` (G244, `crate::red::Red::answered_by`).
fn repairs_red(ctx: &Ctx, trunk: &str) -> Result<bool, String> {
    let names = ctx.record.repair_names();
    if names.is_empty() {
        return Ok(false);
    }
    // The post records on the remote decide the red: fetched now, only for a change that names
    // a repair, so a runner never judges a repair on a stale reading.
    ctx.repo.fetch_evidence(&ctx.keys);
    journaled(ctx, || {
        crate::red::repairs(&ctx.repo.git, trunk, ctx.registry.as_ref(), &names)
    })
}

/// How long a judgement of main's red takes before a runner's journal names it (G592).
const SLOW_RED_S: u64 = 60;

/// `judge`, a judgement of main's red, and the journal line of the judgement it made when that
/// took [`SLOW_RED_S`] or more (G592): what it read, beside how many others, and how long its
/// thread waited for a CPU. Read beside the operation's own line, it says whether the judgement
/// took the `red` step's time. A test build reads the bound from `BAY_TEST_SLOW_RED_S`.
fn journaled<T>(ctx: &Ctx, judge: impl FnOnce() -> T) -> T {
    let _ = crate::red::last_cost();
    let out = judge();
    let slow = std::env::var("BAY_TEST_SLOW_RED_S")
        .ok()
        .filter(|_| TEST_HOOKS)
        .and_then(|seconds| seconds.parse().ok())
        .unwrap_or(SLOW_RED_S);
    if let Some(cost) = crate::red::last_cost().filter(|cost| cost.seconds >= slow) {
        eprintln!("bay runner: {} {}", ctx.id, cost.said());
    }
    out
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
    let mut queue = crate::queue::Queue::read_shared(ctx.repo)?;
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
    let head = queue.head().cloned();
    // BACKLOG 291: a place behind another names who holds that head, as its lease in this store
    // says, so a reader of the runner's line can tell a line that moves from one that is stuck.
    let head_holder = head
        .as_ref()
        .filter(|head| head.op_id != op_id)
        .and_then(|head| {
            let tip = crate::lease::tip(&ctx.repo.git, &head.change)?;
            let lease = crate::lease::read(&ctx.repo.git, &tip).ok().flatten()?;
            (lease.op_id == head.op_id).then_some(lease.holder)
        });
    let mut result = BayResult::refused(Decision::Queued, reason)
        .with_change(ctx.id.clone())
        .with_op(op_id)
        .with_detail(json!({
            "position": queue.position(entry.seq),
            "seq": entry.seq,
            "head": head.map(|head| head.op_id),
            "head_holder": head_holder,
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
    if result.decision != Decision::Queued || (repo.ahead() == 0 && !repo.config.footprint_lanes())
    {
        return result;
    }
    ctx.step.set("ahead");
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
    // With batching on, runners check whole batches ahead of their turn, and screen alone an
    // entry no planned check covers (G861).
    if repo.config.batch().is_some() {
        return match speculate_batch(ctx, op_id, trunk)? {
            Ahead::Checked(checked) => Ok(Some(checked)),
            Ahead::Leave => Ok(None),
            Ahead::Open => screen(ctx, op_id, trunk),
        };
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
    let (kind, base) = if ahead.len() <= repo.ahead() {
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
    // G779: a check is spent only on main as it stands now; a moved main is read next pass.
    if !repo.trunk_still(trunk).map_err(|error| error.message())? {
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
    let Some(verdict) = observed(&ran?, ctx.yielded.take()) else {
        return Ok(None);
    };
    push_evidence(repo, &ctx.producer()?.0)?;
    let recorded = Precheck {
        base: base.clone(),
        base_tree,
        tree,
        verdict: verdict.to_owned(),
        at_ms: now_ms(),
        screen: false,
    };
    let shown = shown_precheck(kind, &recorded);
    keep_precheck(ctx, op_id, recorded);
    ctx.save(&revision, None, &format!("prechecked {op_id} on {base}"))?;
    ctx.push_chain();
    Ok(Some(shown))
}

/// G861: the verdict a check ahead of its turn observed, `ran`: none when it gave way to a free
/// head (`yielded`) or no worker answered it, so nothing is recorded.
fn observed(
    (verdict, refusal): &(&'static str, Option<BayResult>),
    yielded: bool,
) -> Option<&'static str> {
    let unanswered = refusal
        .as_ref()
        .is_some_and(|refusal| refusal.reason == Some(Reason::CheckUnavailable));
    (!yielded && !unanswered).then_some(*verdict)
}

/// The `detail.precheck` a pass shows of `precheck`, a check ahead of its turn `on` a
/// predicted base, the tip, or alone (a screen).
fn shown_precheck(on: &str, precheck: &Precheck) -> serde_json::Value {
    json!({
        "on": on,
        "base": precheck.base,
        "base_tree": precheck.base_tree,
        "tree": precheck.tree,
        "verdict": precheck.verdict,
    })
}

/// Speculative batches (owner, 2026-10-27: "go for speculative batch pipelining"; a fleet
/// finding the same night: one batch was checked at a time, on a check of under two minutes,
/// while runners spent about half their time pre-checking single entries the batch would check
/// again). From the trunk tip, the batches the heads will form are planned one after another,
/// each on the tip the one before it predicts ([`throughput::plan_batch`], the very plan the
/// head makes, footprint lanes included). When `op_id` leads one of them other than the first,
/// the first being the head's own (G845: the one its holder's lease names, when a holder checks
/// it), that batch's combined tree is checked now on its predicted base, by this executor's
/// producer, as the head will check it (`bisect_batch`: a tip naming no change). When its turn
/// comes and trunk is what was predicted, the head's plan is the same members on the same base,
/// so its tree is the same, and exact reuse lands it with no new run. A miss is a record about
/// another tree, never trusted. A tree already checked on its base is not run again.
///
/// [`Ahead::Leave`] when a planned check covers `op_id` (the head's own batch, or a later one
/// whose check did not fail), batching is off at this trunk, by a rule or by the failure rate, a
/// policy change ends the prediction, or main moved. [`Ahead::Open`] when no planned check
/// covers it (G861): the plan ends at `[runner] ahead` entries before it, it leads a batch
/// smaller than `[runner] batch` lets form other than the one right behind the head's (G936: that
/// one is checked as it is, a batch of one included), or the check of its batch failed.
fn speculate_batch(ctx: &mut Ctx, op_id: &str, trunk: &str) -> Result<Ahead, String> {
    let repo = ctx.repo;
    let git = &repo.git;
    let Some(bounds) = repo.config.batch() else {
        return Ok(Ahead::Leave);
    };
    let Ok(Some(at_trunk)) = policy::load_at(git, trunk) else {
        return Ok(Ahead::Leave);
    };
    if no_batch(ctx, &at_trunk, trunk).is_some() {
        return Ok(Ahead::Leave);
    }
    let queue = crate::queue::Queue::local(git)?;
    if lane_failure_rate(git, &queue, &at_trunk, None) > bounds.max_failure_rate {
        return Ok(Ahead::Leave);
    }
    // G581: a place whose operation does not read here is passed over, never the end of the
    // line checked ahead; the batch decision names it.
    let places: Vec<&crate::queue::Entry> = queue
        .entries
        .values()
        .filter(|entry| !queue.released.contains(&entry.seq))
        .collect();
    let (line, _) = read_line_fetched(repo, &places);
    // G845: with `[runner] footprint_lanes`, a batch ends at the first member out of its head's
    // lane, as the head plans it (`batch_plan`); a batch planned past it is a tree no head forms.
    let by_lane = repo.config.footprint_lanes();
    let mut base = trunk.to_owned();
    let mut covered = 0;
    let mut at = 0;
    let mut first = true;
    let mut planned = 0;
    let reach = repo.ahead();
    while at < line.len() && covered <= reach {
        let head = &line[at];
        if !head.live {
            at += 1;
            continue;
        }
        // G845: the head's own batch is the one its holder checks, as its lease names it: a head
        // checked alone is a batch of one, whatever joined the line since. With no lease naming
        // one, it is the batch the head will plan.
        let named = if first {
            held_batch(git, &head.change, &head.op_id)
        } else {
            None
        };
        let (members, stopped, _) = if let Some(named) = named {
            let behind: Vec<throughput::Queued> = line[at + 1..]
                .iter()
                .filter(|queued| named.contains(&queued.op_id))
                .cloned()
                .collect();
            throughput::plan_batch(git, &at_trunk, &base, head, &behind, named.len(), by_lane)?
        } else {
            throughput::plan_batch(
                git,
                &at_trunk,
                &base,
                head,
                &line[at + 1..],
                batch_size(git, &queue, &at_trunk, bounds.max),
                by_lane,
            )?
        };
        let Some(last) = members.last() else {
            // Its head does not fold onto the predicted tip: a policy change ends the
            // prediction; a conflict or no effect is predicted not to land.
            if stopped == "policy" {
                return Ok(Ahead::Leave);
            }
            at += 1;
            continue;
        };
        if members.iter().any(|member| member.op_id == op_id) {
            // The head's own batch: its holder checks it.
            if first {
                return Ok(Ahead::Leave);
            }
            // G861: a later batch whose check ahead of its turn failed covers none of its
            // members. Each is screened alone, and the batch planned again passes over the one
            // that fails on its own.
            let leader = &members[0];
            let base_tree = tree_of(git, &base)?;
            let failed = batch_failed(ctx, (&leader.change, &leader.op_id), &base_tree, &last.tree);
            if leader.op_id != op_id {
                return Ok(if failed { Ahead::Open } else { Ahead::Leave });
            }
            // G936 (BACKLOG 349): an operation alone right behind the head's batch is checked
            // as a batch of one on the tip the head predicts, not alone on trunk: once the head
            // lands, its turn is the same tree on the same base, and exact reuse lands it.
            let next_to_head = planned == 1;
            if failed || (members.len() < bounds.min.max(2) && !next_to_head) {
                return Ok(Ahead::Open);
            }
            let (commit, tree) = (last.commit.clone(), last.tree.clone());
            // G779: a check is spent only on main as it stands now; a moved main is read next pass.
            if !repo.trunk_still(trunk).map_err(|error| error.message())? {
                return Ok(Ahead::Leave);
            }
            return checked_ahead(ctx, op_id, &base, &commit, &tree, members.len())
                .map(Ahead::Checked);
        }
        let last_seq = last.seq;
        base.clone_from(&last.commit);
        covered += members.len();
        first = false;
        planned += 1;
        at = line
            .iter()
            .position(|queued| queued.seq == last_seq)
            .map_or(line.len(), |position| position + 1);
    }
    Ok(Ahead::Open)
}

/// G861: what [`speculate_batch`] finds for an operation that waits behind the head.
enum Ahead {
    /// It leads a batch past the head's, and that batch's check ahead of its turn ran, now or
    /// before.
    Checked(serde_json::Value),
    /// A planned check covers it, or no check is worth making now.
    Leave,
    /// No planned check covers it: it is screened ([`screen`]).
    Open,
}

/// G861: how long past `[runner] execution_budget_s` a screen's `screening` mark stands for the
/// runner that wrote it. Past that, the runner is taken to have stopped, and another one screens
/// the operation.
const SCREEN_MARK_SLACK_MS: u64 = 10 * 60 * 1000;

/// G861 (BACKLOG 248, fix A; a fleet measurement, 2026-11-20: 28 red batches were bisected one
/// check after another, and the line's head was held 10.3 hours in all): an operation behind the
/// head that no planned check covers is checked alone, merged onto the trunk tip, ahead of its
/// turn. This is its screen. A change that fails on its own is then known before a batch holds
/// it: [`throughput::plan_batch`] passes it over, and as the head it runs alone. Its author is
/// woken while it waits ([`screen_failed_wake`]).
///
/// One runner screens an operation. The chain carries the runner's mark (`screening`) before the
/// check starts, pushed by compare-and-swap. A runner that reads the mark leaves the operation
/// alone, and a runner whose push of the mark loses leaves it too. A screen that gave way to a
/// free head, or that no worker answered, takes its mark back and records nothing.
///
/// `None` when the operation is the head, the policy at the tip checks nothing or its records
/// stand for no run there, its merge onto the tip conflicts or has no effect, it needs no screen
/// now ([`screen_due`]), main moved, or another runner's write to the chain won.
fn screen(ctx: &mut Ctx, op_id: &str, trunk: &str) -> Result<Option<serde_json::Value>, String> {
    let repo = ctx.repo;
    let git = &repo.git;
    let queue = crate::queue::Queue::local(git)?;
    if queue.head().is_none_or(|head| head.op_id == op_id) || !ctx.reuse_trusted(trunk) {
        return Ok(None);
    }
    let Ok(Some(policy)) = policy::load_at(git, trunk) else {
        return Ok(None);
    };
    if policy.unchecked {
        return Ok(None);
    }
    let revision = ctx.op(op_id).revision.clone();
    let throughput::Fold::Applied { commit, tree, .. } =
        throughput::fold(git, &policy, trunk, &revision)?
    else {
        return Ok(None);
    };
    let base_tree = tree_of(git, trunk)?;
    if !screen_due(
        ctx.op(op_id),
        &base_tree,
        now_ms(),
        screen_mark_stands(repo),
    ) {
        return Ok(None);
    }
    // G779: a check is spent only on main as it stands now; a moved main is read next pass.
    if !repo.trunk_still(trunk).map_err(|error| error.message())? {
        return Ok(None);
    }
    let marked = now_ms();
    let screen_of = |verdict: &str, at_ms: u64| Precheck {
        base: trunk.to_owned(),
        base_tree: base_tree.clone(),
        tree: tree.clone(),
        verdict: verdict.to_owned(),
        at_ms,
        screen: true,
    };
    keep_precheck(ctx, op_id, screen_of(throughput::SCREENING, marked));
    ctx.save(
        &revision,
        None,
        &format!("screens {op_id} alone on {trunk}"),
    )?;
    if !ctx.push_chain() {
        return Ok(None);
    }
    let candidate_id = format!("screen-{op_id}-{marked}");
    let facts = change_facts(ctx, &revision);
    ctx.ahead_of_turn = true;
    let ran = run_policy(
        ctx,
        op_id,
        &candidate_id,
        (commit.as_str(), tree.as_str(), Some(&facts)),
        trunk,
        &policy,
        trunk,
        &[],
    );
    ctx.ahead_of_turn = false;
    let yielded = ctx.yielded.take();
    let verdict = ran.as_ref().ok().and_then(|ran| observed(ran, yielded));
    let op = ctx.op_mut(op_id);
    op.prechecks
        .retain(|precheck| !(precheck.screen && precheck.at_ms == marked));
    let Some(verdict) = verdict else {
        // Nothing was observed: the mark is taken back, so a later pass screens it.
        ctx.save(&revision, None, &format!("took back the screen of {op_id}"))?;
        ctx.push_chain();
        let why = match ran {
            Err(error) => return Err(error),
            Ok(_) if yielded => "it gave way to the head",
            Ok(_) => "no worker answered",
        };
        return Ok(Some(json!({"on": "screen", "unchecked": why})));
    };
    push_evidence(repo, &ctx.producer()?.0)?;
    let recorded = screen_of(verdict, now_ms());
    let shown = shown_precheck("screen", &recorded);
    keep_precheck(ctx, op_id, recorded);
    ctx.save(
        &revision,
        None,
        &format!("screened {op_id} alone on {trunk}: {verdict}"),
    )?;
    ctx.push_chain();
    if verdict == throughput::PRODUCT_FAILURE
        && let Err(why) = screen_failed_wake(ctx, op_id, &tree)
    {
        eprintln!("bay runner: {op_id} failed its screen, and its author was not woken: {why}");
    }
    Ok(Some(shown))
}

/// G861: how long a screen's `screening` mark stands: `[runner] execution_budget_s` and
/// [`SCREEN_MARK_SLACK_MS`].
fn screen_mark_stands(repo: &Repo) -> u64 {
    let budget = repo.config.execution_budget().map_or(u64::MAX, |budget| {
        u64::try_from(budget.as_millis()).unwrap_or(u64::MAX)
    });
    budget.saturating_add(SCREEN_MARK_SLACK_MS)
}

/// G861: whether operation `op` is screened on a trunk of `base_tree` now. An operation is
/// screened once: a screen that passed or failed stands, and a runner's mark stands for
/// `stands` milliseconds from `at_ms`. A screen that judged nothing (a check's machinery
/// failed) is made again on another trunk, and one whose runner stopped is made again.
fn screen_due(op: &Operation, base_tree: &str, now: u64, stands: u64) -> bool {
    let Some(newest) = op.prechecks.iter().rev().find(|precheck| precheck.screen) else {
        return true;
    };
    match newest.verdict.as_str() {
        "pass" | throughput::PRODUCT_FAILURE => false,
        throughput::SCREENING => now.saturating_sub(newest.at_ms) > stands,
        _ => newest.base_tree != base_tree,
    }
}

/// Records `precheck` on operation `op_id`, newest last, keeping
/// [`throughput::PRECHECKS_KEPT`].
fn keep_precheck(ctx: &mut Ctx, op_id: &str, precheck: Precheck) {
    let op = ctx.op_mut(op_id);
    op.prechecks.push(precheck);
    let excess = op
        .prechecks
        .len()
        .saturating_sub(throughput::PRECHECKS_KEPT);
    if excess > 0 {
        op.prechecks = op.prechecks.split_off(excess);
    }
}

/// G861: whether the check ahead of its turn of the batch the operation `op_id` of `change`
/// leads, `tree` on a base of `base_tree`, found a product failure, as the leader's chain here
/// records it.
fn batch_failed(ctx: &Ctx, (change, op_id): (&str, &str), base_tree: &str, tree: &str) -> bool {
    let failed = |prechecks: &[Precheck]| {
        prechecks.iter().any(|precheck| {
            !precheck.screen
                && precheck.base_tree == base_tree
                && precheck.tree == tree
                && precheck.verdict == throughput::PRODUCT_FAILURE
        })
    };
    if change == ctx.id {
        return ctx.find_op(op_id).is_some_and(|op| failed(&op.prechecks));
    }
    Chain::new(&ctx.repo.git)
        .read(change)
        .ok()
        .and_then(|(_, record)| record.operations.into_iter().find(|op| op.op_id == op_id))
        .is_some_and(|op| failed(&op.prechecks))
}

/// G845: the operations the holder of operation `op_id` of `change` checks now, that one first,
/// as its lease names them in this store (`crate::lease::batch_of`); `None` when no holder's
/// lease names one.
fn held_batch(git: &Git, change: &str, op_id: &str) -> Option<Vec<String>> {
    let (_, record) = Chain::new(git).read(change).ok()?;
    let op = record.operations.iter().find(|op| op.op_id == op_id)?;
    crate::lease::batch_of(git, change, op)
}

/// G846 (a fleet finding, 2026-11-21: a code change passed a head whose own paths no check of
/// the code change reads, but the head was checked in a batch of nine that held code changes;
/// its landing stopped that batch's 18-minute check, and the next head was passed the same way):
/// the live entries other than `entry` that its holder's check covers, as its lease names them
/// (G845), read from `queue`: empty when no holder's lease names a batch, `None` when one of
/// them cannot be read.
///
/// G867 (a fleet finding, 2026-11-23: a passer decided 18 s after another runner claimed the
/// head, before that runner's lease named its batch, and the batch it passed was cut when it
/// landed): `None` too while `holder`, a runner other than `me`, holds the entry and its lease
/// names no batch yet. The holder names one before its check starts, alone or in a batch.
fn batch_behind(
    repo: &Repo,
    queue: &crate::queue::Queue,
    entry: &crate::queue::Entry,
    (holder, me): (Option<&str>, &str),
) -> Option<Vec<throughput::Queued>> {
    let Some(members) = held_batch(&repo.git, &entry.change, &entry.op_id) else {
        return holder.is_none_or(|holder| holder == me).then(Vec::new);
    };
    members
        .iter()
        .skip(1)
        .filter_map(|member| queue.of(member))
        .map(|place| throughput::Queued::read(&repo.git, place))
        .collect()
}

/// Checks the speculative batch led by `op_id`, of `size` members, whose combined tip is
/// `commit` with `tree` on the predicted `base`, and records it in the operation's
/// `prechecks`; a tree already checked on that base is not run again.
fn checked_ahead(
    ctx: &mut Ctx,
    op_id: &str,
    base: &str,
    commit: &str,
    tree: &str,
    size: usize,
) -> Result<serde_json::Value, String> {
    let repo = ctx.repo;
    let git = &repo.git;
    let base_tree = tree_of(git, base)?;
    let done = |precheck: &Precheck| precheck.tree == tree && precheck.base_tree == base_tree;
    if ctx.op(op_id).prechecks.iter().any(done) {
        return Ok(json!({"on": "batch", "size": size, "tree": tree, "checked": "before"}));
    }
    let Ok(Some(policy)) = policy::load_at(git, base) else {
        return Ok(json!({"on": "batch", "size": size, "unchecked": "no policy at the base"}));
    };
    let candidate_id = format!("spec-{op_id}-{}", now_ms());
    ctx.ahead_of_turn = true;
    // As the head checks a batch's tip (`bisect_batch`): a tip naming no one change.
    let ran = run_policy(
        ctx,
        op_id,
        &candidate_id,
        (commit, tree, None),
        base,
        &policy,
        base,
        &[],
    );
    ctx.ahead_of_turn = false;
    let (verdict, refusal) = ran?;
    if ctx.yielded.take() {
        return Ok(json!({"on": "batch", "size": size, "unchecked": "it gave way to the head"}));
    }
    if refusal
        .as_ref()
        .is_some_and(|refusal| refusal.reason == Some(Reason::CheckUnavailable))
    {
        return Ok(json!({"on": "batch", "size": size, "unchecked": "no worker answered"}));
    }
    push_evidence(repo, &ctx.producer()?.0)?;
    let revision = ctx.op(op_id).revision.clone();
    let op = ctx.op_mut(op_id);
    op.prechecks.push(Precheck {
        base: base.to_owned(),
        base_tree: base_tree.clone(),
        tree: tree.to_owned(),
        verdict: verdict.to_owned(),
        at_ms: now_ms(),
        screen: false,
    });
    let excess = op
        .prechecks
        .len()
        .saturating_sub(throughput::PRECHECKS_KEPT);
    if excess > 0 {
        op.prechecks = op.prechecks.split_off(excess);
    }
    ctx.save(
        &revision,
        None,
        &format!("checked the batch of {size} it leads ahead of its turn, on {base}"),
    )?;
    ctx.push_chain();
    Ok(json!({
        "on": "batch",
        "size": size,
        "base": base,
        "base_tree": base_tree,
        "tree": tree,
        "verdict": verdict,
    }))
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
    let step = Step::new("probe");
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
    let paths = conflict.as_ref().map(|found| found.paths.clone());
    ctx.op_mut(op_id).early_conflict = conflict;
    ctx.save(&revision, None, &message)?;
    let pushed = ctx.push_chain();
    if pushed && let Some(paths) = paths {
        let _ = early_conflict_wake(&ctx, op_id, &paths);
    }
    Ok(pushed)
}

/// G649: a conflict found while a change waits in line wakes its author once for those paths, as
/// a block does (G307), so a session that left the change to the runners can repair it before
/// its turn. The answer is a `wake` event on this executor's log, whose id is the operation and
/// the paths: the same conflict never wakes twice, however often a pass notes it. Without a
/// `[plugin.wake] command` nothing is asked and nothing is written.
fn early_conflict_wake(ctx: &Ctx, op_id: &str, paths: &[String]) -> Result<(), String> {
    if !crate::talk::wake::configured(ctx.repo) {
        return Ok(());
    }
    let id = digest(&json!({"op_id": op_id, "outcome": "early_conflict", "paths": paths}))?;
    let id = format!("evt_{}", &id.trim_start_matches("sha256:")[..20]);
    if crate::log::holds_event(ctx.repo, &id)? {
        return Ok(());
    }
    let reason = Reason::MergeConflict;
    let wake = wake_author(ctx, op_id, Decision::Blocked, Some(reason));
    let event = json!({
        "schema": crate::log::EVENT_SCHEMA,
        "id": id,
        "at": crate::clock::now_rfc3339(),
        "kind": "wake",
        "change": ctx.id,
        "op_id": op_id,
        "decision": Decision::Blocked.word(),
        "reason": reason.code(),
        "conflicts": paths,
        "actor": ctx.record.actor,
        "wake": wake,
        "recurrence_key": format!("wake/{op_id}/early-conflict"),
    });
    let executor = ctx.executor_name();
    let signer = ctx.executor_signer().ok().flatten();
    crate::log::append(ctx.repo, &executor, signer.as_ref(), &event).map(|_| ())
}

/// G861: a screen that found a product failure wakes the change's author once for that tree, as
/// an early conflict does (G649), so a session that left the change to the runners can repair it
/// while it waits. The change keeps its place: it is refused only at its own turn. The answer is
/// a `wake` event on this executor's log, whose id is the operation and the tree. Without a
/// `[plugin.wake] command` nothing is asked and nothing is written.
fn screen_failed_wake(ctx: &Ctx, op_id: &str, tree: &str) -> Result<(), String> {
    if !crate::talk::wake::configured(ctx.repo) {
        return Ok(());
    }
    let id = digest(&json!({"op_id": op_id, "outcome": "screen_failed", "tree": tree}))?;
    let id = format!("evt_{}", &id.trim_start_matches("sha256:")[..20]);
    if crate::log::holds_event(ctx.repo, &id)? {
        return Ok(());
    }
    let reason = Reason::CheckFailed;
    let wake = wake_author(ctx, op_id, Decision::Queued, Some(reason));
    let event = json!({
        "schema": crate::log::EVENT_SCHEMA,
        "id": id,
        "at": crate::clock::now_rfc3339(),
        "kind": "wake",
        "change": ctx.id,
        "op_id": op_id,
        "decision": Decision::Queued.word(),
        "reason": reason.code(),
        "screen": {"tree": tree, "verdict": throughput::PRODUCT_FAILURE},
        "actor": ctx.record.actor,
        "wake": wake,
        "recurrence_key": format!("wake/{op_id}/screen"),
    });
    let executor = ctx.executor_name();
    let signer = ctx.executor_signer().ok().flatten();
    crate::log::append(ctx.repo, &executor, signer.as_ref(), &event).map(|_| ())
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
    // A lease watched out here is the claim's time, never the fetch's.
    ctx.step.set("claim");
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
    if ctx.find_op(op_id).is_none() {
        return Err(CHAIN_MOVED.to_owned());
    }
    // G693 (a fleet finding, 2026-11-10: three runners that did not serve a change's environment
    // took it twelve times in fifteen minutes, a minute of each runner's time apiece): the visit
    // judged the wait on the chain the pass had read, which a claim made since showed without
    // one, since each claim ends the wait until its holder records it again (G690). The claim
    // judges it again on the chain it just read.
    if let Some(waiting) = waits_for_worker(ctx, op_id, revision, trunk) {
        return Ok(Some(waiting));
    }
    let Some(op) = ctx.find_op(op_id) else {
        return Err(CHAIN_MOVED.to_owned());
    };
    // An operation waiting for a worker holds no lease against a box that serves it: its holder
    // recorded that it cannot run it and writes nothing more, so this executor, its own holder
    // or another, takes it at once (§ 7). A box that does not serve it passed it over already
    // (`waits_for_worker`). G425 (a fleet finding, 2026-10-28): each serving runner watched a
    // full lease, ten minutes, before taking over a head another runner had left waiting, and
    // ran nothing meanwhile. The claim raises the epoch by compare-and-swap, so two serving
    // runners never both hold it.
    let waiting = op.unavailable.is_some();
    let held =
        op.owner.is_some() && matches!(op.state, OpState::Running | OpState::Checked) && !waiting;
    // Another process holds the operation: take it over only once its lease expired, on this
    // process's own clock. A restarted supervisor watches a full lease again. A holder whose
    // executor key the registry revoked holds no lease (§ 10a).
    let quiet = holder_lease(
        &ctx.repo.config,
        ctx.find_op(op_id).and_then(|op| op.owner.as_deref()),
    );
    ctx.claim_waited = held;
    // G809: a held operation the chain says checked, while the line released it `published`,
    // landed: never held, and never claimed again.
    if held {
        match after_release(ctx, op_id)? {
            AfterRelease::Landed(landed) => return Ok(Some(*landed)),
            AfterRelease::Unanswered => return Err(CHAIN_MOVED.to_owned()),
            AfterRelease::Stands => {}
        }
    }
    if held && !holder_revoked(ctx, op_id)? && !lease_expired(ctx, quiet) {
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
        let frozen = frozen(ctx, op_id);
        let Some(op) = ctx.find_op_mut(op_id) else {
            return Err(CHAIN_MOVED.to_owned());
        };
        if op.state.is_terminal() && !frozen {
            return Ok(Some(superseded(&id, op_id, op.epoch, op.owner.as_ref())));
        }
        // G600 (a fleet finding, 2026-10-31: four runners claimed and checked a failed operation
        // again on one main, one after another): a pass visits an operation it listed while
        // another runner ran it, and that runner blocked it meanwhile. A blocked operation is
        // claimed only once its re-arm reached the remote ([`retry_blocked`]).
        if op.state == OpState::Blocked {
            return Ok(Some(blocked_elsewhere(&id, op)));
        }
        let read = op.epoch;
        op.epoch += 1;
        op.owner = Some(name.clone());
        // G690: the claim ends a wait for a worker; this holder runs it under its lease, and a
        // holder that cannot records the wait again. Kept, it let every serving runner take
        // the operation from the one running its check.
        op.unavailable = None;
        if op.state != OpState::Checked {
            op.state = OpState::Running;
        }
        let epoch = op.epoch;
        let message = format!("claim {op_id} epoch {epoch} by {name} (process {nonce})");
        let before = ctx.head.clone();
        if ctx.save_cas(revision, None, &message)? {
            kill_point("before-claim-push");
            if ctx.membership == Membership::Batch {
                kill_point("before-member-claim-push");
            }
            if ctx.push_chain() {
                ctx.my_epoch = epoch;
                *ctx.lease.get_mut() = Some(crate::lease::Holding::new(crate::lease::Lease::new(
                    &id, op_id, epoch, &name,
                )));
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
    // G760 (a gate finding, 2026-11-16): a batch member its author revised is not ended for its
    // batch's claim ([`frozen`]); a claim that lost its push to the author's write, or to this
    // runner placing the newer revision, claims it again instead of cutting the batch.
    let ended = (op.state.is_terminal() && !frozen(ctx, op_id)) || op.state == OpState::Blocked;
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

/// G910 (a fleet finding, 2026-11-25: on one host in 30 h, runners checked a batch head for 10 to
/// 36 minutes after another runner took its claim, then claimed four to eleven members, found the
/// head gone only at its persist, and gave every member back, while the head's new holder waited
/// for them): whether a beat of this visit's lease on `op_id` found a later holder's.
fn taken_over(ctx: &Ctx, op_id: &str) -> bool {
    ctx.lease
        .borrow()
        .as_ref()
        .is_some_and(|lease| lease.of(op_id) && lease.taken())
}

/// G910: the result of a visit whose claim of `op_id` another runner took ([`taken_over`]), as
/// [`still_owner`] reads it from the remote, or waiting for that runner while the remote cannot
/// be read; `None` while the claim is still this visit's.
fn taken_from(ctx: &mut Ctx, op_id: &str, revision: &str) -> Result<Option<BayResult>, String> {
    if !taken_over(ctx, op_id) {
        return Ok(None);
    }
    if let Some(read) = still_owner(ctx, op_id, revision)? {
        return Ok(Some(read));
    }
    let mut waiting = BayResult::new(Decision::Queued)
        .with_change(ctx.id.clone())
        .with_op(op_id);
    waiting.waiting_for = Some("executor".to_owned());
    Ok(Some(waiting))
}

/// Before the publishing push: the executor reads the chain from the remote, and if its epoch
/// is no longer current it never pushes (§ 9a, G24). It reports the existing publication, or
/// that it was superseded.
fn still_owner(ctx: &mut Ctx, op_id: &str, revision: &str) -> Result<Option<BayResult>, String> {
    if ctx.refetch().is_err() {
        return Ok(None);
    }
    // An operation the remote chain lacks is no one's to publish.
    let frozen = frozen(ctx, op_id);
    let (epoch, owner) = match ctx.find_op(op_id) {
        Some(op) if op.epoch == ctx.my_epoch && (op.state != OpState::Superseded || frozen) => {
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

/// G754: whether a context's operation runs alone or as a member of a batch in its check.
/// G761: when a context's chain push goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PushMode {
    /// At once, on its own (every context but a batch's fast path).
    Now,
    /// With the batch's other members, in one atomic push ([`push_owed`]).
    Defer,
    /// Deferred, and a write waits for that push.
    Owed,
}

/// G761 (a fleet measurement, 2026-11-16: a batch of twenty made 41 pushes, 160 s, to land, each
/// member claimed and persisted by a push of its own): pushes every owed chain of `owners`, with
/// the evidence notes any of them carries, in one `git push --atomic`, each chain leased on the
/// head this process last pushed or read. `true` when the remote took them all; then each is
/// pushed, as [`Ctx::push_chain`] leaves it. `false` when any lease was lost: nothing moved, and
/// the caller goes member by member.
fn push_owed(owners: &mut [&mut Ctx]) -> Result<bool, String> {
    let owed: Vec<usize> = (0..owners.len())
        .filter(|&index| owners[index].push_mode == PushMode::Owed)
        .collect();
    let Some(&first) = owed.first() else {
        return Ok(true);
    };
    let repo = owners[first].repo;
    let mut producers: Vec<String> = owed
        .iter()
        .filter_map(|&index| owners[index].evidence_rides.clone())
        .collect();
    producers.sort();
    producers.dedup();
    let writing = (!producers.is_empty()).then(evidence::writing);
    let mut leases = Vec::new();
    let mut specs = Vec::new();
    for &index in &owed {
        let owner = &owners[index];
        let name = change_ref(&owner.id);
        if let Some(expected) = &owner.pushed_head {
            leases.push(format!("--force-with-lease={name}:{expected}"));
        }
        specs.push(format!("{}:{name}", owner.head));
    }
    for producer in &producers {
        let notes = evidence::notes_ref(producer);
        if repo
            .git
            .ref_target(&notes)
            .map_err(|error| error.message())?
            .is_some()
        {
            specs.push(format!("{notes}:{notes}"));
        }
    }
    // G860: and this executor's log, with the terminal records the members wrote on it.
    let mut logs: Vec<(String, String)> = Vec::new();
    for &index in &owed {
        let Some(executor) = &owners[index].owed_log else {
            continue;
        };
        let log = crate::log::log_ref(executor);
        if !logs.iter().any(|(name, _)| *name == log)
            && let Some(tip) = repo.git.ref_target(&log).map_err(|error| error.message())?
        {
            specs.push(format!("{log}:{log}"));
            logs.push((log, tip));
        }
    }
    let mut args: Vec<&str> = vec!["push", "-q", "--atomic"];
    args.extend(leases.iter().map(String::as_str));
    args.push(repo.remote());
    args.extend(specs.iter().map(String::as_str));
    let pushed = repo.git.run(&args).is_ok();
    drop(writing);
    if !pushed {
        return Ok(false);
    }
    for (log, tip) in &logs {
        crate::log::pushed(repo, log, tip);
    }
    for &index in &owed {
        let owner = &mut *owners[index];
        owner.evidence_rides = None;
        owner.owed_log = None;
        owner.push_mode = PushMode::Defer;
        owner.took();
    }
    Ok(true)
}

/// G860 (a fleet measurement, 2026-11-22: a batch of three made one push of each member's landed
/// record and terminal record after the publishing push, three round trips for one landing):
/// every member's landed record, written while each context deferred its pushes, goes in one
/// atomic push with the terminal records on this executor's log ([`push_owed`]). When the remote
/// refuses it (another executor moved a member's chain), each chain is pushed alone, then the log,
/// as each member's own push does when it goes alone ([`Ctx::push_chain_with_record`]).
fn push_landed<'a>(ctx: &mut Ctx<'a>, others: &mut [Ctx<'a>]) {
    let mut owners: Vec<&mut Ctx> = std::iter::once(&mut *ctx)
        .chain(others.iter_mut())
        .collect();
    let pushed = matches!(push_owed(&mut owners), Ok(true));
    let mut log_owed = None;
    for owner in owners {
        if !pushed && owner.push_mode == PushMode::Owed {
            owner.push_mode = PushMode::Now;
            owner.push_chain();
        }
        if let Some(executor) = owner.owed_log.take() {
            log_owed = Some(executor);
        }
        owner.push_mode = PushMode::Now;
    }
    if let Some(executor) = log_owed {
        crate::log::push(ctx.repo, &executor);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Membership {
    Alone,
    Batch,
}

/// G754 (owner ruling 2026-11-16, "freeze batch members"; a fleet finding the same day: two
/// checked batches of six were thrown away because an author revised one member during the
/// batch's check): whether `op` was superseded only by its author's newer request, of a change
/// neither abandoned nor taken over and not on main. Such a member of a batch in its check still
/// lands its checked revision; the newer revision goes in after it as the next change of the same
/// intent, opened by its author's own `bay merge` ([`follow_up`]).
fn revised(record: &ChangeRecord, op: &Operation) -> bool {
    op.state == OpState::Superseded
        && op
            .outcome
            .as_deref()
            .is_some_and(|outcome| outcome.starts_with("superseded by "))
        && !matches!(
            record.state,
            ChangeState::Abandoned | ChangeState::Superseded
        )
        && !record
            .operations
            .iter()
            .any(|op| op.state == OpState::Published)
}

/// G754: whether `op_id`, held by `ctx`, is a batch member its author revised ([`revised`]).
fn frozen(ctx: &Ctx, op_id: &str) -> bool {
    ctx.membership == Membership::Batch
        && ctx
            .find_op(op_id)
            .is_some_and(|op| revised(&ctx.record, op))
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
    let checked_on = op.checked_on.clone();
    // N-9: a failed or unanswered check of a policy with a waivable check may get a waiver.
    let waivable = waivable_block(ctx, op_id, reason)?;
    // G245: the approval it waits on is written on another machine and reaches this one only
    // through the remote, so the retry reads the remote's records before it judges the change.
    // N-9: so is a waiver.
    // G637: so are the records a failed candidate's block is judged by ([`fails_as_before`]).
    if (reason == Some(Reason::ApprovalPending) || reason == Some(Reason::CheckFailed) || waivable)
        && ctx.online
    {
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
    // G637: a change that failed the same way on two mains is not checked on a third.
    if reason == Some(Reason::CheckFailed)
        && then.as_deref() != Some(now.as_str())
        && let Some(units) = fails_as_before(ctx, op_id, revision, then.as_deref())?
    {
        return Ok(Some(
            BayResult::blocked(Reason::CheckFailed)
                .with_change(ctx.id.clone())
                .with_op(op_id)
                .with_detail(
                    json!({"unchanged": true, "fails_as_before": units, "condition": now}),
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
        let mut detail = json!({"unchanged": true, "condition": now});
        if reason == Reason::CheckUnknown
            && let Some(machine) = checked_on
        {
            detail["checked_on"] = json!(machine);
        }
        return Ok(Some(
            BayResult::blocked(reason)
                .with_change(ctx.id.clone())
                .with_op(op_id)
                .with_detail(detail),
        ));
    }
    let op = ctx.op_mut(op_id);
    op.state = OpState::Submitted;
    op.outcome = None;
    op.blocked_on = None;
    op.checked_on = None;
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

/// G637 (a fleet finding, 2026-11-02: two changes whose own task cards failed a records check
/// were checked again on each new main, and failed the same way each time): whether the block
/// of `op_id` stands although trunk moved. It stands when its last two candidates, each its own
/// on a different main, failed the same units for the same reasons, each failing unit names a
/// file the change changes, and nothing but trunk changed since the block (its condition at the
/// last candidate's main is `then`). A new revision is a new operation and a waiver is new
/// evidence, so either one still re-arms it. A failing unit that names no file of the change
/// may be one a newer main fixes, so it keeps the re-arm. The failing units, when it stands.
fn fails_as_before(
    ctx: &Ctx,
    op_id: &str,
    revision: &str,
    then: Option<&str>,
) -> Result<Option<Vec<serde_json::Value>>, String> {
    let [.., before, last] = ctx.op(op_id).attempts.as_slice() else {
        return Ok(None);
    };
    if before.base == last.base
        || [before, last]
            .iter()
            .any(|attempt| attempt.batch.is_some() || attempt.verdict != "product_failure")
    {
        return Ok(None);
    }
    let git = &ctx.repo.git;
    let at_block = condition(
        git,
        revision,
        &last.base,
        Some(&last.tree),
        &ctx.record.observed_versions,
    )?;
    if then != Some(at_block.as_str()) {
        return Ok(None);
    }
    let Some(failed) = failing_units(git, last)? else {
        return Ok(None);
    };
    if failing_units(git, before)?.as_ref() != Some(&failed) {
        return Ok(None);
    }
    let changed = git
        .run(&[
            "diff",
            "--name-only",
            "--no-renames",
            &last.base,
            &last.commit,
        ])
        .map_err(|error| error.message())?;
    let changed: Vec<&str> = changed.lines().filter(|path| !path.is_empty()).collect();
    let own = failed.iter().all(|(_, unit, reason)| {
        changed.iter().any(|path| {
            unit.contains(path)
                || reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains(path))
        })
    });
    Ok(own.then(|| {
        failed
            .into_iter()
            .map(|(check, unit, reason)| json!({"check": check, "unit": unit, "reason": reason}))
            .collect()
    }))
}

/// [`failing_units`]: a failed unit, as its check, id and reason.
type FailingUnit = (String, String, Option<String>);

/// [`fails_as_before`]: each unit the check records about the candidate of `attempt` failed, as
/// its check, id and reason, sorted, once each. `None` when no record failed, or one failed with
/// no failing unit named, so what failed is not known.
fn failing_units(git: &Git, attempt: &Attempt) -> Result<Option<Vec<FailingUnit>>, String> {
    let mut failed = Vec::new();
    for record in evidence::records_for(git, &attempt.tree)? {
        if record.kind != evidence::KIND_CHECK
            || record.subject.commit != attempt.commit
            || record.verdict != evidence::Verdict::ProductFailure
        {
            continue;
        }
        let before = failed.len();
        for unit in record.units {
            if unit.verdict == evidence::Verdict::ProductFailure {
                failed.push((record.check.id.clone(), unit.id, unit.reason));
            }
        }
        if failed.len() == before {
            return Ok(None);
        }
    }
    failed.sort();
    failed.dedup();
    Ok((!failed.is_empty()).then_some(failed))
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
    match policy::load_base_of(&ctx.repo.git, &attempt.base, &attempt.commit) {
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

/// How many of an operation's candidates a check may leave with no clear answer before a pass
/// stops asking it again ([`asked_again`]); then it waits for its author's `bay merge`.
pub const UNKNOWN_RETRIES: usize = 3;

/// Whether every executor pass asks blocked `op` again ([`retry_blocked`]): it waits for an
/// approval ([`awaits_approval`]), or, G574 (a fleet finding, 2026-10-29: a pre check ran on
/// its author's machine and printed no result, and the change waited for its author while its
/// result said to wait), a check gave no clear answer on fewer than [`UNKNOWN_RETRIES`] of its
/// candidates. Asked again, it is re-armed only once its condition changed (a moved trunk), so
/// an unchanged one costs no run. G621 (a fleet finding, 2026-10-31: an operation blocked
/// `MERGE_OUTCOME_UNKNOWN` said bay would look again by itself, and no pass ever did): so is a
/// publication whose outcome is unknown. The pass reads trunk first, so a candidate that landed
/// is recorded landed, never published again; one that did not is re-armed and runs.
#[must_use]
pub fn asked_again(op: &Operation) -> bool {
    awaits_approval(op)
        || (op.state == OpState::Blocked
            && op.outcome.as_deref().and_then(Reason::from_code)
                == Some(Reason::PublishOutcomeUnknown))
        || (op.state == OpState::Blocked
            && op.outcome.as_deref().and_then(Reason::from_code) == Some(Reason::CheckUnknown)
            && op
                .attempts
                .iter()
                .filter(|attempt| attempt.verdict == "unknown")
                .count()
                < UNKNOWN_RETRIES)
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
        // A fleet finding (2026-10-27): with the remote's credential gone, a lazy fetch of the
        // request object failed, and healthy changes were refused for it. A read that fails is
        // this executor's fault, said and retried, never a refusal of the request.
        Err(error) => {
            return Err(format!(
                "the request of {op_id} cannot be read: {}",
                error.message()
            ));
        }
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

/// G416: the change stands on the base its view stands on, when that is newer than the one it
/// records ([`crate::change::newer_base`]), so its draft holds only its own work.
fn stand_on_view_base(ctx: &mut Ctx, trunk: &str) {
    let view = Git::new(ctx.repo.view_path(&ctx.id));
    if let Some(base) = crate::change::newer_base(&view, &ctx.record.base, trunk) {
        ctx.record.base = base;
    }
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
    let latest = ctx
        .chain
        .latest_draft(id)
        .map_err(|error| error.message())?;
    let frozen = freeze_draft(&view, &ctx.record.base, "draft").map_err(|error| error.message())?;
    let revision = if crate::change::same_draft(&repo.git, &frozen, &latest)
        .map_err(|error| error.message())?
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

/// Appends the request for a new operation; the compare-and-swap allocates its number. An
/// `expedite` reason is part of the signed request (BACKLOG 326).
fn submit(ctx: &mut Ctx, revision: &str, expedite: Option<&str>) -> Result<String, String> {
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
        kept: None,
        unable: Vec::new(),
        checked_on: None,
        unavailable: None,
        early_conflict: None,
        undoes: None,
        prechecks: Vec::new(),
        batch_culprit: None,
    });
    // The request: an agent-signed commit on the revision, linked into the chain. Appending it
    // allocates the op id by compare-and-swap; `refs/bay/requests/<id>` moves with it.
    let mut request = json!({
        "schema": REQUEST_SCHEMA,
        "op_id": op_id,
        "change": ctx.id,
        "revision": revision,
        "request_digest": ctx.op(&op_id).request_digest,
        "actor": ctx.record.actor,
        "registry": ctx.signing().map(|registry| registry.commit.clone()),
    });
    if let Some(why) = expedite {
        request["expedite"] = json!({"why": why});
    }
    let request_id = ctx
        .chain
        .request_commit(&request, revision, &format!("request {op_id}"))
        .map_err(|error| error.message())?;
    ctx.op_mut(&op_id).request_id = Some(request_id.clone());
    ctx.save_or_moved(revision, Some(&request_id), &format!("request {op_id}"))?;
    kill_point("before-request-push");
    match ctx.push_chain_and_request(&request_id) {
        Ok(()) => {}
        Err(_) if !ctx.online => {}
        // The remote chain did not move, so the remote refused the push itself: saying the
        // chain moved would be false, and a retry would be refused the same way.
        Err(Some(why)) => {
            return Err(format!(
                "the remote refused the push of the change, and its chain did not move: {why}"
            ));
        }
        // The remote chain moved while this request was written (an executor appended): the
        // caller re-reads the chain and writes the request again.
        Err(None) => return Err(CHAIN_MOVED.to_owned()),
    }
    admit(ctx, &op_id, revision)?;
    Ok(op_id)
}

/// `submit` or a rearm lost a race with another writer of the chain (G298).
const CHAIN_MOVED: &str = "the change chain moved on the remote while the request was written";

/// G897: the records riding toward a landing push could not be pushed before a write that is not
/// theirs ([`Ctx::flush_riding`]); they are taken back.
const RIDING_UNPUSHED: &str =
    "the checked record riding toward the landing push could not be pushed, so it was taken back";

/// The abandon request (§ 4, G40): an agent-signed request of kind `abandon` on the change
/// chain, by the actor the chain records as current. The change becomes `abandoned` and any
/// actor may then adopt its intent. Its pending operations are cancelled; trunk, the requests
/// ref and the view stay as they are.
fn abandon_change(
    repo: &Repo,
    id: &str,
    step: &Step,
    why: Option<&str>,
) -> Result<BayResult, String> {
    let mut ctx = Ctx::load(repo, step, id)?;
    let trunk = match ctx.fetch_trunk()? {
        Ok(trunk) => trunk,
        Err(refusal) => return Ok(refusal),
    };
    let me = crate::change::current_actor(&repo.common_dir)?;
    if me.name != ctx.record.actor.name {
        // A fleet finding (2026-11-11): the refusal named the change's actor `current_actor`,
        // which a session read as its own identity, and so looked for an owner it was shown.
        // It names the change's owner and the caller apart.
        return Ok(BayResult::blocked(Reason::AbandonRefused)
            .with_change(id)
            .with_detail(json!({"owner": ctx.record.actor,
                "owner_source": ctx.record.actor_source,
                "you": me.name})));
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
    let draft = ctx
        .chain
        .latest_draft(id)
        .map_err(|error| error.message())?;
    let mut request = json!({
        "schema": REQUEST_SCHEMA,
        "kind": "abandon",
        "change": id,
        "actor": ctx.record.actor,
        "registry": ctx.signing().map(|registry| registry.commit.clone()),
    });
    // A fleet finding (2026-10-26): an audit of abandoned work could say who gave a change up,
    // never why. The reason, when given, is part of the signed request.
    if let Some(why) = why.filter(|why| !why.trim().is_empty()) {
        request["why"] = json!(why);
    }
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
    // The executor runs from the work tree as any adapter does (`crate::run`), killed whole at
    // its bound, and is given its action as an argument and the request on stdin.
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
                // Never a destructive repair (the no-work-is-lost rule, 2026-10-27): `--from`
                // checkpoints the view's edits before its draft is replaced.
                "repair": "bay start <change> --from <a branch without the lane's commits> (the view's edits are checkpointed first), then bay merge again; or start the work in the lane: bay start <intent> --lane <lane>",
            })),
    ))
}

/// G244, G677: the red post checks (and the checks a fix answers, E35) on paths this change
/// touches, unless it names the landing that made it red (by `--fixes`, an undo, or a supersede),
/// go in `ctx.red_checks`: they run on its candidate before the merge, judged against main
/// ([`policy::Policy::judging_red`]). The change never waits for main (owner ruling 2026-11-07,
/// "merge unless worse"). A check no landing in the window answers (NEW-6,
/// [`crate::red::Pending`]) is unknown, not red: it adds nothing here, and `bay status` lists it
/// as `red_pending`, never as clear.
fn red_gate(ctx: &mut Ctx, revision: &str, trunk: &str) -> Result<(), String> {
    ctx.step.set("red");
    // Each call judges main's reds anew: what an earlier call or a batch's members (G938) asked
    // is not this candidate's.
    ctx.red_checks.clear();
    // G657: a run of its own, unlike a runner's pass, has not read the post records another
    // machine wrote, and would merge past a red it cannot see.
    if ctx.visit == Visit::Own {
        ctx.repo.fetch_evidence_once(&ctx.keys);
    }
    let git = &ctx.repo.git;
    let reds = journaled(ctx, || {
        crate::red::holding(git, trunk, ctx.registry.as_ref())
    })?;
    if reds.is_empty() {
        return Ok(());
    }
    let base = git
        .run(&["merge-base", trunk, revision])
        .map_err(|error| error.message())?;
    let changed = check::changed_paths(git, &base, revision)?;
    let names = ctx.record.repair_names();
    let blocking = crate::red::blocking(&reds, &changed, &names);
    if blocking.is_empty() {
        return Ok(());
    }
    // G677 (owner ruling 2026-11-07, "merge unless worse"): the change never waits for main;
    // each red check on its paths runs on its candidate before the merge, judged against main,
    // and only a failure main does not have refuses it ([`policy::Policy::judging_red`]).
    // G815: that run asks again the units the red failed, as a post run on main does (G540):
    // its record is the landing's, and a pass that ran none of them would answer the red.
    let red_checks = blocking
        .iter()
        .map(|red| crate::bisect::red_units(git, ctx.registry.as_ref(), red))
        .collect::<Result<Vec<_>, String>>()?;
    ctx.red_checks = red_checks;
    Ok(())
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
    note_wait(ctx, op_id, revision, reason, message)
}

/// The wait of `op_id` as `reason`, written once with the chain message `message`: a claim
/// this executor holds is given back (a running operation returns to admitted or submitted).
fn note_wait(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    reason: Reason,
    message: &str,
) -> Result<(), String> {
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

/// B3 (a fleet finding, 2026-10-26: `ahead = 32` merged to main stopped every bay command on
/// every box): a change that edits `.bay/config.toml` into one this bay cannot load is blocked
/// `POLICY_INVALID`, naming why, before it waits in line, so its owner repairs it in a new
/// revision. `None` when the change leaves the config as its base has it, or it loads.
fn config_gate(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    trunk: &str,
) -> Result<Option<BayResult>, String> {
    let op = ctx.op(op_id);
    if op.state.is_terminal() || op.state == OpState::Blocked {
        return Ok(None);
    }
    let git = &ctx.repo.git;
    let Ok(base) = git.run(&["merge-base", trunk, revision]) else {
        return Ok(None);
    };
    let blob = |at: &str| {
        git.run(&[
            "rev-parse",
            "-q",
            "--verify",
            &format!("{at}:{}", crate::config::CONFIG_PATH),
        ])
        .ok()
    };
    if blob(revision) == blob(&base) {
        return Ok(None);
    }
    let Err(why) = crate::config::Config::exactly_at(git, revision) else {
        return Ok(None);
    };
    let result = BayResult::blocked(Reason::PolicyInvalid)
        .with_change(ctx.id.clone())
        .with_op(op_id)
        .with_detail(json!({"config_at": revision, "why": why}));
    if !held_elsewhere(ctx, op_id) {
        give_back_place(ctx, op_id, "policy_invalid")?;
        let op = ctx.op_mut(op_id);
        op.state = OpState::Blocked;
        op.outcome = Some(Reason::PolicyInvalid.code().to_owned());
        op.owner = None;
        ctx.save(
            revision,
            None,
            &format!(
                "blocked {op_id}: its {} does not load",
                crate::config::CONFIG_PATH
            ),
        )?;
        ctx.push_chain();
    }
    Ok(Some(result))
}

/// G273: the result of `op_id` while a change it lands after is not on trunk at `trunk`, read
/// with no write: `DEPENDENCY_ABANDONED` (blocked) when one ended without landing, else
/// `DEPENDENCY_PENDING` (queued) when one has not landed yet, naming each that is not submitted
/// or stopped (G604) and who must act on it, else `None`. G869: each one on trunk that the
/// remote holds no landed marker for is kept in `ctx`, for the push that lands `op_id`.
fn dependency_waiting(ctx: &Ctx, op_id: &str, trunk: &str) -> Result<Option<BayResult>, String> {
    use crate::after::Standing;
    use std::fmt::Write as _;
    let standing = crate::after::standing(ctx.repo, &ctx.record.after, trunk)?;
    *ctx.unmarked.borrow_mut() = standing
        .iter()
        .filter_map(|dependency| match &dependency.standing {
            Standing::Unmarked(commit) => Some((dependency.change.clone(), commit.clone())),
            _ => None,
        })
        .collect();
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
        .filter(|dependency| {
            matches!(
                dependency.standing,
                Standing::Pending(_) | Standing::Unsubmitted { .. } | Standing::Blocked { .. }
            )
        })
        .collect();
    if pending.is_empty() {
        return Ok(None);
    }
    let names: Vec<&str> = pending
        .iter()
        .map(|dependency| dependency.change.as_str())
        .collect();
    let mut why = format!(
        "this change merges only after {}, not yet merged into main",
        names.join(", ")
    );
    // BACKLOG 157, G604: a change no request was made for, or one that stopped, never merges by
    // waiting; who must act on it is named.
    for stalled in pending.iter().filter_map(|dependency| dependency.stalled()) {
        let _ = write!(why, "; {}", stalled.clause());
    }
    let mut result = BayResult::refused(Decision::Queued, Reason::DependencyPending)
        .with_change(ctx.id.clone())
        .with_op(op_id)
        .with_detail(json!({
            "pending": pending.iter().copied().map(crate::after::Dependency::to_json).collect::<Vec<_>>(),
            "after": after,
            "why": why,
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
/// at `trunk` marks `lockable` (in `.gitattributes`), and no commit of that path's lock names this
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

/// `UNDOES_MERGED` at candidate build (BACKLOG 64 F1, owner ruling 2026-10-24): a change written
/// from an older copy of a file, by any tool, merged cleanly and put back the file as it was
/// before another change main merged after this change began. For each such commit on main's
/// line since the change began (`base_first`) that touched the change's own files (its edits
/// from its current base), its patch of each of those
/// files is applied, only as a check, to the merged `tree`: when it still applies, `tree` holds
/// the file as it was before that commit, so the merge would undo it. Exact reversal only: a
/// change that reworked those lines no longer matches the commit's context. A change whose
/// `Reverts:` names that change undoes it on purpose. Blocked as on a conflict; no candidate is
/// committed and no check runs.
fn undoes_merged(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    trunk: &str,
    tree: &str,
) -> Result<Option<BayResult>, String> {
    let repo = ctx.repo;
    let git = &repo.git;
    // The change's own edits, from its current base (main itself once it brought main in).
    let Ok(base) = git.run(&["merge-base", trunk, revision]) else {
        return Ok(None);
    };
    let mine: Vec<String> = git
        .run(&["diff", "--name-only", "--no-renames", "-z", &base, revision])
        .map_err(|error| error.message())?
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect();
    if mine.is_empty() {
        return Ok(None);
    }
    // Since the change began (`base_first`), not since its current base: a change that brought
    // in main and then wrote a file from its older copy undoes work its base already holds.
    let began = &ctx.record.base_first;
    let since = if git.is_ancestor(began, trunk).unwrap_or(false) {
        began.as_str()
    } else {
        base.as_str()
    };
    let range = format!("{since}..{trunk}");
    let mut args = vec!["log", "--first-parent", "--format=%H", range.as_str(), "--"];
    // A change of few files asks only for the commits that touched them, so every merge stays
    // cheap; a list as long as a large move's costs more than reading each commit.
    if mine.len() <= PATHS_NAMED_TO_LOG {
        args.extend(mine.iter().map(String::as_str));
    }
    let commits = git.run(&args).map_err(|error| error.message())?;
    if commits.is_empty() {
        return Ok(None);
    }
    // The merged tree and main's own, each in an index of its own, so a patch is applied to
    // them only as a check. A patch that applies to main too is undone on main already (a later
    // commit there put the file back), never by this change.
    let index = repo
        .common_dir
        .join("bay")
        .join(format!("undo-{op_id}.index"));
    let main_index = repo
        .common_dir
        .join("bay")
        .join(format!("undo-{op_id}.main.index"));
    let (index_var, main_var) = (
        index.display().to_string(),
        main_index.display().to_string(),
    );
    let envs = [("GIT_INDEX_FILE", index_var.as_str())];
    let main_envs = [("GIT_INDEX_FILE", main_var.as_str())];
    // G751: what a killed run of this operation left, lock and all, never fails this one.
    crate::merge::remove_scratch_index(&index);
    crate::merge::remove_scratch_index(&main_index);
    git.run_with(&["read-tree", tree], &envs, None)
        .map_err(|error| error.message())?;
    git.run_with(&["read-tree", trunk], &main_envs, None)
        .map_err(|error| error.message())?;
    let skip = [Some(ctx.id.as_str()), ctx.record.reverts.as_deref()];
    let mine: BTreeSet<&str> = mine.iter().map(String::as_str).collect();
    let merged = paths_of(git, tree)?;
    let found = undone_by(
        git,
        &commits,
        &mine,
        &merged,
        (trunk, tree),
        &skip,
        (&envs, &main_envs),
    );
    // G751: with the locks a killed Git command left beside them.
    crate::merge::remove_scratch_index(&index);
    crate::merge::remove_scratch_index(&main_index);
    let mut found = found.into_iter();
    let Some(mut undone) = found.next() else {
        return Ok(None);
    };
    undone.also = found.collect();
    let condition = condition(git, revision, trunk, None, &ctx.record.observed_versions)?;
    let op = ctx.op_mut(op_id);
    op.state = OpState::Blocked;
    op.outcome = Some(Reason::UndoesMerged.code().to_owned());
    op.blocked_on = Some(condition);
    op.undoes = Some(undone.clone());
    ctx.save(revision, None, "blocked: undoes merged work")?;
    ctx.push_chain();
    Ok(Some(
        BayResult::blocked(Reason::UndoesMerged)
            .with_change(ctx.id.clone())
            .with_op(op_id)
            .with_detail(json!({
                "paths": undone.paths,
                "change": undone.change,
                "commit": undone.commit,
                "also": undone.also,
                "main": trunk,
            })),
    ))
}

/// Every path of `tree`.
fn paths_of(git: &Git, tree: &str) -> Result<BTreeSet<String>, String> {
    Ok(git
        .run(&["ls-tree", "-r", "--name-only", "-z", tree])
        .map_err(|error| error.message())?
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect())
}

/// The most files of a change [`undoes_merged`] names to `git log`.
const PATHS_NAMED_TO_LOG: usize = 500;

/// A Git call's environment.
type Envs<'a> = &'a [(&'a str, &'a str)];

/// How many undone files a refusal names; the next refusal names any others.
const UNDONE_SHOWN: usize = 100;

/// G671: what each of `commits` (newest first) would lose: its files among `mine` whose patch
/// applies to the merged tree's index and not to main's (`indexes`). A commit of a change `skip`
/// names (the change's own, or the one it reverts) is passed. Git is asked once per commit; a
/// patch is read only for a file that can apply: a created file where the merged tree has none,
/// an edited or deleted file where it has one, which leaves out nearly every file of a move.
fn undone_by(
    git: &Git,
    commits: &str,
    mine: &BTreeSet<&str>,
    merged: &BTreeSet<String>,
    (trunk, tree): (&str, &str),
    skip: &[Option<&str>],
    (envs, main_envs): (Envs, Envs),
) -> Vec<crate::change::Undone> {
    let mut found = Vec::new();
    let mut shown = 0;
    for commit in commits.lines() {
        let Ok(changed) = git.run(&[
            "diff-tree",
            "-r",
            "--no-renames",
            "--name-status",
            "-z",
            &format!("{commit}^1"),
            commit,
        ]) else {
            continue;
        };
        let fields: Vec<&str> = changed.split('\0').collect();
        let mut lost = Vec::new();
        for &[status, path] in fields.as_chunks::<2>().0 {
            if !mine.contains(path) || (status == "A") == merged.contains(path) {
                continue;
            }
            let patch = git
                .run_bytes(
                    &[
                        "diff",
                        "--binary",
                        "--no-renames",
                        &format!("{commit}^1"),
                        commit,
                        "--",
                        path,
                    ],
                    &[],
                    None,
                )
                .unwrap_or_default();
            let applies = |envs: &[(&str, &str)]| {
                git.run_bytes(&["apply", "--cached", "--check", "-"], envs, Some(&patch))
                    .is_ok()
            };
            if !patch.is_empty()
                && applies(envs)
                && !applies(main_envs)
                && (status != "A" || !moved_away(git, path, merged, trunk, tree))
            {
                lost.push(path.to_owned());
            }
        }
        if lost.is_empty() {
            continue;
        }
        let theirs = git
            .run(&[
                "log",
                "-1",
                "--format=%(trailers:key=Change-Id,valueonly)",
                commit,
            ])
            .unwrap_or_default();
        let theirs = theirs.trim();
        // Its own earlier merge, or the change it undoes on purpose, is no one else's work.
        if !theirs.is_empty() && skip.contains(&Some(theirs)) {
            continue;
        }
        shown += lost.len();
        found.push(crate::change::Undone {
            paths: lost,
            change: (!theirs.is_empty()).then(|| theirs.to_owned()),
            commit: commit.to_owned(),
            also: Vec::new(),
        });
        if shown >= UNDONE_SHOWN {
            break;
        }
    }
    found
}

/// G671: main's `path` is not lost when the merged tree holds it under another folder, reworked or
/// not: Git sees a rename to a path of the same name that main does not have.
fn moved_away(git: &Git, path: &str, merged: &BTreeSet<String>, trunk: &str, tree: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let mut args = vec![
        "diff",
        "--find-renames",
        "--diff-filter=R",
        "--name-only",
        trunk,
        tree,
        "--",
        path,
    ];
    let before = args.len();
    args.extend(
        merged
            .iter()
            .filter(|other| other.rsplit('/').next() == Some(name))
            .map(String::as_str),
    );
    args.len() > before && git.run(&args).is_ok_and(|renamed| !renamed.is_empty())
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
        // G675: an edit of the policy or a check's own files states the next revision itself.
        let tree = crate::merge::raise_revision(&ctx.repo.git, trunk, &tree)?;
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
        // BACKLOG 64 F1: no change silently puts back what another merged after it began.
        if let Some(refusal) = undoes_merged(ctx, op_id, revision, trunk, &tree)? {
            return Ok(Err(refusal));
        }
        if let Some(no_effect) = no_effect(ctx, op_id, revision, trunk, &tree)? {
            return Ok(Err(no_effect));
        }
        anchor(&ctx.repo.git, op_id, &tree)?;

        let candidate_id = format!("{op_id}.{}", next_candidate(ctx, op_id));
        let request_id = ctx.op(op_id).request_id.clone().unwrap_or_default();
        // N-21: an unsigned landing (no registry, or `signing = "approvals"`) names none.
        let registry_id = ctx
            .signing()
            .map_or_else(|| "none".to_owned(), |registry| registry.commit.clone());
        let message = candidate_message(ctx, revision, &request_id, &candidate_id, &registry_id);
        let commit = commit_candidate(ctx, &tree, trunk, &message, revision)?;
        anchor(&ctx.repo.git, op_id, &commit)?;
        kill_point("after-commit");
        (candidate_id, commit, tree)
    };

    // Step 4: check the exact candidate under the policy at its base.
    // NEW-4: a red trunk holds landings; a lane change never lands, so its checks run.
    // G428, G429 (fleet findings, 2026-10-28): asked once the candidate merges, and before the
    // input providers run, as a batch asks it first. A head that conflicted with main judged
    // main's red for minutes before its twenty-second merge refused it.
    if ctx.record.lane.is_none() {
        red_gate(ctx, revision, trunk)?;
    }
    // G429 (a fleet finding, 2026-10-28): the input providers run once the candidate is known to
    // merge and to be landable. A head that conflicted with main ran them for four to eight
    // minutes first; they judge main's version of an input, never the merged candidate.
    let judged = match input_gate(ctx, op_id, revision, trunk)? {
        Ok(judged) => judged,
        Err(refusal) => return Ok(Err(refusal)),
    };
    ctx.step.set("check");
    let (has_policy, verdict, refusal) = check_candidate(
        ctx,
        op_id,
        &candidate_id,
        (commit.as_str(), tree.as_str(), revision),
        trunk,
        &inputs::request_inputs(&judged),
    )?;
    // G910: another runner took the operation while its candidate was checked: nothing of it is
    // persisted, and the visit ends as the remote's chain reads.
    if let Some(taken) = taken_from(ctx, op_id, revision)? {
        return Ok(Err(taken));
    }
    // G796: main moved while the candidate was checked; nothing of it is persisted.
    if ctx.stopped.borrow().is_some()
        && let Some(stopped) = refusal
    {
        return Ok(Err(stopped));
    }
    let refusal = judged_outcome(ctx, op_id, &candidate_id, refusal, &judged);
    let refusal = match refusal {
        Some(waiting) if waiting.reason == Some(Reason::CheckUnavailable) => {
            return wait_for_worker(ctx, op_id, revision, waiting).map(Err);
        }
        Some(unknown) => match handed_to_runners(ctx, op_id, &unknown, trunk) {
            Some(waiting) => return wait_for_worker(ctx, op_id, revision, waiting).map(Err),
            None => Some(checked_on(ctx, unknown)),
        },
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

/// G796: the number of `op_id`'s next candidate. A candidate whose checks main's move stopped is
/// never persisted, yet its number named its scratch and its records, so the next one of the
/// visit takes a later number; and a later visit counts from the highest number persisted, never
/// from how many attempts there are (a test finding, 2026-11-19: after a visit stopped one
/// candidate and persisted the next, the visit after it built a second candidate of the same
/// number, and the terminal record paired it with the first one's check).
fn next_candidate(ctx: &Ctx, op_id: &str) -> usize {
    let op = ctx.op(op_id);
    let persisted = op
        .attempts
        .iter()
        .filter_map(|attempt| {
            attempt
                .candidate_id
                .strip_prefix(op_id)?
                .strip_prefix('.')?
                .parse::<usize>()
                .ok()
        })
        .max()
        .unwrap_or(0)
        .max(op.attempts.len());
    persisted + ctx.stopped_trees.len() + 1
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
    // The evidence is on the remote before the chain names the candidate checked: it goes in
    // the chain's push, atomically (BACKLOG 92).
    if refusal.is_none() && has_policy {
        ctx.evidence_rides = Some(ctx.producer()?.0);
    }
    // G897: a passing candidate's checked record, and its evidence, ride the landing push that
    // follows, so the remote takes them in one push, not two. A lane change never lands, a batch
    // member's chain goes in the batch's push, and a refusal ends the run here.
    let rides = refusal.is_none()
        && ctx.record.lane.is_none()
        && ctx.push_mode == PushMode::Now
        && ctx.online;
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
        let _ = push_evidence(ctx.repo, &ctx.producer()?.0);
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
    let checked_on = refusal
        .as_ref()
        .and_then(|refusal| refusal.detail.as_ref())
        .and_then(|detail| detail["checked_on"].as_str())
        .map(str::to_owned);
    let message = format!("{word} {}", attempt.candidate_id);
    let persisted = persist_owned(
        ctx,
        op_id,
        revision,
        (&attempt.commit, &message),
        rides,
        |op| {
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
            op.checked_on.clone_from(&checked_on);
        },
    )?;
    match (persisted, refusal) {
        (Persisted::Lost(result), _) => Ok(Err(*result)),
        (_, Some(refusal)) => Ok(Err(refusal)),
        (Persisted::Unpushed, None) => {
            Err("the change chain could not be pushed; nothing was merged".to_owned())
        }
        (Persisted::Pushed | Persisted::Riding, None) => {
            kill_point("after-persist");
            Ok(Ok(attempt))
        }
    }
}

/// What [`persist_owned`] did.
enum Persisted {
    /// The write is on the remote chain.
    Pushed,
    /// G897: the write rides toward the landing push, on this process's head alone
    /// ([`Ctx::save_riding`]).
    Riding,
    /// The write is in the local chain only: the remote could not be reached or kept moving.
    Unpushed,
    /// The operation is no longer this executor's: the result says so.
    Lost(Box<BayResult>),
}

/// Step 5's write (§ 9a): `apply` to the operation this executor claimed, appended and pushed
/// under the chain's compare-and-swap. A lost race, on this box's shared chain ref or on the
/// remote, re-reads the remote chain and applies again while the operation is still this
/// executor's epoch; once another epoch holds it, or the remote lacks it, it is `superseded`.
/// With `rides` (G897) the write is made on this process's head alone, and the landing push
/// that follows carries it.
fn persist_owned(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    (candidate, message): (&str, &str),
    rides: bool,
    apply: impl Fn(&mut Operation),
) -> Result<Persisted, String> {
    for _ in 0..3 {
        let mine = ctx.my_epoch;
        let frozen = frozen(ctx, op_id);
        let Some(op) = ctx.find_op_mut(op_id) else {
            return Ok(Persisted::Lost(Box::new(superseded(
                &ctx.id, op_id, 0, None,
            ))));
        };
        if op.epoch != mine || (op.state.is_terminal() && !frozen) {
            let (epoch, owner) = (op.epoch, op.owner.clone());
            return Ok(Persisted::Lost(Box::new(superseded(
                &ctx.id,
                op_id,
                epoch,
                owner.as_ref(),
            ))));
        }
        apply(op);
        if rides {
            if ctx.save_riding(revision, Some(candidate), message)? {
                return Ok(Persisted::Riding);
            }
        } else if ctx.save_cas(revision, Some(candidate), message)?
            && ctx.push_chain_with_evidence()?
        {
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
    // The revision main's policy states, valid or not: under B2 the judging policy may be an
    // older one, and a fix must still raise main's revision, never move it back.
    let stated = match policy::revision_at(git, trunk) {
        Ok(revision) => revision.unwrap_or(0),
        Err(why) => return Some(why),
    };
    let base_revision = base_policy.map_or(stated, |policy| policy.revision.max(stated));
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
// One judgement, read top to bottom: the base's policy (or the last valid one), then the
// candidate's own when it changes the policy.
#[allow(clippy::too_many_lines)]
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
    // G676: a refused policy edit says `POLICY_INVALID`, its next step and its retry rule, on
    // the result and the operation, so status and the author's wake say it; never an empty block.
    let refuse = |why: String| Ok(policy_invalid(ctx, op_id, commit, "candidate", &why));
    let mut adapters_from: Option<String> = None;
    let base_policy = match policy::load_at(git, trunk) {
        Ok(policy) => policy,
        // § 7: a registry-signed repair is judged by its own new policy alone, so an invalid
        // base policy stays repairable.
        Err(policy::LoadError::Invalid(_)) if ctx.repair.is_some() => None,
        // B2 (owner ruling 2026-10-26): the policy at the accepted base breaks a validity rule,
        // so the candidate is judged by the last valid policy on main's first-parent line, with
        // that commit's adapters, and every result says so. Only when main never had a valid
        // one is the candidate refused: a bad policy never passes its own candidate.
        Err(policy::LoadError::Invalid(why)) => match policy::last_valid_at(git, trunk) {
            Some((at, policy)) => {
                *ctx.policy_used.borrow_mut() = Some(json!({
                    "commit": at,
                    "revision": policy.revision,
                    "invalid_at": trunk,
                    "why": why,
                }));
                adapters_from = Some(at);
                Some(policy)
            }
            None => return Ok(policy_invalid(ctx, op_id, trunk, "base", &why)),
        },
        // A read fault is a fault, retried; never a refusal of the change.
        Err(policy::LoadError::Git(why)) => return Err(why),
    };
    let base_from = adapters_from.as_deref().unwrap_or(trunk);
    // G635: under `signing = "approvals"` no request is signed, so no signature marks a repair.
    // A candidate that changes only the policy and adapter paths is judged by the base's checks
    // as before, each waivable by an enrolled approver on that exact candidate; its own policy
    // still runs every check, none waivable.
    let base_policy = match base_policy {
        Some(base) if ctx.repair.is_none() && policy::repairs(git, trunk, commit, &base)? => {
            Some(base.with_repair_waivers())
        }
        other => other,
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
    let changed = policy::policy_changed(git, trunk, commit, base_policy.as_ref())?;
    if !changed {
        if ctx.repair.is_some() {
            return refuse("a policy repair changes only .bay/policy.toml and adapter paths, and this change does not change them".to_owned());
        }
        let Some(policy) = &base_policy else {
            return Ok(policy_missing(ctx, op_id, trunk));
        };
        let policy = &policy.judging_red(&ctx.red_ids());
        let (verdict, refusal) = run(candidate_id, policy, base_from)?;
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
        // A read fault is a fault, retried; never a refusal of the change.
        Err(policy::LoadError::Git(why)) => return Err(why),
    };
    if let Some(why) = unraised(git, trunk, base_policy.as_ref(), &own_policy) {
        return refuse(why);
    }
    if let Some(principal) = &ctx.repair {
        if !policy::only_policy_paths(git, trunk, commit, base_policy.as_ref(), &own_policy)? {
            return refuse(format!(
                "a policy repair signed by {principal} may touch only .bay/policy.toml and adapter paths"
            ));
        }
        let own_id = format!("{candidate_id}-own");
        let (verdict, refusal) = run(&own_id, &own_policy, commit)?;
        return Ok((!own_policy.unchecked, verdict, refusal));
    }
    // "Approval = repair" (G387): under `signing = "approvals"` a person's yes to a policy fix
    // stands in for a repair's signature; see [`approval_repair`].
    if let Some(approvals) = approval_repair(ctx, base_policy.as_ref(), trunk, commit, &own_policy)?
    {
        let own = run(&format!("{candidate_id}-own"), &own_policy, commit)?;
        return approved_repair(own, || run(candidate_id, &approvals, base_from));
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
    let policy = &policy.judging_red(&ctx.red_ids());
    let (verdict, refusal) = run(candidate_id, policy, base_from)?;
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

/// G387: an approved repair's verdict: its own policy's run, `own`, then, when that passed, the
/// base's approval checks (`approvals`).
type PolicyRun = (&'static str, Option<BayResult>);
fn approved_repair(
    own: PolicyRun,
    approvals: impl FnOnce() -> Result<PolicyRun, String>,
) -> Result<(bool, &'static str, Option<BayResult>), String> {
    let (verdict, refusal) = own;
    if refusal.is_some() {
        return Ok((true, verdict, refusal));
    }
    let (approved, refusal) = approvals()?;
    let verdict = if refusal.is_some() || verdict == UNCHECKED {
        approved
    } else {
        verdict
    };
    Ok((true, verdict, refusal))
}

/// G387: the base's policy cut to the approval checks the candidate wakes, when the candidate
/// is an approved repair's kind under `signing = "approvals"`: it changes only the policy and
/// adapter paths, and the base requires an approval of it. `None` otherwise, and under
/// `signing = "all"` or for a registry-signed repair, where a repair is the registry's
/// signature.
fn approval_repair(
    ctx: &Ctx,
    base: Option<&policy::Policy>,
    trunk: &str,
    commit: &str,
    own: &policy::Policy,
) -> Result<Option<policy::Policy>, String> {
    let git = &ctx.repo.git;
    let (Some(registry), Some(base)) = (ctx.registry.as_ref(), base) else {
        return Ok(None);
    };
    if registry.signs_all() || base.unchecked || ctx.repair.is_some() {
        return Ok(None);
    }
    if !policy::only_policy_paths(git, trunk, commit, Some(base), own)? {
        return Ok(None);
    }
    let changed = check::changed_paths(git, trunk, commit)?;
    let mut approvals = base.clone();
    approvals
        .checks
        .retain(|entry| entry.is_approval() && base.requires(entry, &changed));
    // Only those checks are owed of this run; the digest stays the base's, which an approval
    // names.
    approvals.manifest.retain(|owed| {
        approvals
            .checks
            .iter()
            .any(|entry| entry.id == owed.id && entry.environment == owed.environment)
    });
    Ok((!approvals.checks.is_empty()).then_some(approvals))
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
/// its approver signed it, and nothing new is written), and the waivers of earlier candidates
/// that carry onto `commit` (G638). A batch's tip lands several changes, so no one change's
/// approvals or waivers carry onto it (`one_change` false).
fn counted_and_carried(
    ctx: &Ctx,
    policy: &policy::Policy,
    (commit, tree, base_tree): (&str, &str, &str),
    one_change: bool,
) -> Result<(Vec<evidence::EvidenceRecord>, throughput::CarriedApprovals), String> {
    let git = &ctx.repo.git;
    if one_change {
        fetch_for_carry(ctx, policy, tree);
    }
    let mut all = evidence::counted(
        git,
        evidence::records_for(git, tree)?,
        ctx.registry.as_ref(),
    )?;
    let carried = if one_change {
        let earlier = throughput::candidates_of(&ctx.record);
        let mut carried = throughput::carried_approvals(
            git,
            policy,
            ctx.registry.as_ref(),
            (tree, base_tree),
            &all,
            &earlier,
        )?;
        carried.waivers = throughput::carried_waivers(
            git,
            policy,
            ctx.registry.as_ref(),
            (commit, tree, base_tree),
            &all,
            &earlier,
        )?;
        carried
    } else {
        throughput::CarriedApprovals::default()
    };
    all.extend(carried.records.iter().cloned());
    Ok((all, carried))
}

/// G638: a waiver of an earlier candidate is written on the approver's machine and reaches this
/// one only through the remote. A new revision's first merge passes no retry that fetches it
/// (`retry_blocked`), so the carry reads the remote's records first: once in this process (a
/// runner's pass fetched them at its start), and only for a change with a candidate on another
/// tree under a policy with a waivable check.
fn fetch_for_carry(ctx: &Ctx, policy: &policy::Policy, tree: &str) {
    if ctx.online
        && policy.checks.iter().any(|entry| entry.waivable)
        && throughput::candidates_of(&ctx.record)
            .iter()
            .any(|candidate| candidate.tree != tree)
    {
        ctx.repo.fetch_evidence_once(&ctx.keys);
    }
}

/// Runs `policy`'s checks on the candidate with the adapter materialized from `adapter_from`,
/// and combines the evidence on its tree under that policy's manifest. `change` is the change
/// the candidate lands, when it lands one (a batch's lands several).
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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
    // The checks' scratch is the repository's own, in bay's state directory, and hidden from
    // every check but its own part (G26, G62). It is never under the repository: its path
    // holds no `/.git`, which some tools leave out (G241, `check::scratch_root`).
    let scratch = check::scratch_root(&ctx.repo.common_dir)?.join(candidate_id);
    // A fleet finding (2026-10-27): the scratch was removed only after a check run that
    // returned, so an error left it behind, and one box's runners held 17 GB of finished
    // candidates. It goes on every path out.
    let _cleanup = crate::run::Scratch(scratch.clone());
    // E21: what is read before the checks run (provenance, the evidence that already answers
    // them, the changed paths) is its own step; about a minute before each bisection run.
    ctx.step.set("evidence");
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
    // G859: a failure that a record reused or carried in place of a run already answers (G713)
    // ends the run before it starts, as a failure in the run ends it (owner ruling 2026-10-25).
    // A fleet finding, 2026-11-22: a batch's culprit, refused on the bisection's carried record,
    // held the head of the line 19 minutes while the checks after it ran.
    let known = known_failures(policy, &reused, &changed, (commit, &base_tree));
    let missing = check::unserved(policy, &changed, &finished, &check::Served::read(git));
    if !missing.is_empty() && known.is_empty() {
        return Ok(("unknown", Some(unavailable(ctx, op_id, &missing, None))));
    }
    let mut keep = |record: &mut evidence::EvidenceRecord| -> Result<(), String> {
        // The producer signs outside the check's sandbox, after reading its output (§ 7).
        if let Some(signer) = &producer_signer {
            record.sign(&signer.key)?;
        }
        evidence::append(git, &producer, record)
    };
    ctx.step.set("check");
    let baseline = baseline_of(ctx, policy, trunk, &base_tree)?;
    // G796: main's move stops the checks of a candidate that can land: never a lane change's,
    // which never lands, nor a check ahead of its turn, which judges a predicted base. G861: a
    // check ahead of its turn stops when it gives way to a head nobody runs ([`HeadWatch`]).
    let halt = ctx
        .record
        .lane
        .is_none()
        .then(|| std::sync::Arc::new(check::Halt::default()));
    let reusing = ctx.repo.config.evidence_reuse() && ctx.reuse_trusted(trunk) && inputs.is_empty();
    let moved_to: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
    let mut line = ctx.ahead_of_turn.then(|| HeadWatch::new(git, &ctx.id));
    let mut beat = || {
        heartbeat(ctx, op_id);
        let Some(halt) = &halt else {
            return;
        };
        // G910: a holder another runner took over stops its checks at once.
        if taken_over(ctx, op_id) {
            if !halt.moved() {
                eprintln!(
                    "bay runner: another runner took {op_id} while {candidate_id} was checked: its checks stop"
                );
                halt.stop_all_but(Vec::new());
            }
            return;
        }
        if let Some(line) = line.as_mut() {
            if let Some(head) = line.gives_way(ctx.repo, halt) {
                eprintln!(
                    "bay runner: {} gives way to {head}, a head nobody runs, while {candidate_id} was checked ahead of its turn: its checks stop",
                    ctx.id
                );
                ctx.yielded.set(true);
            }
            return;
        }
        if let Some(now) = watch_main(ctx, (policy, reusing), trunk, halt) {
            eprintln!(
                "bay runner: {} main moved to {} while {candidate_id} was checked: its checks stop, and it is built again",
                ctx.id,
                now.get(..10).unwrap_or(&now)
            );
            *moved_to.borrow_mut() = Some(now);
        }
    };
    let mut run = |finished: Vec<evidence::CheckId>| {
        check::run_all(
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
                turn: Some(if ctx.ahead_of_turn { "ahead" } else { "head" }),
                rerun: &[],
                retry: 0,
            },
            &producer,
            &check::Limits {
                budget: ctx.repo.config.execution_budget(),
                finished,
                asked: ctx.red_checks.clone(),
                heartbeat: Some(ctx.repo.config.heartbeat()),
                hidden: hidden_paths(ctx),
                sandbox: check::Sandbox::here().reading(git),
                baseline: baseline.clone(),
                parallel: ctx.repo.parallel_checks(),
                halt: halt.clone(),
            },
            &mut check::Hooks {
                keep: &mut keep,
                beat: &mut beat,
                seen: None,
            },
        )
    };
    let check::Ran {
        mut records,
        unavailable: mut outage,
        mut stopped,
    } = if known.is_empty() {
        run(finished.clone())?
    } else {
        check::Ran {
            records: Vec::new(),
            unavailable: None,
            stopped: false,
        }
    };
    // G861: a check that gave way to a head nobody runs is no verdict: nothing of it is recorded.
    // G910: nor is one another runner took over.
    if stopped && (ctx.yielded.get() || taken_over(ctx, op_id)) {
        let mut waiting = BayResult::new(Decision::Queued)
            .with_change(ctx.id.clone())
            .with_op(op_id);
        waiting.waiting_for = Some("executor".to_owned());
        return Ok(("unknown", Some(waiting)));
    }
    ctx.yielded.set(false);
    // G638: a failure a carried waiver answers ended the run before the checks after it; they
    // run now, as they would after a waiver of this exact candidate (`finished_checks`).
    let failed: Vec<evidence::CheckId> = check::failed(&records)
        .into_iter()
        .cloned()
        .chain(known)
        .collect();
    if outage.is_none() && change.is_some() && !failed.is_empty() {
        fetch_for_carry(ctx, policy, tree);
        let own = evidence::counted(
            git,
            evidence::records_for(git, tree)?,
            ctx.registry.as_ref(),
        )?;
        let waived = throughput::carried_waivers(
            git,
            policy,
            ctx.registry.as_ref(),
            (commit, tree, &base_tree),
            &own,
            &throughput::candidates_of(&ctx.record),
        )?;
        if failed
            .iter()
            .all(|id| waived.iter().any(|waiver| waiver.check == *id))
        {
            // A check this box does not serve still waits for a worker (§ 7).
            if !missing.is_empty() {
                return Ok(("unknown", Some(unavailable(ctx, op_id, &missing, None))));
            }
            let mut done = finished;
            done.extend(records.iter().map(|record| record.check.clone()));
            let again = run(done)?;
            records.extend(again.records);
            outage = again.unavailable;
            stopped = again.stopped;
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    // G796: a run main's move stopped is no verdict: the visit builds again on the new main.
    if stopped && let Some(main) = moved_to.take() {
        *ctx.stopped.borrow_mut() = Some(Stopped {
            main,
            tree: tree.to_owned(),
        });
        let mut waiting = BayResult::new(Decision::Queued)
            .with_change(ctx.id.clone())
            .with_op(op_id);
        waiting.waiting_for = Some("executor".to_owned());
        return Ok(("unknown", Some(waiting)));
    }
    // The adapter reached no worker for a check: nothing was observed, so it is no verdict.
    if let Some((environment, reason)) = outage {
        return Ok((
            "unknown",
            Some(unavailable(ctx, op_id, &[environment], Some(&reason))),
        ));
    }
    let (all, carried) =
        counted_and_carried(ctx, policy, (commit, tree, &base_tree), change.is_some())?;
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
            base_tree: &base_tree,
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
    /// The tree of the base the candidate is judged against (`policy::CheckEntry::key`).
    base_tree: &'a str,
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

/// G673: on each failed check's row, the files of that check (its `adapter`) the change edits,
/// as `runs_as_main`. A check runs as main has it (G26), so such an edit lands on its own first.
fn runs_as_main(policy: &policy::Policy, changed: &[String], rows: &mut serde_json::Value) {
    for row in rows.as_array_mut().into_iter().flatten() {
        let Some(entry) = policy
            .checks
            .iter()
            .find(|entry| row["check"].as_str() == Some(entry.id.as_str()))
        else {
            continue;
        };
        let own: Vec<&String> = changed
            .iter()
            .filter(|path| {
                entry.adapter.iter().any(|file| {
                    let file = file.trim_end_matches('/');
                    *path == file || path.starts_with(&format!("{file}/"))
                })
            })
            .collect();
        if !own.is_empty() {
            row["runs_as_main"] = json!(own);
        }
    }
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
    let Some((code, word, why)) = verdict(
        policy,
        &required,
        (seen.all, &seen.carried.waivers),
        seen.named,
        (seen.commit, seen.base_tree),
    ) else {
        return ("pass", None);
    };
    let mut detail = json!({
        "candidate": seen.candidate_id,
        "adapter_from": seen.adapter_from,
        "checks": why,
        "trust": evidence::trust(registry),
    });
    if code == Reason::CheckFailed {
        runs_as_main(policy, seen.changed, &mut detail["checks"]);
    }
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
            let state = match evidence::combine(&listed, &id, &policy.manifest_digest, Key::Tree)
            {
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

/// BACKLOG 280: a runner's pass on a box at or above a check-start load limit it sets
/// (`crate::load`) claims nothing and writes nothing: a runner on another box takes the
/// operation, or this one at its first pass below the limits. A limit that cannot be read, or a
/// load that cannot be measured, claims nothing either, and says why. A `merge --wait` runs its
/// own operation, as asked.
fn box_busy(ctx: &Ctx, op_id: &str) -> Option<BayResult> {
    if ctx.visit != Visit::Pass {
        return None;
    }
    let detail = match crate::load::busy(&ctx.repo.git) {
        Ok(None) => return None,
        Ok(Some(busy)) => busy.detail(),
        Err(error) => json!({"error": error, "recovery": crate::load::RECOVERY}),
    };
    let mut result = BayResult::refused(Decision::Queued, Reason::BoxBusy)
        .with_change(ctx.id.clone())
        .with_op(op_id)
        .with_detail(detail);
    result.waiting_for = Some("executor".to_owned());
    Some(result)
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
    let machine = detail["machine"].as_str().map(str::to_owned);
    let kept = adapter
        .as_ref()
        .and_then(|_| keeps_place(ctx, op_id, &environments));
    let op = ctx.op_mut(op_id);
    op.kept.clone_from(&kept);
    // The gap doubles only while the adapter's answer repeats.
    let repeats = match (&op.unavailable, &adapter) {
        (Some(before), Some(_)) if before.adapter.is_some() => before.repeats.saturating_add(1),
        _ => 0,
    };
    op.outcome = Some(Reason::CheckUnavailable.code().to_owned());
    if let Some(machine) = &machine
        && !op.unable.contains(machine)
    {
        op.unable.push(machine.clone());
    }
    op.unavailable = Some(Unavailable {
        environments: environments.clone(),
        adapter,
        at_ms: now_ms(),
        repeats,
    });
    let kept_at = kept.as_ref().map(|kept| kept.place);
    let worker = match &machine {
        Some(machine) => format!("{machine} could not run {}", environments.join(", ")),
        None => format!("no worker for {}", environments.join(", ")),
    };
    let message = match kept_at {
        Some(place) => {
            format!("unavailable {op_id}: {worker}; it keeps place {place} for another runner")
        }
        None => format!("unavailable {op_id}: {worker}"),
    };
    ctx.save(revision, None, &message)?;
    ctx.push_chain();
    let mut waiting = waiting;
    if let (Some(place), Some(detail)) = (kept_at, waiting.detail.as_mut()) {
        detail["kept"] = json!(place);
    }
    // G817: a wait for environments this box does not serve says whether a live runner serves
    // them all together: a runner takes it only then.
    if waiting
        .detail
        .as_ref()
        .is_some_and(|detail| detail["unavailable"].is_null())
        && crate::presence::fetch(ctx.repo)
    {
        let me = crate::presence::presence_ref(&ctx.executor_name());
        let together = crate::presence::serving_until(&ctx.repo.git, &me, &environments).is_some();
        if let Some(detail) = waiting.detail.as_mut() {
            detail["served_together"] = json!(together);
        }
    }
    Ok(waiting)
}

/// G811 (a fleet measurement, 2026-11-20: in 3.5 days on one repository 11 changes an agent's own
/// `bay merge --wait` claimed were blocked `CHECK_UNKNOWN` because the check adapter gave no
/// answer on the agent's machine, each waiting a median 700 s for a person to send it again
/// before a runner checked it; 146 own runs checked and landed at once): an own run whose check
/// gave no answer on this machine (no result, the adapter's crash or its time, an infrastructure
/// failure) hands the operation to the runners, as a runner's worker that cannot start it does
/// (G797b), once per machine, while a live runner serves its environments. A runner checks it
/// again: unknown never passes. `None` for any other refusal, a machine that handed it back
/// already, or no live runner that serves it: blocked, as before.
///
/// G915 (a fleet finding, 2026-11-25: a runner's check gave no answer in 0.3 s on its machine,
/// and the change was blocked `CHECK_UNKNOWN` on a quiet main, where nothing asks it again and
/// its author's unchanged `bay merge` is refused): a runner's pass hands it on the same way, to
/// another live runner, once per machine; only a placing visit never does.
fn handed_to_runners(
    ctx: &Ctx,
    op_id: &str,
    refusal: &BayResult,
    trunk: &str,
) -> Option<BayResult> {
    if ctx.visit == Visit::Place || refusal.reason != Some(Reason::CheckUnknown) {
        return None;
    }
    // Every gap is one the adapter gave this machine no judged answer for: no result, its
    // crash or its time (`unknown`, with why and no units), or an infrastructure failure.
    let rows = refusal.detail.as_ref()?["checks"].as_array()?;
    let own_fault = !rows.is_empty()
        && rows.iter().all(|row| {
            matches!(row["verdict"].as_str(), Some("unknown" | "infra_failure"))
                && row["why"].as_str().is_some_and(|why| !why.is_empty())
                && row["reasons"].as_array().is_none_or(Vec::is_empty)
        });
    if !own_fault {
        return None;
    }
    let machine = crate::live::machine(&ctx.repo.git).ok()?;
    if ctx.op(op_id).unable.contains(&machine) {
        return None;
    }
    let policy = policy::load_at(&ctx.repo.git, trunk).ok()??;
    let mut environments: Vec<String> = rows
        .iter()
        .filter_map(|row| row["check"].as_str())
        .flat_map(|id| {
            policy
                .checks
                .iter()
                .filter(move |entry| entry.id == id)
                .map(|entry| entry.environment.clone())
        })
        .collect();
    environments.sort();
    environments.dedup();
    if environments.is_empty() || !live_runner_serving(ctx, &environments) {
        return None;
    }
    let why = rows
        .iter()
        .find_map(|row| row["why"].as_str())
        .unwrap_or("its check gave no answer");
    let mut waiting = unavailable(
        ctx,
        op_id,
        &environments,
        Some(&format!("this machine ({machine}) could not run it: {why}")),
    );
    if let Some(detail) = waiting.detail.as_mut() {
        detail["machine"] = json!(machine);
    }
    Some(waiting)
}

/// G915: a check that gave no clear answer names the machine it ran on (`detail.checked_on`),
/// so its sentence says where; any other refusal is returned as it is.
fn checked_on(ctx: &Ctx, mut refusal: BayResult) -> BayResult {
    if refusal.reason == Some(Reason::CheckUnknown)
        && let Ok(machine) = crate::live::machine(&ctx.repo.git)
        && let Some(detail) = refusal
            .detail
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
    {
        detail.insert("checked_on".to_owned(), json!(machine));
    }
    refusal
}

/// G797: how many times a place is kept for another runner before it is released (owner ruling
/// 2026-11-19, "keep place, bounded": two hand-offs between runners, or one lease of waiting).
const HANDOFFS: u32 = 2;

/// G797 (owner ruling 2026-11-19, "keep place, bounded"; a fleet finding the same day: on a
/// shared test box a check not started within 120 s was answered `unavailable`, and its change
/// went to the back of the line): after this runner's worker answered `unavailable` for
/// `environments`, the place `op_id` keeps in line so that another live runner that serves them
/// takes it there at once: `None` (it is released, as before) when it holds no place, no other
/// live runner serves them, or it was kept there [`HANDOFFS`] times or for one lease already,
/// so a busy box never holds the line.
fn keeps_place(ctx: &Ctx, op_id: &str, environments: &[String]) -> Option<KeptPlace> {
    let place = crate::queue::Queue::read(ctx.repo).ok()?.of(op_id)?.seq;
    let now = now_ms();
    let (since_ms, handoffs) = ctx
        .op(op_id)
        .kept
        .as_ref()
        .filter(|kept| kept.place == place)
        .map_or((now, 0), |kept| (kept.since_ms, kept.handoffs));
    let lease = u64::try_from(ctx.repo.config.lease().as_millis()).unwrap_or(u64::MAX);
    (handoffs < HANDOFFS
        && now.saturating_sub(since_ms) < lease
        && live_runner_serving(ctx, environments))
    .then_some(KeptPlace {
        place,
        since_ms,
        handoffs: handoffs + 1,
    })
}

/// A fleet finding (2026-10-26): a runner whose box does not serve an environment a check of
/// `op_id` runs in leaves the operation, unclaimed and in its place, to a live runner whose box
/// serves them all, and writes nothing. Before, it claimed the queue head, found no worker and
/// released the place, which sent the change to the back of the queue (33 changes in one night).
/// `None` when this box serves them, or when no other live runner does: the operation then
/// waits for a worker as before (§ 7), so a queue never stalls on an environment no one serves.
///
/// G569 (a fleet finding, 2026-10-29: ueapozv7): a box with no check sandbox that has not
/// chosen to run checks without one runs no check (ruling R14), so it serves no environment.
/// Before, `bay merge --wait` on such a box claimed the operation, every check was
/// `infra_failure`, and the operation stayed blocked with no place, which no runner takes.
///
/// G797 (a fleet measurement, 2026-11-19: while main was red on two post checks, runners that
/// served neither environment claimed five changes on their paths, found no worker, and
/// released their places; each rejoined the back of the line and waited up to 17 minutes for a
/// claim): the red post checks this change runs before its merge (G677, [`red_gate`], which
/// the queue gate asked first) are required of it here as well.
///
/// G819 (a fleet finding, 2026-11-20: for 14 minutes nothing merged while the head needed two
/// environments no live runner served together; each runner served one, found the other served
/// elsewhere, and left the head to that runner, which did the same): the other runner must serve
/// every environment the head needs, not only those this machine lacks, since one runner runs them
/// all. When none does, this runner takes it, finds no worker, and the head leaves the line.
fn served_elsewhere(
    ctx: &Ctx,
    op_id: &str,
    revision: &str,
    trunk: &str,
) -> Result<Option<BayResult>, String> {
    let git = &ctx.repo.git;
    let Some(mut policies) = judging_policies(git, revision, trunk) else {
        return Ok(None);
    };
    // The policy at trunk is the one a candidate's red checks run under (`check_candidate`).
    if let Some(at_trunk) = policies.first_mut() {
        *at_trunk = at_trunk.judging_red(&ctx.red_ids());
    }
    let served = check::Served::read(git);
    let Ok(base) = git.run(&["merge-base", trunk, revision]) else {
        return Ok(None);
    };
    let changed = check::changed_paths(git, &base, revision)?;
    let mut required: Vec<&str> = policies
        .iter()
        .flat_map(|policy| {
            policy
                .checks
                .iter()
                .filter(|entry| !entry.is_approval() && policy.requires(entry, &changed))
        })
        .map(|entry| entry.environment.as_str())
        .collect();
    required.sort_unstable();
    required.dedup();
    let whole: Vec<String> = required
        .iter()
        .map(|environment| (*environment).to_owned())
        .collect();
    // The sandbox is probed only when this box would run a check here.
    let unsandboxed = required
        .iter()
        .any(|environment| served.serves(environment))
        .then(|| check::Sandbox::here().refusal)
        .flatten();
    let missing: Vec<String> = required
        .into_iter()
        .filter(|environment| unsandboxed.is_some() || !served.serves(environment))
        .map(str::to_owned)
        .collect();
    if missing.is_empty() {
        return Ok(None);
    }
    if !live_runner_serving(ctx, &whole) {
        return Ok(None);
    }
    let mut waiting = unavailable(ctx, op_id, &missing, None);
    if let (Some(why), Some(detail)) = (unsandboxed, waiting.detail.as_mut()) {
        detail["sandbox"] = json!(why);
    }
    Ok(Some(waiting))
}

/// Whether another runner's presence on the remote is live and declares every one of
/// `environments` ([`crate::presence::serving_until`], on the presence refs fetched now).
fn live_runner_serving(ctx: &Ctx, environments: &[String]) -> bool {
    if !crate::presence::fetch(ctx.repo) {
        return false;
    }
    let me = crate::presence::presence_ref(&ctx.executor_name());
    crate::presence::serving_until(&ctx.repo.git, &me, environments).is_some()
}

/// BACKLOG 214 (a fleet finding, 2026-11-02: ftltlvo6): a change's own run (`bay merge --wait`)
/// on a bay older than main's policy read that policy as invalid and refused the change with
/// `POLICY_INVALID`, a refusal no session was told of, and a runner merged it minutes later. A
/// policy this bay cannot read is a live runner's to judge: the run leaves the operation,
/// unclaimed and in its place, writes nothing, and says to update bay. `None` for a runner's
/// pass, when main's policy loads here, or when no other runner is live: the run then judges it
/// as before (B2), so a repository with no runner still merges.
fn policy_unread_here(ctx: &Ctx, op_id: &str, trunk: &str) -> Option<BayResult> {
    if ctx.visit != Visit::Own {
        return None;
    }
    let Err(policy::LoadError::Invalid(why)) = policy::load_at(&ctx.repo.git, trunk) else {
        return None;
    };
    if !live_runner_serving(ctx, &[]) {
        return None;
    }
    let mut result = BayResult::new(Decision::Queued)
        .with_change(ctx.id.clone())
        .with_op(op_id)
        .with_detail(json!({
            "yielded": true,
            "policy_at": trunk,
            "why": why,
            "recovery": "your bay may be older than main's policy: run bay update; a runner merges this change",
        }));
    result.waiting_for = Some("executor".to_owned());
    Some(result)
}

/// An operation waiting for a worker (§ 6, § 7) is passed over with nothing written while this
/// box does not serve what it waits for, and, after an adapter's `unavailable` answer, by the
/// runner whose worker gave it until its rejoin gap has passed ([`rejoin_gap`]). Writing
/// nothing is what lets the lease of the executor that recorded the wait lapse, so an executor
/// that serves it takes it over (§ 9a).
///
/// G797 (a fleet finding, 2026-11-19: on a shared test box a check not started within 120 s was
/// answered `unavailable`, and the change waited out the gap on every runner): the answer is
/// about that runner's worker, so another runner that serves the environments takes the
/// operation at once.
fn waits_for_worker(ctx: &Ctx, op_id: &str, revision: &str, trunk: &str) -> Option<BayResult> {
    let op = ctx.op(op_id);
    let waiting = op.unavailable.as_ref()?;
    if op.state.is_terminal() || op.state == OpState::Blocked {
        return None;
    }
    let served = check::Served::read(&ctx.repo.git);
    // G431 (a fleet finding, 2026-10-28): a wait recorded under an older policy named an
    // environment the policy at trunk no longer requires of this change, and every runner that
    // did not serve it passed the change over for hours. Only an environment still required
    // keeps the wait; a policy that cannot be read keeps every one.
    let still = required_environments(ctx, revision, trunk);
    let unserved = waiting.environments.iter().any(|environment| {
        !served.serves(environment)
            && still
                .as_ref()
                .is_none_or(|still| still.contains(environment))
    });
    // G888 (BACKLOG 278; a fleet finding, 2026-11-24: a change needed checks in two environments
    // and no live runner served both; each runner served the environment the last wait named,
    // took the operation over, found no worker for the other and named that one: 25 claims in 20
    // minutes): one runner runs every check of a candidate (G819), so a box that does not serve
    // every environment the change still needs passes over the wait too.
    let partial = still
        .as_ref()
        .is_some_and(|still| still.iter().any(|environment| !served.serves(environment)));
    let answered_here = op.owner.as_deref() == Some(ctx.executor_name().as_str());
    let early = waiting.adapter.is_some() && answered_here && {
        let gap = rejoin_gap(ctx.repo.config.lease(), waiting.repeats);
        now_ms()
            < waiting
                .at_ms
                .saturating_add(u64::try_from(gap.as_millis()).unwrap_or(u64::MAX))
    };
    if !unserved && !partial && !early {
        return None;
    }
    Some(unavailable(
        ctx,
        op_id,
        &waiting.environments,
        waiting.adapter.as_deref(),
    ))
}

/// The environments the policies that judge `revision`'s change run checks in (G431, G579);
/// `None` when that cannot be read. G797: a post check main is red on counts too: the change
/// runs it before its merge (G677), and only a box that serves it takes the change.
///
/// G818 (a fleet finding, 2026-11-20: a change waited for a box serving both its own checks'
/// environment and a red post check's; main went green on that check, and the change waited
/// on for hours, as every post check it woke counted, red or not): main's reds are read as
/// `red_gate` reads them, and a post check counts only while main is red on it. Reds that
/// cannot be read keep every environment the wait names.
fn required_environments(ctx: &Ctx, revision: &str, trunk: &str) -> Option<Vec<String>> {
    let git = &ctx.repo.git;
    let mut policies = judging_policies(git, revision, trunk)?;
    let base = git.run(&["merge-base", trunk, revision]).ok()?;
    let changed = check::changed_paths(git, &base, revision).ok()?;
    let loaded = if ctx.registry.is_some() {
        None
    } else {
        crate::registry::Registry::current(git).ok()?
    };
    let registry = ctx.registry.as_ref().or(loaded.as_ref());
    let reds = crate::red::holding(git, trunk, registry).ok()?;
    let red: Vec<String> = crate::red::blocking(&reds, &changed, &ctx.record.repair_names())
        .iter()
        .map(|red| red.check.clone())
        .collect();
    // The policy at trunk is the one a candidate's red checks run under (`check_candidate`).
    if let Some(at_trunk) = policies.first_mut() {
        *at_trunk = at_trunk.judging_red(&red);
    }
    Some(
        policies
            .iter()
            .flat_map(|policy| {
                policy
                    .checks
                    .iter()
                    .filter(|entry| policy.runs(entry, &changed))
            })
            .map(|entry| entry.environment.clone())
            .collect(),
    )
}

/// The policies a candidate of `revision` is checked by: the policy at `trunk`, and the
/// change's own policy when it raises the revision, since a change to the policy is checked
/// by both (§ 7). `None` when the policy at trunk cannot be read.
///
/// G579 (a fleet finding, 2026-10-30: 6cpulzc2): a change that added a check in an
/// environment one box served was routed by the policy at trunk alone. Every box claimed it,
/// ran the base's checks, found no worker for its own new check and sent it to the back; one
/// operation was claimed 41 times.
fn judging_policies(git: &Git, revision: &str, trunk: &str) -> Option<Vec<policy::Policy>> {
    let at_trunk = policy::load_at(git, trunk).ok()??;
    let own = policy::load_at(git, revision)
        .ok()
        .flatten()
        .filter(|own| own.revision > at_trunk.revision);
    Some(std::iter::once(at_trunk).chain(own).collect())
}

/// The verdict of `policy` over every counted record on the tree (`all`): `None` for a pass,
/// else its refusal code and word, and what `named` say about each gap. `required` is the part
/// of its manifest the candidate's diff requires (G146). Without a registry `all` holds no
/// approval: [`evidence::counted`] counts none (G245). `carried` are the waivers of earlier
/// candidates that carry onto `commit` (G638).
fn verdict(
    policy: &policy::Policy,
    required: &[policy::ManifestEntry],
    (all, carried): (&[evidence::EvidenceRecord], &[evidence::EvidenceRecord]),
    named: &[&evidence::EvidenceRecord],
    (commit, base_tree): (&str, &str),
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
        .gaps(&listed, carried, required, (commit, base_tree))
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
    // Owner ruling 2026-10-25: a failed check ends the run (`crate::check::run_all`), so each
    // required check with no record at all never started; it is named `not_run`, never passed.
    if code == Reason::CheckFailed {
        why.extend(
            gaps.iter()
                .filter(|(id, _)| !all.iter().any(|record| record.check.id == id.id))
                .map(|(id, _)| {
                    json!({"check": id.id, "verdict": "not_run",
                        "why": "a check before it failed, so the run ended before it started"})
                }),
        );
    }
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
    let landed: std::cell::OnceCell<LandedMarkers> = std::cell::OnceCell::new();
    let edits = policy::outside_edits(&ctx.repo.git, trunk, policy, &|commit| {
        let markers = if let Some(markers) = landed.get() {
            markers
        } else {
            let read = LandedMarkers::read(ctx.repo)?;
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
fn is_publication(git: &Git, markers: &LandedMarkers, commit: &str) -> Result<bool, String> {
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
    Ok(markers.of(git, change)?.as_deref() == Some(commit))
}

/// The remote's landed markers (§ 10a), read once: `Change-Id` to the commit its
/// `refs/bay/landed/<Change-Id>` names, in one `ls-remote`, and a compacted one from the record
/// its shard index keeps, one shard read for each change asked about, never every record (G921).
pub(crate) struct LandedMarkers {
    listed: std::collections::HashMap<String, String>,
}

impl LandedMarkers {
    /// One listing of the remote's landed markers, and one fetch of its shard indexes.
    ///
    /// # Errors
    ///
    /// A remote that cannot be read is an error, never "no marker": an unread provenance is
    /// never taken for either answer.
    pub(crate) fn read(repo: &Repo) -> Result<Self, String> {
        let listed = repo
            .git
            .run(&[
                "ls-remote",
                repo.remote(),
                &format!("{}*", crate::names::LANDED),
            ])
            .map_err(|error| error.message())?;
        let listed = listed
            .lines()
            .filter_map(|line| {
                let (oid, name) = line.split_once('\t')?;
                Some((
                    name.strip_prefix(crate::names::LANDED)?.to_owned(),
                    oid.to_owned(),
                ))
            })
            .collect();
        crate::compact::fetch(repo)?;
        Ok(Self { listed })
    }

    /// The commit the landed marker of `change` names, or the shard index record that stands in
    /// for it.
    ///
    /// # Errors
    ///
    /// A git fault reading the shard, or a malformed record.
    pub(crate) fn of(&self, git: &Git, change: &str) -> Result<Option<String>, String> {
        match self.listed.get(change) {
            Some(commit) => Ok(Some(commit.clone())),
            None => crate::compact::recorded(git, &format!("{}{change}", crate::names::LANDED)),
        }
    }
}

/// Renews the executor's lease while a check runs (§ 9a). G787 (owner ruling 2026-11-18): one
/// beat of its lease ref (`crate::lease`), signed by the executor and pushed alone by
/// compare-and-swap, never a commit on the change's history: the history moves only on real
/// state. A beat that fails means another holder took over; the persist step finds out. G856: a
/// beat goes out only when one is due ([`beat_every`]).
fn heartbeat(ctx: &Ctx, op_id: &str) {
    if ctx.ahead_of_turn {
        return;
    }
    let every = beat_every(&ctx.repo.config, ctx.op(op_id).owner.as_deref());
    let signer = ctx.executor_signer().ok().flatten();
    if let Some(lease) = ctx
        .lease
        .borrow_mut()
        .as_mut()
        .filter(|lease| lease.of(op_id))
    {
        lease.beat(ctx.repo, signer.as_ref(), every);
    }
}

/// G845: the holder's lease names `members` (operation ids, `op_id` first) as what its check of
/// `op_id` covers (`crate::lease::Holding::checks`). G867: when a place outside `members` stands
/// in the line, the beat that says so goes out now, before the check starts, since that place
/// passes the head only once the lease names its batch ([`batch_behind`]). With none, the next
/// due beat says it.
fn names_batch(ctx: &Ctx, op_id: &str, members: Vec<String>) {
    let waited_on = crate::queue::Queue::local(&ctx.repo.git).is_ok_and(|queue| {
        queue
            .line()
            .iter()
            .any(|entry| !members.contains(&entry.op_id))
    });
    if let Some(lease) = ctx
        .lease
        .borrow_mut()
        .as_mut()
        .filter(|lease| lease.of(op_id))
    {
        lease.checks(members);
    }
    if waited_on {
        heartbeat(ctx, op_id);
    }
}

/// G679 (a fleet finding, 2026-11-08: twice in one hour a change's own `bay merge` claimed its
/// operation, recorded its check and stopped, and both runners watched the dead claim for the
/// full 450 s lease, running nothing else): how long a holder's chain and lease may stay quiet
/// before its claim is taken over. A runner holds the configured lease, which covers its longest quiet
/// step. A change's own run, whose owner is bay's default name for a clone that is no runner,
/// holds six heartbeats: it beats its lease while its check runs, and its other steps are
/// short. Taking over a live one costs a repeated check, never a second landing: the claim
/// raises the epoch, and a holder whose epoch moved never publishes (§ 9a).
#[must_use]
pub fn holder_lease(config: &crate::config::Config, owner: Option<&str>) -> Duration {
    let lease = config.lease();
    if owner == Some(crate::signing::UNNAMED) {
        lease.min(config.heartbeat() * 6)
    } else {
        lease
    }
}

/// How many beats of a holder's lease fit in the lease a reader grants it (G856).
const BEATS_PER_LEASE: u32 = 8;

/// G856 (a fleet measurement, 2026-11-22: runners holding a 450 s lease pushed a beat each
/// 10 s heartbeat of a check, a median 40 s, at about 5 s a push, for a lease no reader judges
/// quiet before 450 s): how often a holder beats its lease while it checks. An eighth of the lease
/// readers grant it ([`holder_lease`]), and never more often than once a heartbeat, so a lease
/// under eight heartbeats (the default, 60 s at 10 s) is beaten each heartbeat as before. A
/// record that changed (G845's batch) goes out at the next heartbeat whatever the time.
#[must_use]
pub fn beat_every(config: &crate::config::Config, owner: Option<&str>) -> Duration {
    (holder_lease(config, owner) / BEATS_PER_LEASE).max(config.heartbeat())
}

/// G696: a looping runner's sightings of the chains other runners hold, by change: the head and
/// lease it saw ([`crate::lease::watched`]) and when it first saw them. `None` until the runner
/// loops ([`watch_leases_across_passes`]).
static SIGHTINGS: std::sync::Mutex<Option<std::collections::HashMap<String, (String, Instant)>>> =
    std::sync::Mutex::new(None);

/// G799: whether this process is a looping runner, which watches leases across its passes.
fn watches_leases_across_passes() -> bool {
    SIGHTINGS.lock().is_ok_and(|sightings| sightings.is_some())
}

/// G696: this process is a looping runner; a held lease is watched across its passes.
pub fn watch_leases_across_passes() {
    if let Ok(mut sightings) = SIGHTINGS.lock()
        && sightings.is_none()
    {
        *sightings = Some(std::collections::HashMap::new());
    }
}

/// G696: this looping runner's sighting of the holder of `change`, whose history is at `head`:
/// its history and lease as this store holds them ([`crate::lease::watched`]), noted, and when
/// it first saw them as they are. A holder whose sighting stood a lease is one whose lease
/// expired. G805: each pass renews it for an operation it holds from the leases the pass
/// fetched, with no visit. `None` when this process does not loop.
pub fn sighted(git: &Git, change: &str, head: &str) -> Option<Instant> {
    let mut sightings = SIGHTINGS.lock().ok()?;
    let seen = sightings.as_mut()?;
    let now = Instant::now();
    let watched = crate::lease::watched(git, change, head);
    let sighting = seen
        .entry(change.to_owned())
        .or_insert_with(|| (watched.clone(), now));
    if sighting.0 != watched {
        *sighting = (watched, now);
    }
    Some(sighting.1)
}

/// Watches an operation another executor holds for `lease` on this process's monotonic
/// clock, from its own first sighting (§ 9a). Returns whether the lease expired: neither the
/// chain head nor the holder's lease ref changed (G787, [`crate::lease::watched`]). Any change
/// means the owner is alive.
///
/// G696 (a fleet finding, 2026-11-11: every runner but the head's holder spent 20 to 35 s of
/// each pass, and up to 30 fetches, watching the holder's chain until its next heartbeat). A
/// looping runner's pass never waits here: it notes when it first saw this head and judges the
/// lease against that on each later pass. Any other caller watches as before.
fn lease_expired(ctx: &mut Ctx, lease: Duration) -> bool {
    let watched = |ctx: &Ctx| crate::lease::watched(&ctx.repo.git, &ctx.id, &ctx.head);
    if ctx.visit == Visit::Pass
        && let Some(seen) = sighted(&ctx.repo.git, &ctx.id, &ctx.head)
    {
        return seen.elapsed() >= lease;
    }
    let first = watched(ctx);
    let seen = Instant::now();
    // G749 (a fleet finding, 2026-11-16: each `merge --wait` behind a held operation fetched its
    // chain five times a second, about 3,000 to 7,700 Git commands a wait, while the holder moves
    // it once a heartbeat): the chain and its lease are read again once a heartbeat, never
    // faster, in one fetch ([`Repo::remote_chain`]).
    let every = ctx
        .repo
        .config
        .heartbeat()
        .max(Duration::from_millis(200))
        .min(lease);
    while seen.elapsed() < lease {
        std::thread::sleep(every);
        if ctx.refetch().is_ok() && watched(ctx) != first {
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
    let counted = policy.answering(evidence::counted(
        git,
        evidence::records_for(git, judged.tree)?,
        ctx.registry.as_ref(),
    )?);
    // N-9: a check waived on this exact candidate runs no more: the waiver answers it, never as
    // a pass (`coverage_gaps`). G635: a run again of a check that gave no clear answer wrote a
    // second record under its first run's id, and the landing failed.
    let waived: Vec<evidence::CheckId> = policy
        .manifest
        .iter()
        .map(|entry| evidence::CheckId::new(&entry.id, &entry.environment))
        .filter(|id| policy.waiver(&counted, &[], id, judged.commit).is_some())
        .collect();
    let (resumed, others): (Vec<_>, Vec<_>) = counted
        .into_iter()
        .partition(|record| record.id.contains(&mine));
    let digest = &policy.manifest_digest;
    let mut finished = Vec::new();
    let mut reused = Vec::new();
    for entry in &policy.manifest {
        let id = evidence::CheckId::new(&entry.id, &entry.environment);
        let key = policy.key(&id, (judged.commit, Some(judged.base_tree)));
        if answered(&resumed, &id, digest, key) || waived.contains(&id) {
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
        if answered(&candidates, &id, digest, key) {
            reused.extend(candidates.into_iter().filter(|record| record.check == id));
            finished.push(id);
            continue;
        }
        // NEW-10: a check keyed by its read set carries the record of an earlier candidate of
        // this operation whose read set is the same, byte for byte (`crate::throughput`).
        // Items 1 and 2: an observed footprint is exact by itself, so it carries under
        // exact-inputs reuse with no lanes key.
        if key == Key::Tree
            && (ctx.repo.config.footprint_lanes()
                || check.footprint == policy::FootprintMode::Observed)
        {
            let carried = carry_over(ctx, op_id, policy, judged, check)?;
            if answered(&carried, &id, digest, Key::Tree) {
                reused.extend(carried);
                finished.push(id);
            }
        }
    }
    Ok((finished, reused))
}

/// NEW-10 footprint-keyed evidence (`crate::throughput`): the records of check `entry`, which
/// declares `footprint = true`, on this operation's earlier candidates and pre-checks, and on
/// the candidate's own tree checked on another base (G713, a batch's bisection), whose
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
        // Items 1 and 2: main's own tip, the candidate's base, where the last landing's records
        // and the runner's warm records sit, for an observed footprint.
        .chain((entry.footprint == policy::FootprintMode::Observed).then_some(judged.base_tree))
        // G713: the candidate's own tree, checked on another base. A batch's bisection checks
        // each prefix on the batch's base; once the prefix before its culprit lands, the
        // culprit's candidate is that failing prefix's tree, exactly, on a base the prefix
        // moved. Its record then answers the culprit here when the read set is the same in
        // both bases, instead of a run alone that repeats it.
        .chain(std::iter::once(judged.tree))
        // G796: a candidate of this visit whose checks main's move stopped; the checks it
        // carries finished there.
        .chain(ctx.stopped_trees.iter().map(String::as_str))
        .collect();
    let observed = entry.footprint == policy::FootprintMode::Observed;
    let declared = throughput::stamp(git, entry, judged.tree, judged.base_tree);
    if declared.is_none() && !observed {
        return Ok(Vec::new());
    }
    let trees_now = (judged.tree, judged.base_tree);
    let id = evidence::CheckId::new(&entry.id, &entry.environment);
    let index = policy
        .checks
        .iter()
        .position(|check| check.id == entry.id && check.environment == entry.environment)
        .map_or(0, |at| at + 1);
    let holds = |record: &&evidence::EvidenceRecord| {
        // A record on these very inputs is exact reuse's, and a carried one is never carried
        // again.
        (record.subject.tree != judged.tree || record.subject.base_tree != judged.base_tree)
            && record.kind == evidence::KIND_CHECK
            && record.check == id
            && record.manifest_digest == policy.manifest_digest
            && record.policy_revision == policy.revision
            && record.policy_provenance.as_deref() == Some(judged.provenance.as_str())
            && record.fixtures().as_deref() == Some(entry.fixtures.as_slice())
            && match &declared {
                Some(stamp) => throughput::same_stamp(record, stamp),
                // A record whose read set cannot be read is no match.
                None => throughput::same_observed(git, record, trees_now).unwrap_or(false),
            }
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
            // The carried record names the footprint as it is in this candidate's trees.
            let stamp = match &declared {
                Some(stamp) => stamp.clone(),
                None => throughput::restamp(git, record, trees_now)?,
            };
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

/// G859: the checks the candidate `commit`, judged against a base whose tree is `base_tree`,
/// runs whose records, reused or carried in place of a run, already answer `product_failure`
/// (`evidence::combine`, as the judgement reads them): a failure known before the run starts. A
/// check the candidate does not run (`Policy::runs`) is never one.
fn known_failures(
    policy: &policy::Policy,
    reused: &[evidence::EvidenceRecord],
    changed: &[String],
    (commit, base_tree): (&str, &str),
) -> Vec<evidence::CheckId> {
    policy
        .checks
        .iter()
        .filter(|entry| policy.runs(entry, changed))
        .map(|entry| evidence::CheckId::new(&entry.id, &entry.environment))
        .filter(|id| {
            let key = policy.key(id, (commit, Some(base_tree)));
            matches!(
                evidence::combine(reused, id, &policy.manifest_digest, key),
                Combined::ProductFailure
            )
        })
        .collect()
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

/// Whether `records` answer check `id` under `digest` and its `key`: they combine to a
/// verdict, never `unknown`.
fn answered(
    records: &[evidence::EvidenceRecord],
    id: &evidence::CheckId,
    digest: &str,
    key: Key,
) -> bool {
    matches!(
        evidence::combine(records, id, digest, key),
        Combined::Pass | Combined::ProductFailure | Combined::Flaky
    )
}

/// Step 3: the candidate commit, signed by the publishing executor when signing is on.
fn commit_candidate(
    ctx: &Ctx,
    tree: &str,
    trunk: &str,
    message: &str,
    revision: &str,
) -> Result<String, String> {
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
    // A draft a person wrote (a branch made outside bay) keeps its author on main's commit;
    // bay, or the runner's key, stays the committer.
    let author = git
        .run(&[
            "log",
            "-1",
            "--format=%an%x00%ae%x00%ad",
            "--date=raw",
            revision,
        ])
        .map_err(|error| error.message())?;
    let mut parts = author.splitn(3, '\0');
    let (name, email, date) = (
        parts.next().unwrap_or_default().to_owned(),
        parts.next().unwrap_or_default().to_owned(),
        parts.next().unwrap_or_default().to_owned(),
    );
    let mut envs: Vec<(&str, &str)> = identity
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    if !email.is_empty() && !crate::change::is_record_author(&email) {
        envs.retain(|(key, _)| !key.starts_with("GIT_AUTHOR"));
        envs.extend([
            ("GIT_AUTHOR_NAME", name.as_str()),
            ("GIT_AUTHOR_EMAIL", email.as_str()),
            ("GIT_AUTHOR_DATE", date.as_str()),
        ]);
    } else if let Some((who, address)) = ctx.repo.config.merge_author() {
        // G878: `[runner] author` writes bay's own commits on main; the committer stays bay's.
        envs.retain(|(key, _)| !matches!(*key, "GIT_AUTHOR_NAME" | "GIT_AUTHOR_EMAIL"));
        envs.extend([("GIT_AUTHOR_NAME", who), ("GIT_AUTHOR_EMAIL", address)]);
    }
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
    // G702 (a fleet finding, 2026-11-12: main's title kept an intent the work had outgrown,
    // and contradicted the body): the agent's own first line titles main's commit, written when
    // the work was done; the intent titles it only when the agent wrote no message. The intent
    // stays in its trailer.
    let (title, body) = match text.split_once('\n') {
        Some((first, rest)) => (first.trim(), rest.trim()),
        None => (text.trim(), ""),
    };
    let mut out = if title.is_empty() {
        format!("{subject}\n\n")
    } else {
        format!("{title}: change {}\n\n", ctx.id)
    };
    if !body.is_empty() {
        out.push_str(body);
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

/// Pushes the producer's notes ref. A push the remote took holds exactly that commit: Git reports
/// a ref update only once the remote made it, so nothing is read back (BACKLOG 92).
///
/// G900: a ref lock another process holds on the remote is waited out and the push tried again,
/// up to [`PUBLISH_RETRY_LIMIT`] tries; it is never read as a collision, and bay never unlinks it
/// (G128). A lock held through every try fails the push and says so.
fn push_evidence(repo: &Repo, producer: &str) -> Result<(), String> {
    let notes = evidence::notes_ref(producer);
    let spec = format!("{notes}:{notes}");
    let mut tries = 0;
    loop {
        // #123: no post piece writes the notes between the read of the local ref and the push.
        let writing = evidence::writing();
        if repo
            .git
            .ref_target(&notes)
            .map_err(|error| error.message())?
            .is_none()
        {
            // This producer wrote nothing: every record the pass rests on was resumed from
            // another producer's ref, fetched from the remote (a takeover, G22).
            return Ok(());
        }
        let Err(error) = repo.git.run(&["push", "-q", repo.remote(), &spec]) else {
            return Ok(());
        };
        drop(writing);
        let message = error.message();
        tries += 1;
        if held_lock(&message) {
            if tries < PUBLISH_RETRY_LIMIT {
                std::thread::sleep(HOOK_RETRY_WAIT);
                continue;
            }
            return Err(format!(
                "evidence could not be pushed: another process held the remote's lock on {notes} through {tries} tries: {message}"
            ));
        }
        if message.contains("non-fast-forward")
            || message.contains("fetch first")
            || message.contains("rejected")
        {
            // § 7: one producer name used by two executors is a collision.
            return Err(format!(
                "producer {producer} collides with another runner: the remote's {notes} holds records this runner never wrote; give each runner its own bay.producerName"
            ));
        }
        return Err(format!("evidence could not be pushed: {message}"));
    }
}

/// G864: what rides in a publishing push beside trunk and its markers: each ref's lease, and its
/// refspec.
#[derive(Default)]
struct Riding {
    leases: Vec<String>,
    specs: Vec<String>,
}

impl Riding {
    /// G864: `ctx`'s chain, on the head this process holds, leased on the head it last pushed.
    fn chain(&mut self, ctx: &Ctx) {
        let name = change_ref(&ctx.id);
        if let Some(expected) = &ctx.pushed_head {
            self.leases
                .push(format!("--force-with-lease={name}:{expected}"));
        }
        self.specs.push(format!("{}:{name}", ctx.head));
    }

    /// G867: the release of the lease `ctx`'s beats pushed for `op_id`, when they pushed one: it
    /// rides the push that lands it, so the next head's claim never waits on a push of its own.
    /// Returns whether it rides.
    fn lease(&mut self, ctx: &Ctx, op_id: &str) -> bool {
        let release = ctx
            .lease
            .borrow()
            .as_ref()
            .filter(|lease| lease.of(op_id))
            .and_then(|lease| lease.riding_release(&ctx.repo.git));
        let Some((expected, spec)) = release else {
            return false;
        };
        self.leases.push(expected);
        self.specs.push(spec);
        true
    }

    /// G897: the evidence notes of `ctx`'s checked candidate, while its checked record rides this
    /// push ([`persist_attempt`]): the host's hook judges trunk on the notes the push carries
    /// (`crate::hook`), so they need no push of their own before it. Returns whether they ride.
    ///
    /// # Errors
    ///
    /// The notes ref cannot be read.
    fn evidence(&mut self, ctx: &Ctx) -> Result<bool, String> {
        let Some(producer) = &ctx.evidence_rides else {
            return Ok(false);
        };
        let notes = evidence::notes_ref(producer);
        if ctx
            .repo
            .git
            .ref_target(&notes)
            .map_err(|error| error.message())?
            .is_none()
        {
            return Ok(false);
        }
        self.specs.push(format!("{notes}:{notes}"));
        Ok(true)
    }

    /// G869: the landed marker of each change `ctx`'s operation lands after that the remote
    /// holds none for ([`Ctx::unmarked`]), at its publication, which the push's trunk reaches.
    /// Each is create-only: a marker another writer made first refuses the push, and the
    /// operation's next visit reads it.
    fn unmarked(&mut self, ctx: &Ctx) {
        for (change, commit) in ctx.unmarked.borrow().iter() {
            let name = format!("{}{change}", crate::names::LANDED);
            let spec = format!("{commit}:{name}");
            if self.specs.contains(&spec) {
                continue;
            }
            self.leases.push(format!("--force-with-lease={name}:"));
            self.specs.push(spec);
        }
    }
}

/// G867: the push that landed `ctx`'s operation took its lease's release ([`Riding::lease`]).
fn lease_released(ctx: &Ctx) {
    if let Some(lease) = ctx.lease.borrow_mut().as_mut() {
        lease.released_riding(&ctx.repo.git);
    }
}

/// Test builds keep a landing's records apart from its push on request, as a refused push leaves
/// them (G809, G863).
fn records_apart() -> bool {
    TEST_HOOKS && std::env::var_os("BAY_TEST_LAND_APART").is_some()
}

/// G864 (a fleet measurement, 2026-11-23: a landing made four pushes in a row at about 5.5 s
/// each, its claim, its checked state, its publishing push and its records, and read its chain
/// again before the publishing one): the landing's own records ride in its publishing push. The
/// chain, with the operation recorded published, and this executor's log, with its terminal
/// record, move in the one atomic push that moves trunk and creates the landed marker. The
/// chain goes on the head this process last pushed, leased on it, so the push stands for the read
/// before it ([`still_owner`]): no other write reached the chain since this process's own, so no
/// other executor took the operation over, and no newer request or cancel came in.
///
/// Returns the landing; or, when the remote refused the push or a record could not be written,
/// why, once every record is taken back off this box (the chain ref, the record and the log),
/// so the caller reads the chain again and answers the refusal ([`publish_checked`]).
///
/// # Errors
///
/// The view's journal cannot be written once the landing is pushed.
fn land_with_records(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    commit: &str,
    release: Option<&(u64, String)>,
    claim: Option<&crate::change::IntentClaim>,
) -> Result<Result<BayResult, String>, String> {
    if records_apart() {
        return Ok(Err("the test keeps the landing's records apart".to_owned()));
    }
    kill_point("before-publish");
    let marked = match mark_landed(ctx, op_id, revision, commit) {
        Ok(marked) => marked,
        Err(why) => return Ok(Err(why)),
    };
    let repo = ctx.repo;
    let executor = ctx.executor_name();
    let signer = ctx.executor_signer().ok().flatten();
    let mut riding = Riding::default();
    riding.chain(ctx);
    riding.unmarked(ctx);
    let leased = riding.lease(ctx, op_id);
    // G897: and the candidate's evidence, whose checked record rides on the chain. #123: no post
    // piece writes the notes between the read of the local ref and the push.
    let writing = ctx.evidence_rides.is_some().then(evidence::writing);
    let notes = match riding.evidence(ctx) {
        Ok(notes) => notes,
        Err(why) => {
            taken_back(ctx, &marked.before, marked.record);
            return Ok(Err(why));
        }
    };
    let id = ctx.id.clone();
    let terminal = std::slice::from_ref(&marked.terminal);
    let pushed = crate::log::ride(repo, &executor, signer.as_ref(), terminal, |log| {
        riding.specs.push(log.to_owned());
        kill_point("landing-marked");
        publish_push(repo, &id, commit, release, claim, &riding)
    });
    drop(writing);
    match pushed {
        Ok(Ok(_)) => {}
        Ok(Err(why)) | Err(why) => {
            taken_back(ctx, &marked.before, marked.record);
            return Ok(Err(why));
        }
    }
    if notes {
        ctx.evidence_rides = None;
    }
    kill_point("after-publish");
    if leased {
        lease_released(ctx);
    }
    chain_pushed(ctx);
    recorded(ctx, op_id, revision, commit, &marked.behind).map(Ok)
}

/// G864: a landing's records as this box holds them before its push: the head and the record
/// [`taken_back`] restores, the operations the landing ends, and its terminal record.
struct Marked {
    before: String,
    record: ChangeRecord,
    behind: Vec<String>,
    terminal: serde_json::Value,
}

/// G864: makes the record of `op_id`'s landing of `commit`, the operation published and the change
/// landed, on this process's head, and builds its terminal record; or why not, with nothing left
/// written, when another process moved the chain ref or a record could not be written. G875: the
/// chain ref takes the record only once the landing's push took it ([`Ctx::save_riding`]).
fn mark_landed(ctx: &mut Ctx, op_id: &str, revision: &str, commit: &str) -> Result<Marked, String> {
    let before = ctx.head.clone();
    let record = ctx.record.clone();
    let behind = under_way_beside(ctx, op_id);
    ctx.record.state = ChangeState::Landed;
    let op = ctx.op_mut(op_id);
    op.state = OpState::Published;
    op.outcome = None;
    end_behind(ctx, &behind);
    match ctx.save_riding(revision, Some(commit), &format!("landed {op_id}")) {
        Ok(true) => {}
        Ok(false) => {
            ctx.record = record;
            return Err(CHAIN_MOVED.to_owned());
        }
        Err(why) => {
            ctx.record = record;
            return Err(why);
        }
    }
    // BACKLOG 99: the record stands only if this push lands the operation, and then no other
    // runner holds a terminal record of it, so no log is fetched to look for one. A refusal
    // takes the mark back with the record.
    ctx.pushed_landing = Some(commit.to_owned());
    match terminal_record(ctx, op_id, "published", Some(commit)) {
        Ok(Some(terminal)) => Ok(Marked {
            before,
            record,
            behind,
            terminal,
        }),
        Ok(None) => {
            taken_back(ctx, &before, record);
            Err("no terminal record was written".to_owned())
        }
        Err(why) => {
            taken_back(ctx, &before, record);
            Err(why)
        }
    }
}

/// G864: a landing whose push took its chain: the chain stands pushed, as [`Ctx::push_chain`]
/// leaves it.
fn chain_pushed(ctx: &mut Ctx) {
    ctx.took();
}

/// G864: the landing of `commit` once its push took its records: the view's journal says
/// published, the operations it ended are told (G823), and the landing is answered.
///
/// # Errors
///
/// The view's journal cannot be written.
fn recorded(
    ctx: &Ctx,
    op_id: &str,
    revision: &str,
    commit: &str,
    behind: &[String],
) -> Result<BayResult, String> {
    if view::journal(ctx.repo, &ctx.id).is_some() {
        view::set_journal(ctx.repo, &ctx.id, view::Journal::Published)?;
    }
    tell_behind(ctx, behind, revision, commit);
    Ok(landing_row(ctx, op_id, revision, commit))
}

/// G864: a landing whose push did not take its records: this process's head goes back to
/// `before`, the head the records were made on, and the record to `record`, as they stood before
/// it. G875: the chain ref never took them, so no other process on this box holds them.
fn taken_back(ctx: &mut Ctx, before: &str, record: ChangeRecord) {
    ctx.withdraw(before);
    ctx.record = record;
    ctx.pushed_landing = None;
}

/// Step 6: one atomic push of trunk and the landed marker. Never forced.
fn publish_push(
    repo: &Repo,
    id: &str,
    commit: &str,
    release: Option<&(u64, String)>,
    claim: Option<&crate::change::IntentClaim>,
    riding: &Riding,
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
    args.extend(riding.leases.iter().map(String::as_str));
    args.extend([repo.remote(), trunk.as_str(), landed.as_str()]);
    if let Some(done) = &done {
        // § 6: trunk, the head's release and the landed marker move in one transaction.
        args.push(done);
    }
    if let Some(landed_intent) = &landed_intent {
        args.push(landed_intent);
    }
    args.extend(riding.specs.iter().map(String::as_str));
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
    // G421 (a fleet finding, 2026-10-27): trunk held the change from a push around bay, its
    // Change-Id and no Revision-Id. The refusal was written nowhere, so every runner ran it
    // again each pass and its place held the whole line. The refusal ends the operation, and
    // the sweep releases its place.
    if let Some(op_id) = ctx
        .record
        .operations
        .iter()
        .find(|op| {
            op.revision == revision && !op.state.is_terminal() && op.state != OpState::Blocked
        })
        .map(|op| op.op_id.clone())
    {
        let op = ctx.op_mut(&op_id);
        op.state = OpState::Blocked;
        op.outcome = Some(Reason::ChangeAlreadyLanded.code().to_owned());
        ctx.save(
            revision,
            None,
            &format!("blocked {op_id}: the change is already on main"),
        )?;
        ctx.push_chain();
    }
    // G790 (a fleet finding, 2026-11-19): another revision of the change is on trunk, so the
    // change has ended, but its record stayed `ready`. Every reader counted it live, and each
    // later visit refused it again and wrote one more receipt (133 on one change in three
    // weeks). The record ends as landed, with the revision that did land published.
    if ctx.record.state.is_live() {
        ctx.record.state = ChangeState::Landed;
        if let Some(op_id) = ctx
            .record
            .operations
            .iter()
            .find(|op| op.revision == publication.revision)
            .map(|op| op.op_id.clone())
        {
            let op = ctx.op_mut(&op_id);
            op.state = OpState::Published;
            op.outcome = None;
        }
        ctx.save(
            revision,
            Some(&publication.commit),
            &format!("landed {}: an earlier revision is on main", ctx.id),
        )?;
        ctx.push_chain();
    }
    Ok(Some(
        BayResult::blocked(Reason::ChangeAlreadyLanded)
            .with_change(ctx.id.clone())
            .with_detail(
                json!({"landed": publication.commit, "landed_revision": publication.revision}),
            ),
    ))
}

/// G931: how long a landing's record stands on its chain, unmarked and with main lacking its
/// commit, before its change opens again. A landing push under way (a batch's re-push included)
/// ends in seconds; its record is never reopened under it.
const REOPEN_AFTER_S: u64 = 600;

/// G875 (a fleet finding, 2026-11-23: `bay merge --wait` answered a change merged from its
/// chain's record, which another process had pushed before the landing's own push was refused;
/// main never held the commit): [`derive_landed`] for a landing read from the change's record
/// alone, once main holds its commit. One main does not hold is answered `MERGE_OUTCOME_UNKNOWN`,
/// never merged.
///
/// G931 (a fleet finding, 2026-11-27: one landing batch of six changes left every member's
/// chain saying landed, while main, fetched again, never held the batch and the remote held no
/// landed marker of any member; each change kept its intent, so neither it nor a new change
/// could carry that intent on): a landing main lacks once fetched again, which the remote marks
/// nowhere, and whose record is older than [`REOPEN_AFTER_S`], never happened. Its operation
/// ends failed `MERGE_OUTCOME_UNKNOWN`, the change is ready again, and `None` tells the land
/// to go on as for any ready change. Only the author's land reopens: a runner's pass never does.
///
/// # Errors
///
/// As [`derive_landed`]; the reopened record could not be saved.
fn landed_if_on_main(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    commit: &str,
) -> Result<Option<BayResult>, String> {
    let unknown = |ctx: &Ctx, reopens_in: Option<u64>| {
        let mut detail = json!({"recorded_merged": commit, "on_main": false});
        if let Some(seconds) = reopens_in {
            detail["reopens_in_s"] = json!(seconds);
        }
        Ok(Some(
            BayResult::blocked(Reason::PublishOutcomeUnknown)
                .with_change(ctx.id.clone())
                .with_op(op_id)
                .with_detail(detail),
        ))
    };
    let repo = ctx.repo;
    let holds = |tip: &str| repo.git.is_ancestor(commit, tip);
    let tracking = repo
        .git
        .ref_target(&repo.trunk_tracking_ref())
        .ok()
        .flatten();
    if tracking
        .as_deref()
        .is_some_and(|tip| holds(tip).unwrap_or(false))
    {
        return derive_landed(ctx, op_id, revision, commit).map(Some);
    }
    // Main as the remote holds it now: a fetch or a read that fails says nothing.
    let fetched = repo.refetch_trunk().ok();
    match fetched.as_deref().map(holds) {
        Some(Ok(true)) => return derive_landed(ctx, op_id, revision, commit).map(Some),
        Some(Ok(false)) if ctx.online => {}
        _ => return unknown(ctx, None),
    }
    if !matches!(crate::undo::marker(repo, &ctx.id), Ok(None)) {
        return unknown(ctx, None);
    }
    let recorded = repo
        .git
        .run(&["show", "-s", "--format=%ct", &ctx.head])
        .ok()
        .and_then(|time| time.trim().parse::<u64>().ok());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    match recorded.map(|at| now.saturating_sub(at)) {
        Some(age) if age >= REOPEN_AFTER_S => {}
        Some(age) => return unknown(ctx, Some(REOPEN_AFTER_S - age)),
        None => return unknown(ctx, None),
    }
    let op = ctx.op_mut(op_id);
    op.state = OpState::Failed;
    op.outcome = Some(Reason::PublishOutcomeUnknown.code().to_owned());
    ctx.record.state = ChangeState::Ready;
    let message = format!(
        "reopened {}: main never took {op_id}'s landing {commit}",
        ctx.id
    );
    ctx.save_or_moved(revision, None, &message)?;
    if !ctx.push_chain() {
        return Err(CHAIN_MOVED.to_owned());
    }
    Ok(None)
}

/// G875: whether main holds `commit`: as this store's tracking ref holds main, else as the
/// remote's main, fetched once.
pub(crate) fn main_holds(repo: &Repo, commit: &str) -> bool {
    let holds = |tip: &str| repo.git.is_ancestor(commit, tip).unwrap_or(false);
    repo.git
        .ref_target(&repo.trunk_tracking_ref())
        .ok()
        .flatten()
        .is_some_and(|tip| holds(&tip))
        || repo.refetch_trunk().is_ok_and(|tip| holds(&tip))
}

/// Step 7: derive "landed" from trunk, record it, and push the chain (idempotent).
fn derive_landed(
    ctx: &mut Ctx,
    op_id: &str,
    revision: &str,
    commit: &str,
) -> Result<BayResult, String> {
    let behind = under_way_beside(ctx, op_id);
    if ctx.op(op_id).state != OpState::Published
        || ctx.record.state != ChangeState::Landed
        || !behind.is_empty()
    {
        ctx.record.state = ChangeState::Landed;
        let op = ctx.op_mut(op_id);
        op.state = OpState::Published;
        op.outcome = None;
        end_behind(ctx, &behind);
        ctx.save(revision, Some(commit), &format!("landed {op_id}"))?;
    }
    if view::journal(ctx.repo, &ctx.id).is_some() {
        view::set_journal(ctx.repo, &ctx.id, view::Journal::Published)?;
    }
    // G789: the terminal record rides in the chain's push, one round trip ahead of the next
    // head's take, not two.
    match terminal_record(ctx, op_id, "published", Some(commit))? {
        Some(record) => ctx.push_chain_with_record(&record)?,
        None => {
            ctx.push_chain();
        }
    }
    tell_behind(ctx, &behind, revision, commit);
    Ok(landing_row(ctx, op_id, revision, commit))
}

/// G823 (a fleet finding, 2026-11-20: a batch merged a member's checked revision while its
/// author's newer revision waited with no place in line, and status said it waited its turn):
/// the operations of the change other than `op_id` that are still under way when `op_id` merges.
fn under_way_beside(ctx: &Ctx, op_id: &str) -> Vec<String> {
    ctx.record
        .operations
        .iter()
        .filter(|op| op.op_id != op_id && op.state.next().is_some())
        .map(|op| op.op_id.clone())
        .collect()
}

/// G823: each operation in `behind` ends `CHANGE_ALREADY_MERGED` with the merge of another
/// revision, as G421 ends one at its visit. Its author's next `bay merge` in the view sends the
/// edits it holds since the merged revision as the next change ([`carry`], [`follow_up`]).
fn end_behind(ctx: &mut Ctx, behind: &[String]) {
    for id in behind {
        let op = ctx.op_mut(id);
        op.state = OpState::Blocked;
        op.outcome = Some(Reason::ChangeAlreadyLanded.code().to_owned());
        op.unavailable = None;
    }
}

/// G823: the event of each operation [`end_behind`] ended, which wakes its author once (G307).
/// No session waits on it: the merge that ended it was another operation's.
fn tell_behind(ctx: &Ctx, behind: &[String], revision: &str, commit: &str) {
    for id in behind {
        let result = BayResult::blocked(Reason::ChangeAlreadyLanded)
            .with_change(ctx.id.clone())
            .with_op(id.clone())
            .with_detail(json!({"landed": commit, "landed_revision": revision}));
        if let Ok(Some(event)) = outcome_event(ctx, id, &result) {
            // A lost notice is never retried: `bay status --since` reads the event from the log.
            let _ = crate::notify::notify(ctx.repo, &event);
        }
    }
}

/// The result of `op_id`'s landing of `commit`, recorded on the change's chain: what a landing
/// says, derived from the chain, main's policy and the evidence ([`derive_landed`]).
fn landing_row(ctx: &Ctx, op_id: &str, revision: &str, commit: &str) -> BayResult {
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
    // G677: the red checks the landing ran before its merge, judged against main.
    if let Some(detail) = result.detail.as_mut()
        && !ctx.red_checks.is_empty()
    {
        detail["red_checked"] = json!(ctx.red_ids());
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
    result
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
    let judged = attempt_policies(git, attempt)?;
    if judged.is_empty() {
        return Ok(Vec::new());
    }
    // A batch member's evidence is about the batch tip's tree (`crate::throughput`).
    let tree = attempt
        .batch
        .as_ref()
        .map_or(&attempt.tree, |batch| &batch.tree);
    // A batch tip is judged at the batch's base, over the batch's whole diff
    // (`policy::CheckEntry::key`).
    let base_tree = tree_of(
        git,
        attempt
            .batch
            .as_ref()
            .map_or(&attempt.base, |batch| &batch.base),
    )?;
    let subject = (attempt.commit.as_str(), base_tree.as_str());
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
        .flat_map(|policy| baseline_gaps(policy, &records, subject))
    {
        if !gaps.contains(&gap) {
            gaps.push(gap);
        }
    }
    // A flaky check that landed under `allow_flaky` is named with every run, never as a pass.
    for gap in judged
        .iter()
        .flat_map(|policy| flaky_gaps(policy, &records, subject))
    {
        if !gaps.contains(&gap) {
            gaps.push(gap);
        }
    }
    // N-9: a waivable check that landed on a person's waiver is named with the waiver and its
    // real verdict, never as a pass.
    let changed = check::changed_paths(git, &attempt.base, &attempt.commit)?;
    let earlier = throughput::candidates_of(&ctx.record);
    for policy in &judged {
        // G638: a waiver of an earlier candidate that carried onto this one is named with it.
        let carried = if attempt.batch.is_none() {
            throughput::carried_waivers(
                git,
                policy,
                ctx.registry.as_ref(),
                (&attempt.commit, &attempt.tree, &base_tree),
                &records,
                &earlier,
            )?
        } else {
            Vec::new()
        };
        for gap in waiver_gaps(policy, (&records, &carried), &changed, subject) {
            if !gaps.contains(&gap) {
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
            "quarantine": {"reason": quarantine.reason, "end": quarantine.end,
                "owner": quarantine.owner},
        });
        if !required && !gaps.contains(&gap) {
            gaps.push(gap);
        }
    }
    Ok(gaps)
}

/// The valid policies that judged the candidate of `attempt`, each once ([`coverage_gaps`]):
/// the base's, as it judged the candidate, and the candidate's own when it differs.
fn attempt_policies(git: &Git, attempt: &Attempt) -> Result<Vec<policy::Policy>, String> {
    let mut judged: Vec<policy::Policy> = Vec::new();
    for at in [&attempt.base, &attempt.commit] {
        // G635: the base's policy as it judged the candidate, so a waived check of a repair is
        // named with its waiver.
        let loaded = if at == &attempt.base {
            policy::load_base_of(git, at, &attempt.commit)
        } else {
            policy::load_at(git, at)
        };
        match loaded {
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
    Ok(judged)
}

/// N-3: the required checks of `policy` with `baseline = "base"` whose passing counted
/// `records` on the landed tree name units that failed on the candidate and on its base: each
/// with those units (`baseline_failures`) and what each record was compared with (`baseline`).
fn baseline_gaps(
    policy: &policy::Policy,
    records: &[evidence::EvidenceRecord],
    (commit, base_tree): (&str, &str),
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
        let key = policy.key(&id, (commit, Some(base_tree)));
        let mut failures: Vec<String> = Vec::new();
        let mut compared: Vec<serde_json::Value> = Vec::new();
        for record in listed.iter().filter(|record| {
            record.kind == evidence::KIND_CHECK
                && record.check == id
                && record.manifest_digest == policy.manifest_digest
                && record.verdict == evidence::Verdict::Pass
                && key.holds(&record.subject)
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
/// counted `records` do not pass for `commit`, judged against a base whose tree is
/// `base_tree`, and that a valid waiver answers
/// ([`policy::Policy::waiver`]): each with its real verdict and the waiver, `waived_by` (the
/// approver), `why` and `record` (the waiver's id). A waiver is never a pass. G638: a waiver
/// among `carried`, of an earlier candidate, also names that candidate (`carried_from`).
fn waiver_gaps(
    policy: &policy::Policy,
    (records, carried): (&[evidence::EvidenceRecord], &[evidence::EvidenceRecord]),
    changed: &[String],
    (commit, base_tree): (&str, &str),
) -> Vec<serde_json::Value> {
    let listed = policy.answering(records.to_vec());
    let (required, _) = policy.required(changed);
    required
        .iter()
        .filter_map(|entry| {
            let id = evidence::CheckId::new(&entry.id, &entry.environment);
            let key = policy.key(&id, (commit, Some(base_tree)));
            let verdict = match evidence::combine(&listed, &id, &policy.manifest_digest, key) {
                Combined::Pass => return None,
                Combined::ProductFailure => "product_failure",
                Combined::Flaky => "flaky",
                Combined::Unknown => "unknown",
            };
            let waiver = policy.waiver(&listed, carried, &id, commit)?;
            let mut gap = json!({
                "check": entry.id,
                "environment": entry.environment,
                "verdict": verdict,
                "waived_by": waiver.producer.name,
                "why": waiver.why,
                "record": waiver.id,
            });
            if waiver.subject.commit != commit {
                gap["carried_from"] = json!(waiver.subject.commit);
            }
            Some(gap)
        })
        .collect()
}

/// N-9: the waivable checks the policy at `base` requires of candidate `commit` (tree `tree`)
/// that its counted records do not pass and no valid waiver answers yet, judged as its land
/// judges them: each `{"check", "environment", "verdict"}`, the verdict its records combine
/// to. What `bay waiting` lists and `bay approve --waive` answers. None without a valid policy.
/// `earlier` are the candidates the change's operations built, whose waivers may carry (G638).
///
/// # Errors
///
/// A git fault, an unreadable note, or a class file that cannot be written.
pub fn waivable_gaps(
    git: &Git,
    (base, commit, tree): (&str, &str, &str),
    registry: Option<&Registry>,
    earlier: &[throughput::Earlier],
) -> Result<Vec<serde_json::Value>, String> {
    let policy = match policy::load_base_of(git, base, commit) {
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
    // G638: a check a waiver of an earlier candidate already answers waits for no new one.
    let base_tree = tree_of(git, base)?;
    let carried = throughput::carried_waivers(
        git,
        &policy,
        registry,
        (commit, tree, &base_tree),
        &records,
        earlier,
    )?;
    Ok(policy
        .gaps(&records, &carried, &required, (commit, &base_tree))
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
/// `flaky` for `commit` judged against a base whose tree is `base_tree`, and whose entry says
/// `allow_flaky`, each with every run that answered it.
fn flaky_gaps(
    policy: &policy::Policy,
    records: &[evidence::EvidenceRecord],
    (commit, base_tree): (&str, &str),
) -> Vec<serde_json::Value> {
    let listed = policy.answering(records.to_vec());
    let mut gaps = Vec::new();
    for entry in &policy.manifest {
        let id = evidence::CheckId::new(&entry.id, &entry.environment);
        let key = policy.key(&id, (commit, Some(base_tree)));
        if !policy.allows_flaky(&id)
            || evidence::combine(&listed, &id, &policy.manifest_digest, key) != Combined::Flaky
        {
            continue;
        }
        let runs: Vec<serde_json::Value> = listed
            .iter()
            .filter(|record| {
                record.kind == evidence::KIND_CHECK
                    && record.check == id
                    && record.manifest_digest == policy.manifest_digest
                    && key.holds(&record.subject)
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
    let records = evidence::records_for(git, &tree_of(git, commit)?)?;
    answers_of(git, &policy, commit, &changed, records, registry, recorded)
}

/// [`post_answers`] of each landed `(commit, tree)`, in their order, read in a few batches for
/// them all, never six git processes a landing (a fleet finding, 2026-10-29: a red window of
/// 99 landings was 594 of them, most of one `bay status --all`): one `cat-file --batch-check`
/// names their bases, [`policy::load_many`] loads the bases' policies, one `diff-tree --stdin`
/// reads their diffs and [`evidence::records_for_many`] their notes.
///
/// # Errors
///
/// A git fault, or an unreadable note.
pub fn post_answers_many(
    git: &Git,
    landings: &[(&str, &str)],
    registry: Option<&crate::registry::Registry>,
    recorded: bool,
) -> Result<Vec<Vec<serde_json::Value>>, String> {
    let names: Vec<String> = landings
        .iter()
        .map(|(commit, _)| format!("{commit}^1"))
        .collect();
    // A landing with no parent has no base, so no policy judged it.
    let parents = git.object_ids(&names).map_err(|error| error.message())?;
    let based: Vec<(usize, String)> = parents
        .into_iter()
        .enumerate()
        .filter_map(|(at, base)| Some((at, base?)))
        .collect();
    let base_ids: Vec<String> = based.iter().map(|(_, base)| base.clone()).collect();
    let mut judged = Vec::new();
    for ((at, base), loaded) in based.iter().zip(policy::load_many(git, &base_ids)) {
        match loaded {
            Ok(Some(policy)) if !policy.post.is_empty() => judged.push((*at, base, policy)),
            Ok(_) | Err(policy::LoadError::Invalid(_)) => {}
            Err(policy::LoadError::Git(why)) => return Err(why),
        }
    }
    let pairs: Vec<(&str, &str)> = judged
        .iter()
        .map(|(at, base, _)| (landings[*at].0, base.as_str()))
        .collect();
    let trees: Vec<&str> = judged.iter().map(|(at, _, _)| landings[*at].1).collect();
    let changed = check::changed_paths_many(git, &pairs)?;
    let records = evidence::records_for_many(git, &trees)?;
    let mut out: Vec<Vec<serde_json::Value>> = landings.iter().map(|_| Vec::new()).collect();
    for (((at, _, policy), changed), records) in judged.iter().zip(changed).zip(records) {
        out[*at] = answers_of(
            git,
            policy,
            landings[*at].0,
            &changed,
            records,
            registry,
            recorded,
        )?;
    }
    Ok(out)
}

/// BACKLOG 348: the post answers of landed `commit` judged by the policy at `commit` itself,
/// from the records on its `tree`: what a run of no change on it answered, as the run on the
/// base of a red's range is (`crate::bisect`). The landing that added a post check has no base
/// policy that lists it, so [`post_answers`] never reads that run.
///
/// # Errors
///
/// A git fault, or an unreadable note.
pub fn own_answers(
    git: &Git,
    commit: &str,
    tree: &str,
    registry: Option<&crate::registry::Registry>,
) -> Result<Vec<serde_json::Value>, String> {
    let policy = match policy::load_at(git, commit) {
        Ok(Some(policy)) if !policy.post.is_empty() => policy,
        Ok(_) | Err(policy::LoadError::Invalid(_)) => return Ok(Vec::new()),
        Err(policy::LoadError::Git(why)) => return Err(why),
    };
    let records = evidence::records_for(git, tree)?;
    answers_of(git, &policy, commit, &[], records, registry, true)
}

/// BACKLOG 237: the answer row field of a post check its landing's own tree holds no record of,
/// which a newer landing's run on trunk answered for it: that landing's commit.
pub const ANSWERED_ON: &str = "answered_on";

/// [`post_checks`] of landed `commit`, read on main as of `trunk` (BACKLOG 237, a fleet finding,
/// 2026-11-14: four landings showed their post checks `pending` for hours, each answered by a run
/// on a newer tip). A runner runs a post check on main's newest tip only, and that run answers
/// every landing since the check's previous run (G439), so the first record at or after a landing
/// is its answer. A row still `pending` reads forward along main's first-parent line, the oldest
/// landing first, to the first landing with a record of the check: when that answer is a pass,
/// the row passes and names the landing in [`ANSWERED_ON`]. Any other answer leaves it `pending`:
/// a red names its range, never one landing in it, and only a run narrowed to this one (G495,
/// `[runner] bisect_post`) can say it failed. At most [`crate::red::DEFAULT_REACH`] newer
/// landings are read, a few at first, as the first record is near.
///
/// # Errors
///
/// A git fault, or an unreadable note.
pub fn post_checks_on_trunk(
    git: &Git,
    commit: &str,
    trunk: &str,
    registry: Option<&crate::registry::Registry>,
) -> Result<Vec<serde_json::Value>, String> {
    let mut rows = post_checks(git, commit, registry)?;
    let unanswered =
        |row: &serde_json::Value| row["state"] == "pending" && row[RERUN_DUE].is_null();
    let mut open: Vec<usize> = (0..rows.len())
        .filter(|at| unanswered(&rows[*at]))
        .collect();
    if open.is_empty() {
        return Ok(rows);
    }
    let range = format!("{commit}..{trunk}");
    let line = git
        .run(&[
            "log",
            "--first-parent",
            "--reverse",
            "--format=%H %T %P",
            &range,
        ])
        .map_err(|error| error.message())?;
    let mut newer: Vec<(&str, &str)> = Vec::new();
    let mut parent = commit;
    for landing in line.lines().take(crate::red::DEFAULT_REACH) {
        let mut fields = landing.split(' ');
        let (Some(id), Some(tree)) = (fields.next(), fields.next()) else {
            break;
        };
        // Only a landing on top of this one answers it: one off main's line says nothing of it.
        if fields.next() != Some(parent) {
            break;
        }
        newer.push((id, tree));
        parent = id;
    }
    let (mut from, mut take) = (0, 8);
    while !open.is_empty() && from < newer.len() {
        let read = &newer[from..newer.len().min(from + take)];
        for ((landing, _), answers) in read
            .iter()
            .zip(post_answers_many(git, read, registry, true)?)
        {
            open.retain(|at| {
                let row = &mut rows[*at];
                let Some(theirs) = answers.iter().find(|answer| {
                    answer["check"] == row["check"] && answer["environment"] == row["environment"]
                }) else {
                    // The policy there lists it no more, so no run after it answers it.
                    return false;
                };
                if unanswered(theirs) {
                    return true;
                }
                if theirs["state"] == "pass" {
                    row["state"] = json!("pass");
                    row[ANSWERED_ON] = json!(landing);
                }
                false
            });
        }
        from += read.len();
        take *= 2;
    }
    Ok(rows)
}

/// The post answers of landed `commit` ([`post_answers`]), from its base's `policy`, the paths
/// its diff `changed` and the evidence `records` on its tree.
fn answers_of(
    git: &Git,
    policy: &policy::Policy,
    commit: &str,
    changed: &[String],
    records: Vec<evidence::EvidenceRecord>,
    registry: Option<&crate::registry::Registry>,
    recorded: bool,
) -> Result<Vec<serde_json::Value>, String> {
    let now = crate::evidence::reader_now();
    // G163: only records from the producers a check lists answer it.
    let records = policy.answering(evidence::counted(git, records, registry)?);
    // G677: a run judged against its base passes when every failure was already there, which
    // lets a change merge on a red main; on main, those failures still stand, so as an answer to
    // the post check it is a failure, and main stays red until a run fails nothing.
    let records: Vec<evidence::EvidenceRecord> = records
        .into_iter()
        .map(|mut record| {
            if !record.baseline_failures().is_empty() {
                record.verdict = evidence::Verdict::ProductFailure;
            }
            record
        })
        .collect();
    let mut out = Vec::new();
    for entry in &policy.post {
        let id = evidence::CheckId::new(&entry.id, &entry.environment);
        let mut row = json!({"check": entry.id, "environment": entry.environment});
        let skipped = policy
            .check(&id)
            .and_then(|check| policy.unwoken(check, changed));
        if recorded {
            // NEW-6: whether this landing requires the check, which no answer may yet say.
            row["required"] = json!(skipped.is_none());
        } else if let Some(skipped) = skipped {
            row["state"] = json!("not_required");
            row["reason"] = json!(skipped.reason);
            out.push(row);
            continue;
        }
        // A post check runs on main's own commit (`policy::CheckEntry::key`).
        let key = policy.key(&id, (commit, None));
        let (state, due) = evidence::answer(&records, &id, &policy.manifest_digest, key, now);
        row["state"] = json!(state);
        if let Some(due) = due {
            row[RERUN_DUE] = json!(crate::clock::rfc3339_of(due));
        }
        // BACKLOG 340: the units a failure failed, for the red judgement alone.
        if recorded
            && matches!(state, "flaky" | "product_failure")
            && let Some(units) = evidence::failed_units(&records, &id, &policy.manifest_digest, key)
        {
            row[FAILED_UNITS] = json!(units);
        }
        out.push(row);
    }
    Ok(out)
}

/// G628: the answer row field of a post check that is `pending` because its failure waits for a
/// rerun ([`evidence::answer`]): when the first of those reruns is due, in RFC 3339. Past it, the
/// failure is the check's answer.
pub const RERUN_DUE: &str = "rerun_due";

/// BACKLOG 340: the answer row field, on a row the red judgement reads, of a post check that
/// answers `flaky` or `product_failure`: the units its failures failed
/// ([`evidence::failed_units`]). Absent when a failure names no unit.
pub const FAILED_UNITS: &str = "failed_units";

/// The compact terminal record that outlives the drafts (§ 9a). Written once per operation.
fn write_terminal(
    ctx: &Ctx,
    op_id: &str,
    outcome: &str,
    published: Option<&str>,
) -> Result<(), String> {
    let Some(record) = terminal_record(ctx, op_id, outcome, published)? else {
        return Ok(());
    };
    let signer = ctx.executor_signer().ok().flatten();
    crate::log::append(ctx.repo, &ctx.executor_name(), signer.as_ref(), &record).map(|_| ())
}

/// [`write_terminal`]'s record, or `None` when a log holds it already.
fn terminal_record(
    ctx: &Ctx,
    op_id: &str,
    outcome: &str,
    published: Option<&str>,
) -> Result<Option<serde_json::Value>, String> {
    let executor = ctx.executor_name();
    // BACKLOG 99: after this process's own push landed the operation, no runner can hold its
    // terminal record (that push would have been refused), so no log is fetched to look for one.
    if ctx.pushed_landing.is_none() {
        // G887: as for an event ([`crate::log::holds_event`]), the logs on this box first.
        let held = || -> Result<bool, String> {
            Ok(crate::log::records_for(&ctx.repo.git, &ctx.id)?
                .iter()
                .any(|record| record["op_id"] == op_id && record["outcome"] == outcome))
        };
        if held()? {
            return Ok(None);
        }
        crate::log::fetch(ctx.repo);
        if held()? {
            return Ok(None);
        }
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
    // G786: when the change was started, so its draft's time outlives the change's record.
    if let Some(started_at) = ctx.record.started_at {
        record["started_at"] = json!(started_at);
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
    Ok(Some(record))
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

use crate::throughput::tree_of;

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
/// G599: the latest land request is the newest one its record names, which the chain links.
/// A runner reads no requests ref, so the cut of a change merged from another clone and then
/// abandoned left it out, the host refused the cut, and every pass tried it again.
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
    if let Some(request) = ctx
        .record
        .operations
        .iter()
        .rev()
        .find_map(|op| op.request_id.clone().filter(|request| !request.is_empty()))
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
    let step = Step::new("retention");
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

    /// BACKLOG 81: a refusal record keeps what the host said, never the command line.
    #[test]
    fn a_refusal_names_what_the_host_said() {
        // git prints the hook's lines first, so the first of them shares the command's line.
        let why = "batch b1 was refused: git push -q --atomic origin x:refs/heads/main \
            y:refs/bay/landed/a failed: remote: error: GH013: Repository rule violations found \
            for refs/heads/main.\nremote: \nremote: error: GH013: Repository rule violations \
            found for refs/heads/main.\nTo https://example/repo.git\n ! [remote rejected] x -> main \
            (push declined due to repository rule violations)\nerror: failed to push some refs\n";
        assert_eq!(
            super::host_said(why),
            "remote: error: GH013: Repository rule violations found for refs/heads/main.; ! \
             [remote rejected] x -> main (push declined due to repository rule violations); error: \
             failed to push some refs"
        );
        assert_eq!(
            super::host_said("git push failed: timed out\n"),
            "git push failed: timed out"
        );
    }

    /// BACKLOG 216: a host's refusal for permission is told from a refusal a retry clears.
    #[test]
    fn a_refusal_for_permission_is_told_from_one_a_retry_clears() {
        let push = "git push -q --atomic origin x:refs/heads/main failed: ";
        for refused in [
            "remote: error: GH013: Repository rule violations found for refs/heads/main.\n",
            "remote: error: Cannot update this protected ref.\n ! [remote rejected] x -> main (protected branch hook declined)\n",
            "remote: Permission to org/repo.git denied to someone.\nfatal: unable to access 'https://example/repo.git/': The requested URL returned error: 403\n",
        ] {
            assert!(
                super::refused_for_permission(&format!("{push}{refused}")),
                "{refused}"
            );
        }
        for transient in [
            "remote: error: GH009: something went wrong.\n",
            " ! [remote rejected] x -> main (failed)\n",
            "remote: bay: the publication queue is held\n",
            "timed out\n",
        ] {
            assert!(
                !super::refused_for_permission(&format!("{push}{transient}")),
                "{transient}"
            );
        }
    }

    /// BACKLOG 90: only an added `git merge-tree` marker (labelled with a commit) marks a path;
    /// a removed one, one with another label, and a content line that looks like a header do not.
    #[test]
    fn only_an_added_merge_tree_marker_marks_its_path() {
        let main = "a".repeat(40);
        let draft = "b".repeat(64);
        let diff = format!(
            "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n\
             @@ -1 +1,5 @@\n+<<<<<<< {main}\n+x\n+=======\n+y\n+>>>>>>> {draft}\n\
             diff --git a/doc.md b/doc.md\n--- a/doc.md\n+++ b/doc.md\n@@ -1,2 +1,2 @@\n\
             -<<<<<<< {main}\n+<<<<<<< HEAD\n+++ b/src/a.rs\n\
             diff --git a/gone.rs b/gone.rs\n--- a/gone.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n\
             -<<<<<<< {main}\n\
             diff --git a/src/b.rs b/src/b.rs\n--- a/src/b.rs\n+++ b/src/b.rs\n@@ -0,0 +1 @@\n\
             +>>>>>>> {main}\n"
        );
        assert_eq!(super::marked_paths(&diff), vec!["src/a.rs", "src/b.rs"]);
        assert_eq!(super::marked_paths(""), Vec::<String>::new());
    }

    /// A test pause ends: at its release, at its time limit, or once its parent is gone.
    /// A runner's line names where an operation's time went: each step's seconds, in order.
    #[test]
    fn a_step_reports_the_seconds_spent_in_each() {
        let step = super::Step::new("read");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        step.set("check");
        step.set("check");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        step.set("publish");
        let seconds = step.seconds();
        assert_eq!(step.get(), "publish");
        assert_eq!(
            seconds.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            ["read", "check"],
            "steps under a second are left out: {seconds:?}"
        );
        assert!(seconds.iter().all(|(_, secs)| *secs == 1), "{seconds:?}");
    }

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
