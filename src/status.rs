//! `bay status`: current state, derived on read (`DESIGN` § 4, `INTERFACES` § 1).
//!
//! "Landed" comes from trunk, never from the chain alone: a change whose publication is on
//! trunk is landed even when its chain was not updated before a crash. Built here: one change
//! or every local change, with freshness, and each change's named inputs judged at trunk
//! (`DESIGN` § 8): `invalidated` while trunk requires a version it has not observed, `repairing`
//! while no candidate is validated under an observed one. Leftovers (G41) are a later item.
//! `bay status` also fetches the remote's `refs/bay/live/*` and records that fetch, from which
//! `--all` reads the fleet map ([`crate::fleet`], G23), failing closed without it (G49).
//! Completion is reported per dimension, `running` and `effective` from counted runtime
//! records ([`crate::runtime`]), with the selected profile's verdict ([`crate::completion`]).
//! After a fetch it accepts and acks this actor's talks ([`crate::talk::receive`], § 11c), and
//! says so in `detail.talk_accept`. Trunk commits that did not land through bay (merges and
//! pushes outside it) are named and counted under `detail.unattributed`
//! ([`crate::unattributed`], G85). With operations queued and no fresh executor presence on
//! record (`crate::presence`, G67), `detail.warnings` names `no_live_executor`. N-20: each merge
//! outside bay in the red window that pushed bay landings off trunk's first-parent line is named
//! there as `trunk_not_linear` ([`crate::trunkline`]). G309: `detail.flow` names the flow of
//! work, live changes by actor beside the WIP limit, landings, cycle time, the oldest live change
//! and the changes waiting for an approval, derived from the chains on read ([`crate::flow`]).

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::change::{CHANGES_PREFIX, Chain, ChangeRecord, change_ref};
use crate::clock::now_rfc3339;
use crate::completion::{self, Dimension, Need};
use crate::evidence::{self, EvidenceRecord};
use crate::inputs::Probe;
use crate::publish::{scan, scan_many};
use crate::receipts::{self, Receipt};
use crate::repo::Repo;
use crate::result::{BayResult, Freshness, Reason};
use crate::runtime::Confirmed;
use crate::sweep::{self, Swept};
use crate::sweeper::Answer;
use crate::view;

/// G619: how long, in seconds, a status read waits for the remote by default before it answers
/// from the store ([`fetch_swept`]). The local read after it is not bounded by it: on a store of
/// 3,490 chains, at load 63-80 on 14 cores, a read served from a sweep took 4.5-7.9 s in all
/// before G624 cut its git work (2026-11-01). After it, the same read used 3.2-3.4 s of CPU
/// against 3.75-3.9 s; its wall time, at load 59-75, stayed in the noise (3.5-8.1 s).
pub const STATUS_DEADLINE_S: u64 = 3;

/// The deadline of a read that names none: [`STATUS_DEADLINE_S`], or in a test build
/// `BAY_TEST_STATUS_DEADLINE_S`.
#[must_use]
pub fn default_deadline() -> Duration {
    Duration::from_secs(
        std::env::var("BAY_TEST_STATUS_DEADLINE_S")
            .ok()
            .filter(|_| TEST_HOOKS)
            .and_then(|text| text.parse().ok())
            .unwrap_or(STATUS_DEADLINE_S),
    )
}

/// `bay status [change]`, waiting for the remote at most `deadline` from now (G619).
#[must_use]
pub fn status(dir: &Path, change: Option<&str>, deadline: Duration) -> BayResult {
    run(dir, change, deadline).unwrap_or_else(BayResult::failed)
}

/// What a cached read says instead of a fetch time: it never synchronized.
pub const CACHED: &str = "local cache (not fetched by this read)";

/// The state of the local changes `select` picks, read from this repository's own refs only
/// (`DESIGN` § 11: "Local caches and cursors are allowed; status shows its last
/// synchronization point"). Used by the agent hook, where latency is a measured interface: it
/// pushes no receipt, fetches nothing, and writes no view journal (leftovers are not read).
/// The trunk it judges against is the remote-tracking ref, the last synchronization point,
/// and `freshness.fetched_at` says it was not fetched. `select` sees each change's id and
/// record before any trunk scan, so changes it drops cost one chain read.
///
/// # Errors
///
/// No repository, no tracking ref of the remote trunk, or a git fault.
pub fn cached(
    repo: &Repo,
    select: &dyn Fn(&str, &ChangeRecord) -> bool,
) -> Result<BayResult, String> {
    let trunk = repo
        .git
        .run(&["rev-parse", "--verify", &repo.trunk_tracking_ref()])
        .map_err(|error| error.message())?;
    let pending = receipts::pending(repo);
    // W5: every chain in one batch (G200b), never one read per change.
    let chains = Chain::new(&repo.git)
        .read_all()
        .map_err(|error| error.message())?;
    let mut changes = Vec::new();
    for held in chains {
        let (id, record) = (held.id.as_str(), held.record);
        if !select(id, &record) {
            continue;
        }
        let unsynced: Vec<&Receipt> = pending
            .iter()
            .map(|(_, receipt)| receipt)
            .filter(|receipt| receipt.change == id)
            .collect();
        changes.push(describe(repo, &record, &trunk, &unsynced, &Read::Cached)?);
    }
    let mut result = BayResult::ok(json!({"changes": changes, "trunk": trunk}));
    result.freshness = Some(Freshness {
        trunk,
        fetched_at: CACHED.to_owned(),
    });
    Ok(result)
}

/// Fetches the remote trunk; returns trunk's tip and the time of the trunk fetch, taken before
/// any other fetch so it never overstates trunk's freshness. Offline, the tracking ref is the
/// trunk and there is no fetch time.
fn synchronize(repo: &Repo) -> Result<(String, Option<String>), String> {
    match repo.fetch_trunk() {
        Ok(trunk) => Ok((trunk, Some(now_rfc3339()))),
        Err(_) => Ok((
            repo.git
                .run(&["rev-parse", "--verify", &repo.trunk_tracking_ref()])
                .map_err(|error| error.message())?,
            None,
        )),
    }
}

/// Reads each change of `ids` fresh from the remote ([`Repo::remote_chain`]): a fast-forward
/// brings what an executor recorded elsewhere, a retention cut replaces the local chain, and
/// local work the remote lacks is kept (G289).
fn refresh_chains(repo: &Repo, chain: &Chain, ids: &[String]) -> Result<(), String> {
    for id in ids {
        // Local work the remote lacks was pushed by the sync above, or is kept here.
        let _ = repo.remote_chain(id);
        // A change named here but not local is read from the remote.
        if chain.head(id).map_err(|error| error.message())?.is_none() {
            repo.fetch_refs(&[change_ref(id)]).map_err(|error| {
                format!(
                    "change {id} is not here or on the remote: {}",
                    error.message()
                )
            })?;
        }
    }
    Ok(())
}

/// The changes a status read names, their chains refreshed from the remote: the one named, or
/// every live one this repository holds (the 0.5.3 status patch, owner 2026-10-25). An ended
/// change (merged, abandoned, superseded, no effect) never comes back, so its record here is
/// final, and `bay status <change>` still reads it whole. A listing refreshes in one fetch.
fn listed(repo: &Repo, chain: &Chain, named: Option<String>) -> Result<Vec<String>, String> {
    if let Some(id) = named {
        let ids = vec![id];
        refresh_chains(repo, chain, &ids)?;
        return Ok(ids);
    }
    let ids = live_ids(repo)?;
    let _ = repo.remote_chains(&ids);
    Ok(ids)
}

/// Every change this repository holds that has not ended.
///
/// G708 (a fleet finding, 2026-11-13: a status listing named five changes whose own reads said
/// they were on main): ended is [`crate::overlap::counts_live`]'s rule, which also reads a
/// published operation, a landed marker and a refusal as already merged, not the chain's state
/// alone.
fn live_ids(repo: &Repo) -> Result<Vec<String>, String> {
    // W5: every chain's record in one batch (G200b); a chain it cannot read stays listed.
    let (chains, landed) = crate::overlap::read_chains(repo)?;
    let ended_here: std::collections::BTreeSet<String> = chains
        .into_iter()
        .filter(|held| !crate::overlap::counts_live(held, &landed))
        .map(|held| held.id)
        .collect();
    Ok(repo
        .git
        .run(&["for-each-ref", "--format=%(refname)", CHANGES_PREFIX])
        .map_err(|error| error.message())?
        .lines()
        .map(|name| name.trim_start_matches(CHANGES_PREFIX).to_owned())
        .filter(|id| !ended_here.contains(id))
        .collect())
}

/// G615: [`listed`] from what a sweep found: each chain of the changes the read names moves by
/// [`Repo::remote_chain`]'s rule, and only a chain that moved is touched. A fast-forward, and a
/// chain this store lacks, move by compare-and-swap in one transaction; a chain the remote's is
/// not a fast-forward of (local work ahead, a chain apart, a retention cut) is settled as one git
/// refused; one the remote lacks is kept.
fn settle_listed(
    repo: &Repo,
    chain: &Chain,
    named: Option<String>,
    swept: &Swept,
) -> Result<Vec<String>, String> {
    let is_named = named.is_some();
    // G619: every sweep settles every live chain, so each one serves the reads in its window.
    let mut settled = live_ids(repo)?;
    let ids = match named {
        Some(id) => {
            if !settled.contains(&id) {
                settled.push(id.clone());
            }
            vec![id]
        }
        None => settled.clone(),
    };
    let mut updates = Vec::new();
    for id in &settled {
        let name = change_ref(id);
        let Some(found) = swept.moves.get(&name) else {
            continue;
        };
        match (found.flag, found.new.as_deref()) {
            (' ' | '*', Some(_)) => updates.push((name, found.old.clone(), found.new.clone())),
            ('+', Some(new)) => {
                let _ = repo.settle_chain(id, new);
            }
            _ => {}
        }
    }
    sweep::apply(&repo.git, &updates);
    // A change named here but not local is read from the remote, which must hold it.
    if let [id] = ids.as_slice()
        && is_named
        && !swept.moves.contains_key(&change_ref(id))
        && chain.head(id).map_err(|error| error.message())?.is_none()
    {
        return Err(format!(
            "change {id} is not here or on the remote: the remote holds no {}",
            change_ref(id)
        ));
    }
    Ok(ids)
}

/// How many changes, and how many views, a listing describes; the rest are named in
/// `more_changes` and `more_leftovers`.
const LISTED: usize = 20;

/// The changes or views a listing names but does not describe: how many, the first LISTED ids,
/// and how to read one. `null` when none is left out.
fn more_changes(more: &[String]) -> Value {
    if more.is_empty() {
        return Value::Null;
    }
    json!({
        "count": more.len(),
        "ids": more.iter().take(LISTED).collect::<Vec<_>>(),
        "read": "bay status <change>",
    })
}

/// Whether a change in `state` has ended for good.
pub(crate) const fn ended(state: crate::change::ChangeState) -> bool {
    use crate::change::ChangeState;
    matches!(
        state,
        ChangeState::Landed
            | ChangeState::Abandoned
            | ChangeState::Superseded
            | ChangeState::NoEffect
    )
}

/// The changes shown and every other section of a status of `ids` at `trunk`.
fn read_sections(
    repo: &Repo,
    trunk: &str,
    ids: &[String],
    named: bool,
    view: Option<&str>,
    targets_fetched: bool,
    read: &Read,
) -> (Result<Shown, String>, Sections) {
    let me = crate::change::current_actor(&repo.common_dir)
        .ok()
        .map(|actor| actor.name);
    let me = me.as_deref();
    // G616 (a fleet finding, 2026-10-31: after its one round trip a status spent about 6 s
    // reading this store, each section after the one before): every section only reads this
    // store, so each runs beside the others, and the read costs about its longest section.
    std::thread::scope(|scope| {
        let leftovers = scope.spawn(move || leftovers(repo, trunk, view, me));
        let quarantines = scope.spawn(move || quarantines(repo, trunk));
        let flakes = scope.spawn(move || flake_rows(repo, trunk, read));
        let red_now = scope.spawn(move || red_rows(repo, trunk));
        let targets = scope.spawn(move || target_rows(repo, trunk, targets_fetched, read));
        // G309: the flow of work, derived on read from the chains this repository holds.
        let flow = scope.spawn(move || {
            crate::flow::flow(repo, trunk).unwrap_or_else(|why| json!({"unread": why}))
        });
        let talks = scope.spawn(move || talk_rows(repo));
        let unattributed = scope.spawn(move || unattributed_rows(repo, trunk));
        // N-8: a held trunk is named, with where the hold comes from, its reason and its end.
        let hold = scope.spawn(move || crate::hold::held(repo, trunk));
        // N-20: each outside merge in the red window that pushed bay landings off the line.
        let not_linear = scope.spawn(move || not_linear(&repo.git, trunk));
        // R11: delivery that fails is never silent.
        let wake_failing = scope.spawn(move || crate::log::wake_failing(&repo.git));
        let shown = shown_changes(repo, ids, named, me, trunk, read);
        let runner = shown.as_ref().map_or_else(
            |_| Vec::new(),
            |shown| crate::presence::runner_warnings(repo, shown.queued, trunk),
        );
        let sections = Sections {
            runner,
            leftovers: joined(leftovers),
            quarantines: joined(quarantines),
            flakes: joined(flakes),
            red: joined(red_now),
            targets: joined(targets),
            flow: joined(flow),
            talks: joined(talks),
            unattributed: joined(unattributed),
            hold: joined(hold),
            not_linear: joined(not_linear),
            wake_failing: joined(wake_failing),
        };
        (shown, sections)
    })
}

/// The changes a listing describes and the ids it only names, and how many operations wait.
struct Shown {
    changes: Vec<Value>,
    more: Vec<String>,
    queued: usize,
}

/// What [`run`] reads beside the changes it describes (G616), each section on its own thread.
struct Sections {
    runner: Vec<Value>,
    leftovers: Result<(Vec<Value>, Vec<String>), String>,
    quarantines: Value,
    flakes: Value,
    red: (Value, Value),
    targets: Value,
    flow: Value,
    talks: Value,
    unattributed: Value,
    hold: Option<crate::hold::Hold>,
    not_linear: Vec<Value>,
    wake_failing: Option<Value>,
}

/// What a section's thread returned; a panic in it is this thread's.
fn joined<T>(section: std::thread::ScopedJoinHandle<'_, T>) -> T {
    section
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// The changes of `ids` a read describes. An agent reads this answer whole, so it has a budget
/// (a fleet finding, 2026-10-26: a listing of 146 live changes ran to megabytes and minutes). At
/// most LISTED changes are described, the reader's (`me`) own first; the rest are named, and
/// `bay status <change>` reads one. A listing describes the reader's own changes; everyone
/// else's are counted and named, so an agent's answer stays a few hundred tokens however busy
/// the repository is.
fn shown_changes(
    repo: &Repo,
    ids: &[String],
    named: bool,
    me: Option<&str>,
    trunk: &str,
    read: &Read,
) -> Result<Shown, String> {
    let pending = receipts::pending(repo);
    let mut records = held_records(&Chain::new(&repo.git), ids, named)?;
    let queued: usize = records.iter().map(crate::presence::waiting).sum();
    records.sort_by_key(|record| me != Some(record.actor.name.as_str()));
    let mine_only = ids.len() > 1 && me.is_some();
    let (mut changes, mut more) = (Vec::new(), Vec::new());
    for record in &records {
        let others = mine_only && me != Some(record.actor.name.as_str());
        if changes.len() >= LISTED || others {
            more.push(record.id.clone());
            continue;
        }
        let unsynced: Vec<&Receipt> = pending
            .iter()
            .map(|(_, receipt)| receipt)
            .filter(|receipt| receipt.change == record.id)
            .collect();
        changes.push(describe(repo, record, trunk, &unsynced, read)?);
    }
    Ok(Shown {
        changes,
        more,
        queued,
    })
}

/// The records of `ids`, read in one batch of every chain (G200b), never one walk per change (a
/// fleet finding, 2026-10-26: 2,408 state reads, a third of a five-minute status). One named
/// change is read alone. A listing (`named` false) leaves out a copy of another change's chain,
/// which is no change (G563); a read of the copy alone names it.
fn held_records(chain: &Chain, ids: &[String], named: bool) -> Result<Vec<ChangeRecord>, String> {
    let mut held: std::collections::BTreeMap<String, ChangeRecord> = if ids.len() == 1 {
        std::collections::BTreeMap::new()
    } else {
        chain
            .read_all()
            .map_err(|error| error.message())?
            .into_iter()
            .map(|held| (held.id, held.record))
            .collect()
    };
    ids.iter()
        .filter_map(|id| match held.remove(id) {
            Some(record) => Some(Ok(record)),
            None => match chain.read(id) {
                Ok((_, record)) => Some(Ok(record)),
                Err(crate::change::ChainError::Copy { .. }) if !named => None,
                Err(error) => Some(Err(error.message())),
            },
        })
        .collect()
}

/// What a status read brought from the remote before it reads anything.
struct Refreshed {
    trunk: String,
    fetched_at: Option<String>,
    /// The Unix time of `fetched_at`, when known.
    fetched_unix: Option<u64>,
    /// The changes the read names, their chains refreshed ([`listed`]).
    ids: Vec<String>,
    /// Whether the remote answered the read of the promotion targets.
    targets: bool,
    /// What accepting this actor's talks did (`detail.talk_accept`), online only.
    talk_accept: Option<Value>,
    /// G619: the `stale_read` warning of a read past its deadline.
    stale: Option<Value>,
}

/// Fetches what a status read judges: the trunk first, then at once each set of refs that no
/// other of these fetches writes (a fleet finding, 2026-10-31: nine fetches one after another
/// took 16.5 s of a 42 s status). The receipts' sync and the chains it may push stay in one
/// thread, in that order. Online, this actor's talks are accepted beside them (G612: a talk's
/// three round trips, one after another past the others, took 4 s of a 15 s status). Offline,
/// only the log and the chains are tried, as before.
fn fetch_all(repo: &Repo, named: Option<String>) -> Result<Refreshed, String> {
    let unix = crate::live::unix_now();
    let (trunk, fetched_at) = synchronize(repo)?;
    let online = fetched_at.is_some();
    let (ids, targets, talk_accept) = std::thread::scope(|scope| {
        // § 11c: a fetching command on the recipient's machine accepts and acks its talks.
        let talks = online
            .then(|| scope.spawn(|| crate::talk::poll::command(repo, crate::live::unix_now())));
        let chains = scope.spawn(move || {
            let _ = receipts::sync(repo);
            listed(repo, &Chain::new(&repo.git), named)
        });
        let targets = scope.spawn(move || online && crate::target::fetch(repo));
        scope.spawn(|| crate::log::fetch(repo));
        if online {
            scope.spawn(|| crate::fleet::fetch(repo));
            // NEW-4: the lane refs an unattributed landing is judged against, as the remote
            // holds them.
            scope.spawn(|| crate::lane::fetch(repo));
            // Red, flakes, post checks and targets judge the evidence notes: fetched once here.
            scope.spawn(|| repo.fetch_evidence_once(&crate::signing::Keys::load(&repo.git)));
        }
        (
            chains
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
            targets
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
            talks.and_then(|talks| {
                talks
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            }),
        )
    });
    Ok(Refreshed {
        trunk,
        fetched_unix: fetched_at.is_some().then_some(unix),
        fetched_at,
        ids: ids?,
        targets,
        talk_accept,
        stale: None,
    })
}

/// What a sweep of this store asks the remote, from the config at the trunk the store holds.
struct Plan {
    tracking: String,
    /// The store's trunk; `None` when it holds none.
    held: Option<String>,
    /// Each promotion target's remote ref and its local copy.
    targets: Vec<(String, String)>,
    refspecs: Vec<String>,
}

impl Plan {
    fn of(repo: &Repo) -> Option<Self> {
        let tracking = repo.trunk_tracking_ref();
        let targets: Vec<(String, String)> = repo
            .config
            .targets
            .iter()
            .map(|target| (target.reference(), crate::target::local_ref(&target.name)))
            .collect();
        let held = repo.git.ref_target(&tracking).ok()?;
        let mut refspecs = vec![format!("+refs/heads/{}:{tracking}", repo.trunk())];
        for prefix in [
            CHANGES_PREFIX,
            crate::names::REQUESTS,
            crate::log::LOG_PREFIX,
            evidence::EVIDENCE_PREFIX,
        ] {
            refspecs.push(format!("+{prefix}*:{prefix}*"));
        }
        refspecs.extend(crate::fleet::refspecs());
        // Each target is asked by its name as a prefix, so one the remote no longer holds fails
        // no sweep: it is named as a prune of its copy. A ref that shares the prefix is never
        // taken.
        refspecs.extend(
            targets
                .iter()
                .map(|(reference, local)| format!("+{reference}*:{local}*")),
        );
        Some(Self {
            tracking,
            held,
            targets,
            refspecs,
        })
    }

    /// The time of the sweep whose window is open now ([`sweep::window`]).
    fn window(&self, repo: &Repo) -> Option<String> {
        sweep::window(
            &repo.git,
            &repo.common_dir,
            repo.remote(),
            &self.refspecs,
            crate::live::unix_now(),
        )
    }
}

/// Whether this store holds the chain of the change a read names, or the read names none.
fn holds_named(repo: &Repo, named: Option<&str>) -> bool {
    named.is_none_or(|id| Chain::new(&repo.git).head(id).ok().flatten().is_some())
}

/// G615: [`fetch_all`] in one round trip: the remote is asked once ([`crate::sweep`]) for every
/// namespace a status read judges, with the config at the trunk this store held, and each
/// namespace takes only the moves its own fetch takes:
///
/// - the trunk, forced ([`Repo::fetch_trunk`]); the config is read again at a tip it moved to;
/// - every executor's log, forced, but this executor's own only forward (A10,
///   [`crate::log::fetch`]);
/// - the fleet refs and the locks, forced and pruned ([`crate::fleet::fetch`]);
/// - every other writer's evidence notes, forced and never pruned ([`Repo::fetch_evidence`]);
/// - each target's ref, its copy removed when the remote lacks it ([`crate::target::fetch`]),
///   and fetched as before when the config the trunk moved to names other targets;
/// - the chains of every live change and of the change the read names ([`settle_listed`]),
///   after the receipts' sync, which reads the remote's chains and land requests from the same
///   sweep and writes none.
///
/// This actor's talks are then accepted beside the chains (G612), and the lane refs (NEW-4)
/// fetched as before. `None` when a sweep cannot serve the read, which then fetches as before
/// ([`fetch_all`]): no config at the trunk this store holds, a partial clone (a sweep's mixed
/// fetch carries no filter, `Git::run`), or a sweep that fails (offline, or a git that cannot).
/// Inside the window of a sweep (G617), the read is served by the store as that sweep left it
/// ([`local_read`]).
///
/// G619: the sweep runs as a process of its own ([`crate::sweeper`]), and the read waits for it
/// until `until`. Past it, or while another sweep of the store runs past it, the read is the
/// store as the last sweep left it, and says how old ([`stale_read`]). A read of a change this
/// store lacks waits for the sweep: nothing here can answer it.
fn fetch_swept(
    dir: &Path,
    change: Option<&str>,
    started: Instant,
    until: Instant,
) -> Option<Result<(Repo, Refreshed), Box<BayResult>>> {
    let first = Repo::open_with(dir, crate::repo::Fetch::Never).ok()?;
    if first.git.is_partial() {
        return None;
    }
    let plan = Plan::of(&first)?;
    let named = change.map(str::to_owned).or_else(|| first.view_change(dir));
    if let Some(at) = plan.window(&first)
        && let Some(refreshed) = local_read(
            &first,
            named.clone(),
            None,
            plan.held.clone(),
            Some(at),
            true,
            talk_now,
        )
    {
        return Some(Ok((first, refreshed)));
    }
    // G710 (a measurement, 2026-11-13: on a remote of 59,387 refs a sweep took 18 s, and every
    // read past a sweep's window waited its whole 3 s deadline, then answered from the store
    // anyway): when the store's last sweep took longer than this read may wait, the read waits
    // for none. It answers from the store now, and the sweep it starts runs on.
    let until = if sweep::last_took(&first.common_dir)
        .is_some_and(|took| Duration::from_secs(took) > until.saturating_duration_since(started))
    {
        started
    } else {
        until
    };
    let until = holds_named(&first, named.as_deref()).then_some(until);
    let sweep = match crate::sweeper::start(dir, change, until) {
        Answer::Done {
            at,
            ids,
            targets,
            talk_accept,
        } => return swept_read(dir, named, at, Some(ids), targets, move |_| talk_accept),
        Answer::Moved(at) => return swept_read(dir, named, at, None, true, talk_now),
        Answer::Open => {
            let (at, _) = sweep::last(&first.common_dir)?;
            return swept_read(dir, named, at, None, true, talk_now);
        }
        Answer::Busy(running) => {
            let ended = crate::sweeper::wait_ended(&first.common_dir, &running, until);
            let since = running["since"].as_u64().unwrap_or(u64::MAX);
            if let Some((at, _)) = sweep::last(&first.common_dir).filter(|(_, unix)| *unix >= since)
                && ended
            {
                return swept_read(dir, named, at, None, true, talk_now);
            }
            json!({"state": "running", "by": running})
        }
        Answer::Late(pid) => json!({"state": "started", "by": {"pid": pid}}),
        Answer::Lost => json!({"state": "ended without an answer"}),
        Answer::Unserved => return None,
        Answer::Failed(message) => return Some(Err(Box::new(BayResult::failed(message)))),
    };
    stale_read(first, &plan, named, started, &sweep)
}

/// § 11c: a fetching command accepts and acks this actor's talks, once a poll interval.
fn talk_now(repo: &Repo) -> Option<Value> {
    crate::talk::poll::command(repo, crate::live::unix_now())
}

/// The read after a sweep that ended at `at`: the store opened again, since the sweep may have
/// moved its trunk and so its config; a store that cannot be opened at the trunk the sweep
/// brought answers with why (`CONFIG_MISSING` when the config left trunk). It lists `listed`
/// when the sweep names what it settled.
fn swept_read(
    dir: &Path,
    named: Option<String>,
    at: String,
    listed: Option<Vec<String>>,
    targets: bool,
    talk: impl FnOnce(&Repo) -> Option<Value>,
) -> Option<Result<(Repo, Refreshed), Box<BayResult>>> {
    let repo = match Repo::open_with(dir, crate::repo::Fetch::Never) {
        Ok(repo) => repo,
        Err(error) => return Some(Err(Box::new(error.into_result()))),
    };
    let held = repo.git.ref_target(&repo.trunk_tracking_ref()).ok()?;
    let refreshed = local_read(&repo, named, listed, held, Some(at), targets, talk)?;
    Some(Ok((repo, refreshed)))
}

/// G619: the read when the sweep has not ended by the deadline: the store as the last sweep left
/// it, its time as `fetched_at`, how old it is (`stale_s`), and a `stale_read` warning naming
/// the sweep (`sweep`) and how long the read waited (`waited_ms`). The targets read as not
/// fetched; no talk is accepted (the sweep accepts them).
fn stale_read(
    first: Repo,
    plan: &Plan,
    named: Option<String>,
    started: Instant,
    sweep: &Value,
) -> Option<Result<(Repo, Refreshed), Box<BayResult>>> {
    let (at, unix) = sweep::last(&first.common_dir).unzip();
    let stale_s = unix.map(|unix| crate::live::unix_now().saturating_sub(unix));
    let none = |_: &Repo| None;
    let mut refreshed = local_read(
        &first,
        named,
        None,
        plan.held.clone(),
        at.clone(),
        false,
        none,
    )?;
    refreshed.fetched_unix = unix;
    let mut warning = json!({
        "code": "stale_read",
        "fetched_at": at,
        "stale_s": stale_s,
        "waited_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "sweep": sweep,
        "note": "the remote sweep had not ended by this read's deadline: this is the store as the last sweep left it; the sweep runs on, and a read after it ends shows what it brought",
    });
    // G684: a store whose last sweeps could not ask the remote says so, and since when: a sweep
    // that runs on will most likely not reach it either.
    if let Some(unreached) = sweep::last_unreached(&first.common_dir) {
        warning["note"] = json!(
            "the remote has not answered this store's sweeps: this is the store as the last sweep that reached it left it; a read once the remote answers shows what changed"
        );
        warning["unreached"] = json!({"since": unreached["since"], "why": unreached["why"]});
    }
    refreshed.stale = Some(warning);
    Some(Ok((first, refreshed)))
}

/// What [`fetch_swept`] refreshes from the store alone: as the sweep at `at` left it, the changes
/// `listed` (by default the live ones, or the one the read names), the trunk `held`, the targets
/// read as `targets` says, and the talks `talk` accepts. `None` when the store cannot serve the
/// read: it holds no trunk, or no chain of the change the read names, which then comes from the
/// remote.
fn local_read(
    repo: &Repo,
    named: Option<String>,
    listed: Option<Vec<String>>,
    held: Option<String>,
    at: Option<String>,
    targets: bool,
    talk: impl FnOnce(&Repo) -> Option<Value>,
) -> Option<Refreshed> {
    let trunk = held?;
    let chain = Chain::new(&repo.git);
    let ids = match (listed, named) {
        (Some(listed), _) => listed,
        (None, Some(id)) => {
            chain.head(&id).ok()??;
            vec![id]
        }
        (None, None) => live_ids(repo).ok()?,
    };
    // The sweep brought the evidence notes red, flakes, post checks and targets judge.
    repo.swept_evidence();
    let fetched_unix = at
        .as_ref()
        .and_then(|_| sweep::last(&repo.common_dir))
        .map(|(_, unix)| unix);
    Some(Refreshed {
        trunk,
        fetched_at: at,
        fetched_unix,
        ids,
        targets,
        talk_accept: talk(repo),
        stale: None,
    })
}

/// G619: `bay sweep [change]`, the sweep a status read starts ([`crate::sweeper`]): under the
/// store's sweep lock, sweeps the remote, settles the store ([`settle_swept`]) and records the
/// sweep, then prints one JSON line of what it did (`done`, `open`, `busy`, `unserved` or
/// `failed`) for the read, which may have answered and left by then.
pub fn sweep_child(dir: &Path, change: Option<&str>) {
    use std::io::Write as _;
    let answer = swept_answer(dir, change);
    // The sweep fetched trunk: the coordination cache the `tool.before` hook reads is written
    // before the answer, so a read that returns on it never leaves the hook an older cache.
    crate::coord::after_command();
    let mut out = std::io::stdout().lock();
    // The read that started this sweep may have left: then no one reads the answer.
    let _ = writeln!(out, "{answer}").and_then(|()| out.flush());
}

fn swept_answer(dir: &Path, change: Option<&str>) -> Value {
    let Ok(first) = Repo::open_with(dir, crate::repo::Fetch::Never) else {
        return json!({"unserved": "the repository cannot be opened"});
    };
    let Some(plan) = Plan::of(&first) else {
        return json!({"unserved": "no config at the trunk this store holds"});
    };
    let lock = match sweep::lock(&first.common_dir) {
        Ok(Ok(lock)) => lock,
        Ok(Err(running)) => return json!({ "busy": running }),
        Err(why) => return json!({ "unserved": why }),
    };
    let named = change.map(str::to_owned).or_else(|| first.view_change(dir));
    if plan.window(&first).is_some() && holds_named(&first, named.as_deref()) {
        lock.end();
        return json!({"open": true});
    }
    test_wait("BAY_TEST_SWEEP_DELAY_MS");
    let swept = match sweep::sweep(&first.git, first.remote(), &plan.refspecs) {
        Ok(swept) => swept,
        Err(error) => {
            sweep::unreached(&first.common_dir, &error.message());
            lock.end();
            return json!({"unserved": "the remote sweep failed"});
        }
    };
    test_wait("BAY_TEST_SWEEP_HOLD_MS");
    let Some(trunk) = swept
        .remote_of(&plan.tracking, plan.held.as_deref())
        .map(str::to_owned)
    else {
        lock.end();
        return json!({"unserved": "the remote holds no trunk"});
    };
    let (common, remote) = (first.common_dir.clone(), first.remote().to_owned());
    let answer = match settle_swept(dir, change, first, &swept, &plan.targets, trunk) {
        Ok((_, refreshed)) => {
            sweep::remember(&common, &remote, &plan.refspecs, &swept);
            json!({"done": {
                "at": swept.at,
                "ids": refreshed.ids,
                "targets": refreshed.targets,
                "talk_accept": refreshed.talk_accept,
            }})
        }
        Err(Unsettled::Moved) => json!({ "moved": swept.at }),
        Err(Unsettled::Failed(message)) => json!({ "failed": message }),
    };
    lock.end();
    answer
}

/// In a test build, the milliseconds the variable `name` sets hold the sweep: before it asks the
/// remote (`BAY_TEST_SWEEP_DELAY_MS`, a sweep slower than a read's deadline), or after it
/// (`BAY_TEST_SWEEP_HOLD_MS`, a sweep that takes that long from its start to its end).
fn test_wait(name: &str) {
    if let Some(delay) = std::env::var(name)
        .ok()
        .filter(|_| TEST_HOOKS)
        .and_then(|text| text.parse().ok())
    {
        std::thread::sleep(std::time::Duration::from_millis(delay));
    }
}

use crate::version::TEST_HOOKS;

/// Why [`settle_swept`] settled no read.
enum Unsettled {
    /// The sweep moved the trunk to a tip whose config the store cannot open.
    Moved,
    /// A chain the read lists cannot be read.
    Failed(String),
}

/// The local half of [`fetch_swept`]: the moves `swept` found, each namespace by its rule.
fn settle_swept(
    dir: &Path,
    change: Option<&str>,
    first: Repo,
    swept: &Swept,
    targets: &[(String, String)],
    trunk: String,
) -> Result<(Repo, Refreshed), Unsettled> {
    let tracking = first.trunk_tracking_ref();
    let keys = crate::signing::Keys::load(&first.git);
    let own_log = crate::log::log_ref(&keys.executor_name);
    let own_notes: Vec<String> = keys
        .own_names()
        .into_iter()
        .map(evidence::notes_ref)
        .collect();
    let mut updates = swept.taken(&tracking, |name, _| name == tracking);
    updates.extend(swept.taken(crate::log::LOG_PREFIX, |name, found| {
        if name == own_log {
            matches!(found.flag, ' ' | '*')
        } else {
            found.flag != '-'
        }
    }));
    for prefix in [
        crate::fleet::FLEET_PREFIX,
        crate::fleet::FLEET_EXECUTOR_PREFIX,
        crate::names::LOCKS,
    ] {
        updates.extend(swept.taken(prefix, |_, _| true));
    }
    updates.extend(swept.taken(evidence::EVIDENCE_PREFIX, |name, found| {
        found.flag != '-' && !own_notes.iter().any(|own| own == name)
    }));
    for (_, local) in targets {
        updates.extend(swept.taken(local, |name, _| name == local));
    }
    sweep::apply(&first.git, &updates);
    crate::coord::fetched(&first.work_root);
    crate::fleet::record(&first, &swept.at);
    let repo = if swept.moves.contains_key(&tracking) {
        Repo::open_with(dir, crate::repo::Fetch::Never).map_err(|_| Unsettled::Moved)?
    } else {
        first
    };
    repo.swept_evidence();
    let read_targets: Vec<(String, String)> = repo
        .config
        .targets
        .iter()
        .map(|target| (target.reference(), crate::target::local_ref(&target.name)))
        .collect();
    let targets_fetched = read_targets == targets || crate::target::fetch(&repo);
    let named = change.map(str::to_owned).or_else(|| repo.view_change(dir));
    let (ids, talk_accept) = std::thread::scope(|scope| {
        // § 11c: a fetching command on the recipient's machine accepts and acks its talks.
        let talks = scope.spawn(|| crate::talk::poll::command(&repo, crate::live::unix_now()));
        scope.spawn(|| crate::lane::fetch(&repo));
        let _ = receipts::sync_seen(&repo, Some(swept));
        let ids = settle_listed(&repo, &Chain::new(&repo.git), named, swept);
        (
            ids,
            talks
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
        )
    });
    let refreshed = Refreshed {
        trunk,
        fetched_at: Some(swept.at.clone()),
        fetched_unix: Some(swept.unix),
        ids: ids.map_err(Unsettled::Failed)?,
        targets: targets_fetched,
        talk_accept,
        stale: None,
    };
    Ok((repo, refreshed))
}

fn run(dir: &Path, change: Option<&str>, deadline: Duration) -> Result<BayResult, String> {
    let started = Instant::now();
    let (repo, refreshed) = match fetch_swept(dir, change, started, started + deadline) {
        Some(Ok(swept)) => swept,
        Some(Err(refused)) => return Ok(*refused),
        None => {
            let repo = match Repo::open_fetched(dir) {
                Ok(repo) => repo,
                Err(error) => return Ok(error.into_result()),
            };
            let refreshed = fetch_all(
                &repo,
                change.map(str::to_owned).or_else(|| repo.view_change(dir)),
            )?;
            (repo, refreshed)
        }
    };
    let Refreshed {
        trunk,
        fetched_at,
        fetched_unix,
        ids,
        targets: targets_fetched,
        talk_accept,
        stale,
    } = refreshed;
    // The runtime adapter is asked only for a change this read names: a listing of every
    // change stays bounded however many have landed.
    let named = change.is_some() || repo.view_change(dir).is_some();
    let read = Read::Full {
        registry: crate::registry::Registry::current(&repo.git)?,
        ask_runtime: ids.len() == 1 && named,
    };
    // A read of one change scans its own view only, never every view on the machine (a fleet
    // finding, 2026-10-26: 43 views cost a named status about 26 s).
    let view = change.map(str::to_owned).or_else(|| repo.view_change(dir));
    let (shown, sections) = read_sections(
        &repo,
        trunk.as_str(),
        &ids,
        named,
        view.as_deref(),
        targets_fetched,
        &read,
    );
    let Shown { changes, more, .. } = shown?;
    let (leftovers, more_leftovers) = sections.leftovers?;
    let (red_rows_now, red_pending) = sections.red;
    let mut result = BayResult::ok(json!({
        "changes": changes,
        "more_changes": more_changes(&more),
        "flakes": sections.flakes,
        "flow": sections.flow,
        "leftovers": leftovers,
        "more_leftovers": more_changes(&more_leftovers),
        "quarantines": sections.quarantines,
        "red": red_rows_now,
        "red_pending": red_pending,
        "talks_owed": talks_owed(&repo, &sections.talks),
        "talks": sections.talks,
        "targets": sections.targets,
        "trunk": trunk,
        "unattributed": sections.unattributed,
        // G305: whether bay's hook judges agents' Git commands in this repository: `on` when
        // the config at trunk declares an agent adapter (`bay doctor` checks this box has its
        // entries). Off, `unattributed` counts what went around bay.
        "guard": {"hook": if repo.config.claude_adapter() { "on" } else { "off" }},
    }));
    if let (Some(detail), Some(accept)) = (result.detail.as_mut(), talk_accept) {
        detail["talk_accept"] = accept;
    }
    if let (Some(detail), Some(hold)) = (result.detail.as_mut(), sections.hold) {
        detail["hold"] = hold.to_json();
    }
    // G619: every read names how old its view of the remote is.
    if let Some(detail) = result.detail.as_mut() {
        detail["fetched_at"] = json!(fetched_at);
        detail["stale_s"] =
            json!(fetched_unix.map(|unix| crate::live::unix_now().saturating_sub(unix)));
    }
    let stale_read = stale.is_some();
    let mut warnings: Vec<Value> = stale.into_iter().collect();
    warnings.extend(sections.runner);
    warnings.extend(sections.not_linear);
    warnings.extend(sections.wake_failing);
    if let Some(detail) = result.detail.as_mut()
        && !warnings.is_empty()
    {
        detail["warnings"] = Value::from(warnings);
    }
    if let [only] = ids.as_slice() {
        result = result.with_change(only.clone());
    }
    result.freshness = Some(Freshness {
        trunk,
        fetched_at: fetched_at.unwrap_or_else(|| {
            if stale_read {
                CACHED
            } else {
                "not fetched (remote unreachable)"
            }
            .to_owned()
        }),
    });
    Ok(result)
}

/// N-20: a `trunk_not_linear` warning for each merge outside bay among the newest `[red] window`
/// first-parent commits of trunk that put bay landings off the first-parent line: the merge,
/// its parents, how many landings it pushed off (and which), and the recovery hint. Those
/// landings are still found ([`crate::trunkline`]); the warning says the host let the merge in.
/// A config bay cannot read names none: `red` and `unattributed` say why already.
fn not_linear(git: &crate::git::Git, trunk: &str) -> Vec<Value> {
    crate::red::window(git, trunk)
        .and_then(|window| crate::trunkline::walk(git, trunk, window, None, ""))
        .map(|line| {
            line.merges
                .iter()
                .map(crate::trunkline::ForeignMerge::to_json)
                .collect()
        })
        .unwrap_or_default()
}

/// G47: the open talks this actor sent or must answer, each with who owes the reply, its ages
/// and its standing; a lost or stale talk stays listed. Unreadable talks list nothing here.
fn talk_rows(repo: &Repo) -> Value {
    let Ok(me) = crate::change::current_actor(&repo.common_dir) else {
        return json!([]);
    };
    crate::talk::standing::rows(&repo.git, &me.name, crate::talk::standing::reader_now())
        .map_or_else(|why| json!({"unread": why}), Value::from)
}

/// G650: the ids of the open talks in `rows` whose reply this reader owes.
fn talks_owed(repo: &Repo, rows: &Value) -> Value {
    let Ok(me) = crate::change::current_actor(&repo.common_dir) else {
        return json!([]);
    };
    let owed: Vec<&Value> = rows
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["owes"] == me.name.as_str())
        .map(|row| &row["talk"])
        .collect();
    json!(owed)
}

/// G85: the trunk commits that did not land through bay, named and counted. An unreadable
/// trunk lists none and says why.
fn unattributed_rows(repo: &Repo, trunk: &str) -> Value {
    crate::unattributed::scan(&repo.git, trunk).unwrap_or_else(|why| json!({"unread": why}))
}

/// NEW-5: one row per promotion target (`crate::target::rows`), read after [`fetch_all`]
/// fetched the remote's target refs and the other producers' evidence (`fetched`: the remote
/// answered). Offline, the rows are read from the copies of the last fetch, and say so. A
/// judgement bay cannot make says why under `unread`, never reads as promotable.
fn target_rows(repo: &Repo, trunk: &str, fetched: bool, read: &Read) -> Value {
    if repo.config.targets.is_empty() {
        return json!([]);
    }
    // An unreadable registry never reads as none: evidence would then count unsigned.
    let current;
    let registry = match read {
        Read::Full { registry, .. } => registry.as_ref(),
        Read::Cached => match crate::registry::Registry::current(&repo.git) {
            Ok(registry) => {
                current = registry;
                current.as_ref()
            }
            Err(why) => return json!({"unread": why}),
        },
    };
    match crate::target::rows(
        &repo.git,
        &repo.config,
        trunk,
        registry,
        crate::live::unix_now(),
    ) {
        Ok(rows) => rows
            .into_iter()
            .map(|mut row| {
                row["fetched"] = json!(fetched);
                row
            })
            .collect(),
        Err(why) => json!({"unread": why}),
    }
}

/// G244: the post checks trunk is red on, each owned by the landing that broke it and naming
/// its suspect range; and (NEW-6) those no landing in the window answers, `red_pending`, never
/// clear. A judgement bay cannot make (a policy, config or registry it cannot read, G104b)
/// lists none in either and says why: an unread trunk is never read as not red.
fn red_rows(repo: &Repo, trunk: &str) -> (Value, Value) {
    crate::registry::Registry::current(&repo.git)
        .and_then(|registry| crate::red::shown(&repo.git, trunk, registry.as_ref()))
        .map_or_else(
            |why| (json!({"unread": why}), json!({"unread": why})),
            |judgement| judgement.to_json(),
        )
}

/// The open quarantines of the policy at trunk (ruling 8c), so a set-aside check is never
/// silent: each with its `owner`, its age in days from `since`, and whether it is `overdue`.
/// One past its end date stays in force and is shown as `overdue` (`DESIGN` § 7: an end never
/// changes a judgement by itself; the policy change that removes the entry does). An invalid policy
/// lists none here; `land` names it with `POLICY_INVALID`. A policy bay cannot read (G104b)
/// lists none and says why: an unread policy is never read as no quarantine.
fn quarantines(repo: &Repo, trunk: &str) -> Value {
    open_quarantines(&repo.git, trunk).map_or_else(|why| json!({"unread": why}), Value::from)
}

/// Per required check of the policy at trunk, pre-merge and post (0.6.6: a post check's retry
/// that passes turns no main red, so its flakes are counted here), how many of the newest
/// first-parent landings (the `[red] window`) carry runs of it that combine to `flaky`, read
/// from the evidence already in this clone: a check that flakes is seen before anyone
/// quarantines it. Each row names `landings`, how many were read, and the check's
/// `retry_on_failure` and `allow_flaky`. A policy, config or registry bay cannot read lists
/// none and says why.
fn flake_rows(repo: &Repo, trunk: &str, read: &Read) -> Value {
    // An unreadable registry never reads as none: evidence would then count unsigned.
    let current;
    let registry = match read {
        Read::Full { registry, .. } => registry.as_ref(),
        Read::Cached => match crate::registry::Registry::current(&repo.git) {
            Ok(registry) => {
                current = registry;
                current.as_ref()
            }
            Err(why) => return json!({"unread": why}),
        },
    };
    flakes(&repo.git, trunk, registry).map_or_else(|why| json!({"unread": why}), Value::from)
}

/// The rows of [`flake_rows`].
///
/// # Errors
///
/// The policy or the config at `trunk` cannot be read (G104b), a note is unreadable, or a git
/// fault.
fn flakes(
    git: &crate::git::Git,
    trunk: &str,
    registry: Option<&crate::registry::Registry>,
) -> Result<Vec<Value>, String> {
    let policy = match crate::policy::load_at(git, trunk) {
        Ok(Some(policy)) => policy,
        Ok(None) | Err(crate::policy::LoadError::Invalid(_)) => return Ok(Vec::new()),
        Err(crate::policy::LoadError::Git(why)) => return Err(why),
    };
    let required: Vec<&crate::policy::ManifestEntry> =
        policy.manifest.iter().chain(&policy.post).collect();
    if required.is_empty() {
        return Ok(Vec::new());
    }
    let window = crate::red::window(git, trunk)?.to_string();
    let landings = git
        .run(&[
            "log",
            "--first-parent",
            "-n",
            &window,
            "--format=%H %T",
            trunk,
        ])
        .map_err(|error| error.message())?;
    let landings: Vec<(&str, &str)> = landings
        .lines()
        .filter_map(|line| line.split_once(' '))
        .collect();
    // G608: one read of the window's records, never one a landing (a fleet finding, 2026-10-31:
    // the window of 60 landings cost a status about 60 reads of the evidence refs).
    let trees: Vec<&str> = landings.iter().map(|&(_, tree)| tree).collect();
    let records_of = evidence::records_for_many(git, &trees)?;
    let mut counts = vec![0_u64; required.len()];
    let mut read = 0_u64;
    for (&(commit, _), records) in landings.iter().zip(records_of) {
        read += 1;
        let records = policy.answering(evidence::counted(git, records, registry)?);
        for (entry, count) in required.iter().zip(counts.iter_mut()) {
            let id = evidence::CheckId::new(&entry.id, &entry.environment);
            let subject = policy.commit_keyed(&id).then_some(commit);
            // Each landing was judged under the manifest of its own base: any one of them
            // under which its runs combine to flaky counts it once.
            let mut digests: Vec<&str> = records
                .iter()
                .filter(|record| record.check == id)
                .map(|record| record.manifest_digest.as_str())
                .collect();
            digests.sort_unstable();
            digests.dedup();
            if digests.iter().any(|digest| {
                evidence::combine(&records, &id, digest, subject) == evidence::Combined::Flaky
            }) {
                *count += 1;
            }
        }
    }
    Ok(required
        .iter()
        .zip(counts)
        .map(|(entry, flaky)| {
            let id = evidence::CheckId::new(&entry.id, &entry.environment);
            let check = policy.check(&id);
            json!({
                "check": entry.id,
                "environment": entry.environment,
                "flaky": flaky,
                "landings": read,
                "retry_on_failure": check.map_or(0, |check| check.retry_on_failure),
                "allow_flaky": check.is_some_and(|check| check.allow_flaky),
            })
        })
        .collect())
}

/// The open quarantines of the policy at `at`, each with whether it is overdue today; none
/// without a policy or with an invalid one. Read by `status` and by `bay doctor` (`DESIGN` § 7).
///
/// # Errors
///
/// The policy file cannot be read (G104b), or a git fault.
pub(crate) fn open_quarantines(git: &crate::git::Git, at: &str) -> Result<Vec<Value>, String> {
    let policy = match crate::policy::load_at(git, at) {
        Ok(Some(policy)) => policy,
        Ok(None) | Err(crate::policy::LoadError::Invalid(_)) => return Ok(Vec::new()),
        Err(crate::policy::LoadError::Git(why)) => return Err(why),
    };
    let now = now_rfc3339();
    let today = now.get(..10).unwrap_or_default();
    Ok(policy
        .quarantined
        .iter()
        .map(|quarantine| {
            json!({
                "check": quarantine.id,
                "environment": quarantine.environment,
                "reason": quarantine.reason,
                "end": quarantine.end,
                "owner": quarantine.owner,
                "since": quarantine.since,
                "age_days": quarantine.age_days(today),
                "overdue": quarantine.overdue(today),
            })
        })
        .collect())
}

/// The reader's own views on this machine, each with its change state, its age and whether it
/// has uncheckpointed edits (G41), and the ids of the views a listing names without checking
/// them. No pull-request state, no chat activity.
fn leftovers(
    repo: &Repo,
    trunk: &str,
    only: Option<&str>,
    me: Option<&str>,
) -> Result<(Vec<Value>, Vec<String>), String> {
    // W5: every chain in one batch (G200b), never one read per view.
    let chain = Chain::new(&repo.git);
    let mut records: std::collections::BTreeMap<String, ChangeRecord> = chain
        .read_all()
        .map_err(|error| error.message())?
        .into_iter()
        .map(|held| (held.id, held.record))
        .collect();
    let mut checked = Vec::new();
    let mut more = Vec::new();
    for id in view::all(repo)
        .into_iter()
        .filter(|id| only.is_none_or(|only| only == id))
    {
        let Some(record) = records.remove(&id) else {
            continue;
        };
        // A listing checks the reader's own views, at most LISTED of them: checking every view
        // on a shared box cost a status minutes (a fleet finding, 2026-10-26: 256 views). The
        // views of another actor's ended changes are named, never checked (G609, a fleet
        // finding, 2026-10-31: 20 such views cost each status of 34 agents about 12 s); `bay
        // status <change>` checks one.
        let mine = me.is_none_or(|me| me == record.actor.name);
        if only.is_none() && (!mine || checked.len() >= LISTED) {
            if mine || ended(record.state) {
                more.push(id);
            }
            continue;
        }
        checked.push((id, record));
    }
    // G624: the checked views' landings from one log of trunk, never one log per view.
    let bases: Vec<(&str, &str)> = checked
        .iter()
        .map(|(id, record)| (record.base_first.as_str(), id.as_str()))
        .collect();
    let scans = scan_many(&repo.git, trunk, &bases)?;
    let mut out = Vec::new();
    for (id, record) in checked {
        let landed = scans
            .get(&id)
            .is_some_and(|scan| !scan.publications.is_empty());
        let state = if landed {
            json!("landed")
        } else {
            json!(record.state)
        };
        let path = repo.view_path(&id);
        let latest = chain.latest_draft(&id).map_err(|error| error.message())?;
        let uncheckpointed = if view::exists(&path) {
            json!(view::uncheckpointed(&path, &latest).map_err(|error| error.message())?)
        } else {
            json!(null)
        };
        // § 9a: after a landing, the view is releasable only when it holds no newer work than
        // the published revision. Newer work is kept, never cleaned away.
        let mut newer_work = false;
        if landed && view::exists(&path) {
            let published = last_revision(&record, &latest);
            newer_work =
                view::uncheckpointed(&path, &published).map_err(|error| error.message())?;
            let next = if newer_work {
                view::Journal::Published
            } else {
                view::Journal::Releasable
            };
            if view::journal(repo, &id) != Some(next) {
                view::set_journal(repo, &id, next)?;
            }
        }
        out.push(json!({
            "change": id,
            "view": path.display().to_string(),
            "state": state,
            "age_s": view::age_s(repo, &id),
            "uncheckpointed": uncheckpointed,
            "newer_work": newer_work,
            "journal": view::journal(repo, &id),
        }));
    }
    out.extend(stale_inflight(repo, &chain)?);
    Ok((out, more))
}

/// The revision a view is measured against once its change ended (§ 9a): the change's last
/// published operation's, else its `latest` draft. `leftovers` and the G97 release
/// (`crate::start`) both read it, so a view `status` calls releasable is one `start` may release.
#[must_use]
pub fn last_revision(record: &ChangeRecord, latest: &str) -> String {
    record
        .operations
        .iter()
        .rev()
        .find(|op| op.state == crate::change::OpState::Published)
        .map_or_else(|| latest.to_owned(), |op| op.revision.clone())
}

/// The in-flight refs this store still holds whose operation ended or is gone (G173): an
/// executor that was killed mid-run left them. Each names its ref, its operation and the
/// object it holds; deleting the ref is the cleanup. A ref of an operation still running is
/// that operation's, and is not listed.
fn stale_inflight(repo: &Repo, chain: &Chain) -> Result<Vec<Value>, String> {
    let listed = repo
        .git
        .run(&[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            crate::names::INFLIGHT_PREFIX,
        ])
        .map_err(|error| error.message())?;
    let mut out = Vec::new();
    for line in listed.lines() {
        let Some((name, object)) = line.split_once(' ') else {
            continue;
        };
        let op_id = name.trim_start_matches(crate::names::INFLIGHT_PREFIX);
        let change = op_id.split_once('.').map_or(op_id, |(change, _)| change);
        let op = chain
            .read(change)
            .ok()
            .and_then(|(_, record)| record.operations.into_iter().find(|op| op.op_id == op_id));
        if op.as_ref().is_some_and(|op| !op.state.is_terminal()) {
            continue;
        }
        out.push(json!({
            "kind": "inflight",
            "ref": name,
            "change": change,
            "op_id": op_id,
            "object": object,
            "op_state": op.map(|op| op.state),
        }));
    }
    Ok(out)
}

/// The counted records on `tree` whose id begins, after the producer, with `prefix`.
fn records_named(
    repo: &Repo,
    registry: Option<&crate::registry::Registry>,
    tree: &str,
    prefix: &str,
) -> Result<Vec<EvidenceRecord>, String> {
    let all = evidence::counted(&repo.git, evidence::records_for(&repo.git, tree)?, registry)?;
    Ok(all
        .into_iter()
        .filter(|record| {
            record
                .id
                .split_once('/')
                .is_some_and(|(_, rest)| rest.starts_with(prefix))
        })
        .collect())
}

/// Worker time on `records` (`ROADMAP` § 3): the wall time bay measured around each adapter
/// run and the time the adapter reported for its units, each summed. A record that carries no
/// time still counts, as `untimed`.
fn cost(records: &[EvidenceRecord]) -> Value {
    let round = |sum: f64| (sum * 1000.0).round() / 1000.0;
    json!({
        "records": records.len(),
        "untimed": records.iter().filter(|record| record.seconds.is_none()).count(),
        "worker_seconds": round(records.iter().filter_map(|record| record.seconds).sum()),
        "unit_seconds": round(
            records
                .iter()
                .flat_map(|record| &record.units)
                .filter_map(|unit| unit.seconds)
                .sum()
        ),
    })
}

/// How `describe` reads a change. The hook's path reads no evidence (its costs are `null`) and
/// only the input cache; a full read counts evidence under the registry and runs providers,
/// and asks the runtime adapter when `ask_runtime` (one named change).
enum Read {
    Cached,
    Full {
        registry: Option<crate::registry::Registry>,
        ask_runtime: bool,
    },
}

/// The cost of the change's last operation, over every record its attempts produced
/// (discarded attempts included), and of its landing, over the records on the landed tree that
/// the publishing operation made (`null` when not landed).
fn costs(
    repo: &Repo,
    record: &ChangeRecord,
    landed: Option<&crate::publish::Found>,
    registry: Option<&crate::registry::Registry>,
) -> Result<(Value, Value), String> {
    let mut op_records = Vec::new();
    for attempt in record.operations.last().map_or(&[][..], |op| &op.attempts) {
        let prefix = format!("{}-", attempt.candidate_id);
        op_records.extend(records_named(repo, registry, &attempt.tree, &prefix)?);
    }
    let publishing = landed.and_then(|found| {
        record
            .operations
            .iter()
            .rev()
            .find(|op| op.revision == found.revision)
            .map(|op| (found, op))
    });
    let landing = match publishing {
        Some((found, op)) => {
            let tree = repo
                .git
                .run(&["rev-parse", &format!("{}^{{tree}}", found.commit)])
                .map_err(|error| error.message())?;
            let prefix = format!("{}.", op.op_id);
            cost(&records_named(repo, registry, &tree, &prefix)?)
        }
        None => json!(null),
    };
    Ok((cost(&op_records), landing))
}

/// § 8: required versions are derived on read at trunk. Only a live change has live inputs: a
/// landed, superseded or abandoned one keeps its state, and no provider runs for it. A ready
/// change is live: a moved input takes back the readiness it claims.
fn live_inputs(
    repo: &Repo,
    record: &ChangeRecord,
    landed: bool,
    trunk: &str,
    probe: Probe,
) -> Result<Vec<crate::inputs::Judgement>, String> {
    let live = record.state.is_live();
    if landed || !live {
        return Ok(Vec::new());
    }
    crate::inputs::judge(repo, record, trunk, probe)
}

/// A one-off status cannot watch a lease; it reports a suspected expiry from the age of the
/// chain head and never claims (§ 9a).
fn lease(
    repo: &Repo,
    record: &ChangeRecord,
    op: &crate::change::Operation,
    published: bool,
) -> Value {
    if op.owner.is_none()
        || !matches!(
            op.state,
            crate::change::OpState::Running | crate::change::OpState::Checked
        )
        || published
    {
        return json!(null);
    }
    let age = repo
        .git
        .run(&[
            "log",
            "-1",
            "--format=%ct",
            &crate::change::change_ref(&record.id),
        ])
        .ok()
        .and_then(|time| time.trim().parse::<u64>().ok())
        .map(|time| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |now| now.as_secs())
                .saturating_sub(time)
        });
    json!(
        if age.is_some_and(|age| age >= repo.config.lease().as_secs()) {
            "suspected expired"
        } else {
            "held"
        }
    )
}

/// § 7: the completion dimensions, each from its own evidence, and the verdict of the profile
/// `[policy] profile` selects; never collapsed into "done". The hook's read reads no evidence,
/// so it confirms no runtime dimension and judges no profile (`profile` is `null`).
fn completion(
    repo: &Repo,
    record: &ChangeRecord,
    trunk: &str,
    landed: Option<&crate::publish::Found>,
    read: &Read,
) -> Result<Value, String> {
    let unchecked = landed.and_then(|found| unchecked(&repo.git, record, found));
    let checked = unchecked.is_none()
        && (landed.is_some()
            || record.operations.last().is_some_and(|op| {
                op.attempts
                    .last()
                    .is_some_and(|attempt| attempt.verdict == "pass")
            }));
    let mut out = completion_dimensions(repo, record, trunk, landed, read, checked)?;
    out["unchecked"] = json!(unchecked);
    Ok(out)
}

/// Why landed `found` was never checked, or `None` when it was: judged at its base by
/// [`crate::publish::unchecked_reason`] with the attempt that built it, fail closed.
fn unchecked(
    git: &crate::git::Git,
    record: &ChangeRecord,
    found: &crate::publish::Found,
) -> Option<&'static str> {
    let verdict = record
        .operations
        .iter()
        .flat_map(|op| op.attempts.iter())
        .find(|attempt| attempt.commit == found.commit)
        .map(|attempt| attempt.verdict.as_str());
    crate::publish::unchecked_reason(git, verdict, &found.commit)
}

/// The completion dimensions of [`completion`], with `checked` as judged there.
fn completion_dimensions(
    repo: &Repo,
    record: &ChangeRecord,
    trunk: &str,
    landed: Option<&crate::publish::Found>,
    read: &Read,
    checked: bool,
) -> Result<Value, String> {
    let Read::Full {
        registry,
        ask_runtime,
    } = read
    else {
        return Ok(json!({
            "merged": landed.is_some(),
            "checked": checked,
            "running": "unconfirmed",
            "effective": "unconfirmed",
            "runtime": null,
            "trust": null,
            "profile": null,
        }));
    };
    let (name, require) = selected_profile(repo, trunk, landed);
    // The adapter is asked only when its answer can change the verdict: the profile requires a
    // runtime dimension and a registry exists, without which nothing it prints could count.
    let needs_runtime = require.as_ref().is_ok_and(|profile| {
        profile.require.contains(&Need::Builtin(Dimension::Running))
            || profile
                .require
                .contains(&Need::Builtin(Dimension::Effective))
    });
    // N-21: under `signing = "approvals"` no producer signs, so nothing it prints counts.
    let signs_all = crate::registry::Registry::signing(registry.as_ref()).is_some();
    let ask = *ask_runtime && needs_runtime && signs_all;
    let (confirmed, runtime) = match landed {
        Some(found) => runtime(repo, record, found, registry.as_ref(), ask)?,
        None => (Confirmed::default(), json!(null)),
    };
    let mut met: Vec<Need> = [
        (Dimension::Published, landed.is_some()),
        (Dimension::Checked, checked),
        (Dimension::Running, !confirmed.running.is_empty()),
        (Dimension::Effective, !confirmed.effective.is_empty()),
    ]
    .into_iter()
    .filter_map(|(dimension, met)| met.then_some(Need::Builtin(dimension)))
    .collect();
    // G160: without a registry, or under `signing = "approvals"` (N-21), `checked` rests on
    // unsigned records alone. A runtime dimension never counts an unsigned record
    // ([`crate::runtime`]), and `published` is trunk's own fact.
    let mut unsigned: Vec<Need> = if !signs_all && checked {
        vec![Need::Builtin(Dimension::Checked)]
    } else {
        Vec::new()
    };
    // G150: the site's own dimensions, each judged for the landing; reported here and required
    // only through a profile, never in the landing verdict.
    let mut sites = serde_json::Map::new();
    for dimension in declared_dimensions(repo, trunk, landed) {
        let (state, records, rests_unsigned) = match landed {
            Some(found) => site_dimension(
                repo,
                &dimension,
                &record.id,
                found,
                trunk,
                registry.as_ref(),
            )?,
            None => ("missing", Vec::new(), false),
        };
        let need = Need::Site(dimension.name.clone());
        if matches!(state, "met" | "answered") {
            met.push(need.clone());
            if rests_unsigned {
                unsigned.push(need);
            }
        }
        sites.insert(
            dimension.name,
            json!({"state": state, "kind": dimension.kind, "producers": dimension.producers, "records": records}),
        );
    }
    let word = |ids: &[String]| {
        if ids.is_empty() {
            "unconfirmed"
        } else {
            "confirmed"
        }
    };
    Ok(json!({
        "merged": landed.is_some(),
        "checked": checked,
        "running": word(&confirmed.running),
        "effective": word(&confirmed.effective),
        "runtime": runtime,
        "trust": evidence::trust(registry.as_ref()),
        "dimensions": sites,
        "profile": completion::verdict(&name, require, &met, &unsigned),
    }))
}

/// G160: the evidence on the landed tree, or else on the last attempt's tree, each record named
/// `verified`, `unverifiable` or `unsigned`, under the result's `trust`. The hook's read reads
/// no evidence (`null`).
fn evidence_read(
    repo: &Repo,
    record: &ChangeRecord,
    landed: Option<&crate::publish::Found>,
    read: &Read,
) -> Result<Value, String> {
    let Read::Full { registry, .. } = read else {
        return Ok(json!(null));
    };
    let tree = match landed {
        Some(found) => Some(
            repo.git
                .run(&["rev-parse", &format!("{}^{{tree}}", found.commit)])
                .map_err(|error| error.message())?,
        ),
        None => record
            .operations
            .last()
            .and_then(|op| op.attempts.last())
            .map(|attempt| attempt.tree.clone()),
    };
    let Some(tree) = tree else {
        return Ok(json!(null));
    };
    let records = evidence::tagged(
        &repo.git,
        evidence::records_for(&repo.git, &tree)?,
        registry.as_ref(),
    )?;
    Ok(json!({
        "tree": tree,
        "trust": evidence::trust(registry.as_ref()),
        "records": records.iter().map(|(record, verified)| json!({
            "id": record.id,
            "kind": record.kind,
            "check": record.check.id,
            "verdict": record.verdict,
            "reasons": evidence::reasons(&record.units),
            "verified": verified,
        })).collect::<Vec<_>>(),
    }))
}

/// The selected profile's name and requirement, both read where the change is judged: at the
/// landing's accepted base once landed, else at trunk. Neither the work tree's config nor the
/// landing itself selects or relaxes the profile it is judged by. A config or policy bay
/// cannot read there makes the requirement unknown.
fn selected_profile(
    repo: &Repo,
    trunk: &str,
    landed: Option<&crate::publish::Found>,
) -> (String, Result<completion::Profile, String>) {
    let at = landed.map_or_else(|| trunk.to_owned(), |found| format!("{}^1", found.commit));
    let config = match crate::config::Config::at(&repo.git, &at) {
        Ok(config) => config,
        Err(why) => return (repo.config.profile().to_owned(), Err(why)),
    };
    let name = config.profile().to_owned();
    let require = match crate::policy::load_at(&repo.git, &at) {
        Ok(policy) => completion::requirement(
            &name,
            &policy.map(|policy| policy.profiles).unwrap_or_default(),
        ),
        Err(error) => Err(String::from(error)),
    };
    (name, require)
}

/// The `[[dimension]]` tables of the policy a change is judged by: at the landing's accepted
/// base once landed, else at trunk, as [`selected_profile`] reads it (G150).
fn declared_dimensions(
    repo: &Repo,
    trunk: &str,
    landed: Option<&crate::publish::Found>,
) -> Vec<completion::SiteDimension> {
    let at = landed.map_or_else(|| trunk.to_owned(), |found| format!("{}^1", found.commit));
    crate::policy::load_at(&repo.git, &at)
        .ok()
        .flatten()
        .map(|policy| policy.dimensions)
        .unwrap_or_default()
}

/// G150: one site dimension of a landing. Records count when they are of the dimension's kind,
/// from one of its producers, about that landing commit (never a tree alone: K11b), and, with a
/// registry, verify against the class file their kind names (K11c). A non-pass record is
/// answered only by a later landing whose `Fixes:`, `Reverts:` or `Supersedes:` names the
/// change, never by a later pass (G29, K11a). Returns the state (`met`, `answered`, `rejected`
/// or `missing`), the counted record ids, and whether they rest on unsigned records alone.
fn site_dimension(
    repo: &Repo,
    dimension: &completion::SiteDimension,
    change: &str,
    found: &crate::publish::Found,
    trunk: &str,
    registry: Option<&crate::registry::Registry>,
) -> Result<(&'static str, Vec<String>, bool), String> {
    let git = &repo.git;
    // The producers' notes refs, which this box may not hold yet; one that has none is no
    // error: its records are missing.
    for producer in &dimension.producers {
        let _ = repo.fetch_refs(&[evidence::notes_ref(producer)]);
    }
    let tree = git
        .run(&["rev-parse", &format!("{}^{{tree}}", found.commit)])
        .map_err(|error| error.message())?;
    let mut counted = Vec::new();
    for record in evidence::records_for(git, &tree)? {
        if record.kind != dimension.kind
            || !dimension.producers.contains(&record.producer.name)
            || record.subject.commit != found.commit
        {
            continue;
        }
        // N-21: under `signing = "approvals"` only an approval must verify.
        if let Some(registry) = registry
            && evidence::signed_kind(registry, &record.kind)
            && !record.verifies_in(git, registry)?
        {
            continue;
        }
        counted.push(record);
    }
    let ids = counted.iter().map(|record| record.id.clone()).collect();
    let unsigned = registry
        .is_none_or(|registry| !evidence::signed_kind(registry, &dimension.kind))
        && !counted.is_empty();
    if counted.is_empty() {
        return Ok(("missing", ids, false));
    }
    if counted
        .iter()
        .all(|record| record.verdict == evidence::Verdict::Pass)
    {
        return Ok(("met", ids, unsigned));
    }
    let answered = crate::lineage::RELATIONS.iter().any(|(_, trailer)| {
        git.run(&[
            "log",
            "--first-parent",
            "--format=%H",
            &format!("--grep=^{trailer}: {change}$"),
            &format!("{}..{trunk}", found.commit),
        ])
        .is_ok_and(|out| !out.trim().is_empty())
    });
    Ok((
        if answered { "answered" } else { "rejected" },
        ids,
        unsigned,
    ))
}

/// The counted runtime records on landed `found`'s tree. When `ask` and they do not confirm
/// both runtime dimensions, the runtime adapter is asked too, and what it printed that counts
/// is counted for this read; nothing is written. An adapter that fails confirms nothing and is
/// named, never read as a pass.
fn runtime(
    repo: &Repo,
    record: &ChangeRecord,
    found: &crate::publish::Found,
    registry: Option<&crate::registry::Registry>,
    ask: bool,
) -> Result<(Confirmed, Value), String> {
    let tree = repo
        .git
        .run(&["rev-parse", &format!("{}^{{tree}}", found.commit)])
        .map_err(|error| error.message())?;
    let mut records = evidence::records_for(&repo.git, &tree)?;
    let count = |records: &[EvidenceRecord]| {
        crate::runtime::confirmed(&repo.git, records.to_vec(), registry, &found.commit)
    };
    let mut confirmed = count(&records)?;
    let (mut adapter, mut error, mut refused) = ("not asked", None, Vec::new());
    if ask && (confirmed.running.is_empty() || confirmed.effective.is_empty()) {
        match crate::runtime::observe(repo, &record.id, &found.commit, &tree, registry) {
            Ok(None) => adapter = "unconfigured",
            Ok(Some(observed)) => {
                adapter = "ran";
                refused = observed.refused;
                records.extend(observed.kept);
                confirmed = count(&records)?;
            }
            Err(why) => {
                adapter = "failed";
                error = Some(why);
            }
        }
    }
    Ok((
        confirmed.clone(),
        json!({
            "running": confirmed.running,
            "effective": confirmed.effective,
            "adapter": adapter,
            "error": error,
            "refused": refused,
        }),
    ))
}

/// G91: the post-publication checks of landed `landed`, each pending until a record for its
/// tree arrives ([`crate::publish::post_checks`]); `null` when not landed, or on the hook's
/// read, which reads no evidence. A record arrives from a runner elsewhere, so while one is
/// pending the other producers' evidence is fetched first. A fault is named, never hidden.
fn post_checks(repo: &Repo, landed: Option<&crate::publish::Found>, read: &Read) -> Value {
    let (Some(found), Read::Full { registry, .. }) = (landed, read) else {
        return json!(null);
    };
    let judge = || crate::publish::post_checks(&repo.git, &found.commit, registry.as_ref());
    let pending = |rows: &[Value]| rows.iter().any(|row| row["state"] == "pending");
    let rows = match judge() {
        Ok(rows) if pending(&rows) => {
            repo.fetch_evidence_once(&crate::signing::Keys::load(&repo.git));
            judge()
        }
        other => other,
    };
    rows.map_or_else(|why| json!({"unread": why}), Value::from)
}

/// G245: the approvals the last operation of `record` waits on, when it is held with
/// `APPROVAL_PENDING` and the read is full: who can approve and the candidate to name, judged
/// like its land. An approver's clone may hold no registry yet: who can approve is then read
/// from the remote's, never reported as nobody.
fn approval_of(repo: &Repo, record: &ChangeRecord, read: &Read) -> Result<Option<Value>, String> {
    let Read::Full { registry, .. } = read else {
        return Ok(None);
    };
    approval_pending(repo, record, registry.as_ref())
}

/// G245: the `operation.approval` of `bay status` for `record`, read with `registry` (the
/// repository's, when it holds one): each approval check still owed, who can give it, the paths
/// it covers, and the candidate to name; `None` when the last operation is not held with
/// `APPROVAL_PENDING`. `bay waiting` and `bay approve` (G280, G281) read it here, so all three
/// name one candidate.
///
/// # Errors
///
/// A git fault reading the registry.
pub fn approval_pending(
    repo: &Repo,
    record: &ChangeRecord,
    registry: Option<&crate::registry::Registry>,
) -> Result<Option<Value>, String> {
    let Some(op) = record.operations.last() else {
        return Ok(None);
    };
    let held = op.outcome.as_deref().and_then(Reason::from_code) == Some(Reason::ApprovalPending);
    let Some(attempt) = op.attempts.last().filter(|_| held) else {
        return Ok(None);
    };
    let fetched = if registry.is_none() {
        repo.fetch_registry();
        crate::registry::Registry::current(&repo.git)?
    } else {
        None
    };
    // G292: an earlier candidate's approval holds while the files it covers are unchanged.
    let mut approval = match crate::publish::pending_approval(
        &repo.git,
        (
            attempt.base.as_str(),
            attempt.commit.as_str(),
            attempt.tree.as_str(),
        ),
        &crate::throughput::candidates_of(record),
        registry.or(fetched.as_ref()),
    ) {
        Ok(Some(approval)) => approval,
        Ok(None) => return Ok(None),
        Err(why) => return Ok(Some(json!({"unread": why}))),
    };
    // G281: the one command an approver runs to answer it.
    approval["approve"] = json!(format!("bay approve {}", record.id));
    Ok(Some(approval))
}

/// G658: the work a refused operation would undo.
fn held_by(op: &crate::change::Operation, operation: &mut Value) {
    if let Some(undoes) = &op.undoes {
        operation["undoes"] = json!(undoes);
    }
}

/// What an operation under way waits on, besides its place: § 7, a worker for the
/// environments no worker answered for; G177, a conflict with trunk the executor found while
/// the operation waits behind the queue's head, as `MERGE_CONFLICT` with its paths, so its
/// agent can refresh before the head reaches it.
fn waiting(op: &crate::change::Operation, operation: &mut Value) {
    if let Some(unavailable) = &op.unavailable {
        operation["waiting_for"] = json!("worker");
        operation["environments"] = json!(unavailable.environments);
    }
    if let Some(conflict) = &op.early_conflict {
        let reason = Reason::MergeConflict;
        operation["conflict"] = json!({
            "reason": reason.code(),
            "conflicts": conflict.paths,
            "trunk": conflict.trunk,
            "allowed": reason.allowed(),
            "retry_when": reason.retry_when(),
            "next_step": reason.next(),
        });
    }
}

/// ROADMAP § 3: an operation that ended shows its terminal record's timing.
fn timing_of(terminal: &[Value], op_id: &str) -> Option<Value> {
    terminal
        .iter()
        .rev()
        .filter(|entry| entry["op_id"] == op_id)
        .find_map(crate::timing::from_terminal)
}

/// What the record of `op` is: `published`, the id of its newest receipt (synced or in
/// `unsynced`), an outcome in the chain, or `missing`.
/// The newest receipt of `op`, synced or not.
fn newest_receipt<'a>(
    record: &'a ChangeRecord,
    unsynced: &[&'a Receipt],
    op: &crate::change::Operation,
) -> Option<&'a Receipt> {
    record
        .receipts
        .iter()
        .chain(unsynced.iter().copied())
        .rev()
        .find(|receipt| receipt.op_id.as_deref() == Some(op.op_id.as_str()))
}

/// G603: a fault stops an operation with no outcome, so no reason says why or what to do; its
/// receipt does.
fn fault(
    record: &ChangeRecord,
    unsynced: &[&Receipt],
    op: &crate::change::Operation,
    published: bool,
) -> Option<Value> {
    let stopped = matches!(
        op.state,
        crate::change::OpState::Blocked | crate::change::OpState::Failed
    );
    if published || op.outcome.is_some() || !stopped {
        return None;
    }
    let receipt = newest_receipt(record, unsynced, op)?;
    Some(json!({
        "step": receipt.step,
        "why": receipt.detail.as_str(),
        "recovery": receipt.recovery,
    }))
}

fn receipt_state(
    record: &ChangeRecord,
    unsynced: &[&Receipt],
    op: &crate::change::Operation,
    published: bool,
) -> Value {
    let receipt = newest_receipt(record, unsynced, op).map(|receipt| receipt.id.clone());
    // Never success without proof: an unpublished operation with no outcome and no receipt
    // has a missing record.
    match (&receipt, published, &op.outcome) {
        (_, true, _) => json!("merged"),
        (Some(id), false, _) => json!(id),
        (None, false, Some(_)) => json!("outcome recorded in the chain"),
        (None, false, None) => json!("missing"),
    }
}

fn describe(
    repo: &Repo,
    record: &ChangeRecord,
    trunk: &str,
    unsynced: &[&Receipt],
    read: &Read,
) -> Result<Value, String> {
    let found = scan(&repo.git, &record.base_first, trunk, &record.id)?;
    let landed = found.publications.first();
    let ((op_cost, landing_cost), probe) = match read {
        Read::Full { registry, .. } => {
            (costs(repo, record, landed, registry.as_ref())?, Probe::Run)
        }
        Read::Cached => ((json!(null), json!(null)), Probe::Cached),
    };
    let judged = live_inputs(repo, record, landed.is_some(), trunk, probe)?;
    let state = if landed.is_some() {
        "landed".to_owned()
    } else {
        serde_json::to_value(crate::inputs::derived_state(&judged).unwrap_or(record.state))
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default()
    };
    let terminal = crate::log::records_for(&repo.git, &record.id)?;
    let mut operation = record.operations.last().map(|op| {
        let published = landed.is_some_and(|found| found.revision == op.revision);
        let record_state = receipt_state(record, unsynced, op, published);
        let reason = op.outcome.as_deref().and_then(Reason::from_code);
        let lease = lease(repo, record, op, published);
        let mut operation = json!({
            "lease": lease,
            "owner": op.owner,
            "receipt": record_state,
            "allowed": reason.map(Reason::allowed),
            "retry_when": reason.map(Reason::retry_when),
            "next_step": reason.map(Reason::next),
            "op_id": op.op_id,
            "state": if landed.is_some_and(|found| found.revision == op.revision) { json!("published") } else { json!(op.state) },
            "outcome": op.outcome,
            "attempts": op.attempts.len(),
            // G644: whether a runner will ask this blocked operation again by itself.
            "asked_again": crate::publish::asked_again(op),
            "trunk_moved": op.trunk_moved,
            "policy_changed": op.policy_changed,
            "publish_retries": op.publish_retries,
            "candidate": op.attempts.last().map(|attempt| &attempt.candidate_id),
            "published": landed.is_some_and(|found| found.revision == op.revision),
            "cost": op_cost,
        });
        // G80: an operation under way says how long it has been and what comes next.
        if !published && !op.state.is_terminal() {
            let (elapsed, _, next) = crate::publish::progress(&repo.git, op);
            operation["elapsed_s"] = json!(elapsed);
            operation["next"] = json!(next);
        }
        if !published && !op.state.is_terminal() && op.state != crate::change::OpState::Blocked {
            waiting(op, &mut operation);
        }
        held_by(op, &mut operation);
        if let Some(fault) = fault(record, unsynced, op, published) {
            operation["fault"] = fault;
        }
        if let Some(timing) = timing_of(&terminal, &op.op_id) {
            operation["timing"] = timing;
        }
        operation
    });
    // G245: a change held for an approval names who can give it and the candidate to name.
    if let (Some(operation), Some(approval)) =
        (operation.as_mut(), approval_of(repo, record, read)?)
    {
        operation["approval"] = approval;
    }
    Ok(json!({
        "change": record.id,
        "intent": record.intent.reference,
        "lane": record.lane,
        "from": record.from,
        "state": state,
        "recorded_state": record.state,
        "landed": landed.map(|found| &found.commit),
        "landing_cost": landing_cost,
        "inputs": crate::inputs::to_json(judged.iter()),
        "invalidations": crate::inputs::to_json(judged.iter().filter(|judgement| judgement.is_stale())),
        "validated_candidate": record.validated_candidate,
        "operation": operation,
        "anomalies": found.anomalies.iter().map(|anomaly| json!({
            "commit": anomaly.commit,
            "why": "carries the Change-Id but is not a verified merge",
        })).collect::<Vec<_>>(),
        "completion": completion(repo, record, trunk, landed, read)?,
        "evidence": evidence_read(repo, record, landed, read)?,
        "post_checks": post_checks(repo, landed, read),
        "retention_cut": record.retention_cut,
        "terminal": terminal,
        "receipts": record.receipts.iter().filter(|receipt| {
            // Synced means on the remote: a receipt whose pending file remains is not, even
            // when the local chain already holds it.
            !unsynced.iter().any(|pending| pending.id == receipt.id)
        }).map(|receipt| json!({
            "id": receipt.id, "op_id": receipt.op_id, "step": receipt.step, "reason": receipt.reason,
            "recovery": receipt.recovery, "at": receipt.at, "synced": true,
            "why": crate::result::unknown_why(Some(&receipt.detail)),
        })).chain(unsynced.iter().map(|receipt| json!({
            "id": receipt.id, "op_id": receipt.op_id, "step": receipt.step, "reason": receipt.reason,
            "recovery": receipt.recovery, "at": receipt.at, "age_s": receipts::age_s(receipt), "synced": false,
            "why": crate::result::unknown_why(Some(&receipt.detail)),
        }))).collect::<Vec<_>>(),
    }))
}
