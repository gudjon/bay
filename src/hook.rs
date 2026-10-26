//! The scoped host hook (`DESIGN` § 10a): ref-prefix scoping by key class.
//!
//! Git does not tell a hook which key pushed, so the hook judges a push by the signatures on
//! what it adds. It runs as `pre-receive`, which sees every ref of the push at once and
//! rejects the whole push on any refusal. It applies only on a host with a key registry;
//! without one the host keeps just the append-only trunk (`host.rs`) and the promotion targets.
//!
//! Judged here: bay ref names (the fixed alphabet), trunk publications, the landed marker,
//! change chains (agent, executor and registry signatures; adoption; no deletion; the
//! retention cut), requests, evidence notes, executor logs, the registry, queue reservations
//! and releases, intent claims, the landed intent, live summary refs, talks (`crate::talk`), and
//! marker compaction into the shard indexes (`crate::compact`): a compacted name is never created
//! again, a marker is deleted only by its compaction, and a shard index only appends.
//!
//! N-21: when the policy at the host's trunk tip, before the push, says `[policy] signing =
//! "approvals"`, the hook judges only what that mode signs: the registry (only a registry key
//! moves it) and evidence notes, where every `approval` record added verifies against the
//! approver file and no record is withdrawn. Everything else is kept as on a host with no
//! registry. A push never sets the mode it is judged by: the mode is read before it lands.
//!
//! A push that moves a promotion target (`[[target]]`, NEW-5) is judged on every bay host, with
//! or without a registry (`targets`, `crate::target`): it is refused with `PROMOTION_REFUSED`
//! unless the target moves to a promotable commit.
//!
//! An adopt of another actor's open change is judged by the host's own clock (ruling R1): the
//! chain head's age is the age of its newest entry in the host reflog, and the last heartbeat
//! of the change's actor is the newest entry of a live ref's host reflog with a fresh row of the
//! change; both must be at least the takeover window (`[runner] takeover_s` on trunk) old.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use serde_json::Value;

use crate::change::{Chain, OpState, RECORD_FILE};
use crate::compact;
use crate::evidence;
use crate::git::{Git, GitError};
use crate::policy;
use crate::registry::{KeyClass, REGISTRY_REF, Registry};
use crate::signing;
use crate::throughput;

struct Update {
    old: String,
    new: String,
    name: String,
}

/// How [`Judge::publication`] judges a trunk commit's evidence (`crate::throughput`).
#[derive(Debug, PartialEq, Eq)]
enum Judged {
    /// On its own tree, by the policy at its parent.
    Parent,
    /// A batch member before the last: the batch tip's evidence, judged in the same push.
    InBatch,
    /// A batch tip: on its own tree, by the policy at the batch's base (this commit), over
    /// the batch's whole diff.
    BatchBase(String),
}

fn is_zero(oid: &str) -> bool {
    !oid.is_empty() && oid.bytes().all(|byte| byte == b'0')
}

/// G269: a pruned branch's archive (`refs/bay/archive/heads/<branch>`) is created once and
/// never moved or deleted: it is what the restore line of `bay prune` reads.
fn archive(update: &Update) -> Result<(), String> {
    if is_zero(&update.old) {
        Ok(())
    } else {
        Err("an archived branch is created once; it is never moved or deleted".to_owned())
    }
}

/// The words of the host's refusal of a push that would pass the queue's head, which the
/// merging side reads to know it waits its turn (`QUEUED`).
pub const QUEUE_HELD_BY: &str = "the publication queue is held by";

/// The most live refs a host holds and an adopt check reads (§ 10a: the hook's work is bounded).
const LIVE_REFS_MAX: usize = 256;
/// The most host reflog entries of one live ref an adopt check reads. An honest machine pushes
/// at most one summary per heartbeat, so this covers a takeover window of 64 heartbeats.
const LIVE_REFLOG_MAX: usize = 64;

/// Whether `git ls-tree -l` of a live commit lists `live.json` alone, a blob of at most
/// [`crate::live::LIVE_MAX_BYTES`].
fn live_tree_is_small(tree: &str) -> bool {
    one_small_file(tree, crate::live::LIVE_FILE, crate::live::LIVE_MAX_BYTES)
}

/// Whether `git ls-tree -l` of a commit lists `file` alone, a blob of at most `max` bytes.
fn one_small_file(tree: &str, file: &str, max: u64) -> bool {
    let mut lines = tree.lines();
    let (Some(line), None) = (lines.next(), lines.next()) else {
        return false;
    };
    let Some((entry, path)) = line.split_once('\t') else {
        return false;
    };
    let fields: Vec<&str> = entry.split_whitespace().collect();
    path == file
        && fields.len() == 4
        && fields[0] == "100644"
        && fields[1] == "blob"
        && fields[3].parse::<u64>().is_ok_and(|size| size <= max)
}

/// The deepest fan-out the hook looks for a note at: Git adds a level per 256-fold growth of a
/// notes tree, so four levels cover far more notes than any producer ref holds.
const NOTE_FANOUT_MAX: usize = 4;

/// G159 (owner ruling Q1): an evidence record is never withdrawn. Every record of the old note
/// on `tree` is in the new note (`None`: the note is removed) with the same JSON value, which is
/// the same canonical bytes its signature covers. The order of a note is not its meaning (records
/// combine by record, never latest-wins, § 7), so a reordering keeps every record. Only a
/// registry-signed retention cut may remove one, and no retention cut of evidence notes exists
/// yet: every removal is refused.
fn kept(old: &str, new: Option<&str>, tree: &str) -> Result<(), String> {
    let Ok(before) = serde_json::from_str::<Vec<Value>>(old) else {
        // An old note that is not a list of records holds no record to keep.
        return Ok(());
    };
    let after: Vec<Value> = match new {
        Some(text) => serde_json::from_str(text).unwrap_or_default(),
        None => Vec::new(),
    };
    for record in &before {
        if after.contains(record) {
            continue;
        }
        let id = record["id"].as_str().unwrap_or("(no id)");
        let how = if after.iter().any(|other| other["id"] == record["id"]) {
            "changed"
        } else {
            "withdrawn"
        };
        return Err(format!(
            "EVIDENCE_WITHDRAWN: record `{id}` {how} on the note of {tree}; an evidence record is never withdrawn, and no retention cut of evidence notes exists yet"
        ));
    }
    Ok(())
}

fn message(error: &GitError) -> String {
    error.message()
}

/// The live summaries a judgement reads, or why the first unreadable one cannot be read
/// (G104b): a summary this host cannot read refuses the push, never counts as none.
fn strict(summaries: Vec<crate::live::SummaryRead>) -> Result<Vec<Option<Value>>, String> {
    summaries.into_iter().collect()
}

/// § 10a: one push's compaction is bounded, so its judgement stays cheap.
fn compaction_bound(updates: &[Update]) -> Result<(), String> {
    let compactions = updates
        .iter()
        .filter(|update| {
            compact::compactable(&update.name) && is_zero(&update.new) && !is_zero(&update.old)
        })
        .count();
    if compactions > compact::PUSH_MAX {
        return Err(format!(
            "a push moves at most {} markers into the shard indexes; this one moves {compactions}",
            compact::PUSH_MAX
        ));
    }
    Ok(())
}

/// Judges one push. `input` is pre-receive's stdin: `<old> <new> <ref>` per line.
///
/// # Errors
///
/// The refusal, naming the ref and the rule.
pub fn pre_receive(git: &Git, input: &str) -> Result<(), String> {
    let registry = Registry::current(git)?;
    let updates: Vec<Update> = input
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            Some(Update {
                old: words.next()?.to_owned(),
                new: words.next()?.to_owned(),
                name: words.next()?.to_owned(),
            })
        })
        .collect();
    let trunk = format!("refs/heads/{}", crate::host::served_branch(git));
    // NEW-5: a promotion target moves only to a promotable commit, on any bay host; without a
    // registry its evidence counts unsigned, as everywhere else.
    targets(git, &updates, &trunk, registry.as_ref())?;
    let Some(registry) = registry else {
        return Ok(());
    };
    // N-21: `Registry::current` read the mode at the trunk tip as it stands before this push.
    if !registry.signs_all() {
        return approvals_only(git, &registry, &updates);
    }
    compaction_bound(&updates)?;
    // The talks this push creates, read once for every talk ref it judges (`crate::talk`).
    let talk_creates: Vec<(String, String)> = updates
        .iter()
        .filter(|update| {
            update.name.starts_with(crate::talk::TALK_PREFIX)
                && is_zero(&update.old)
                && !is_zero(&update.new)
        })
        .map(|update| (update.name.clone(), update.new.clone()))
        .collect();
    let judge = Judge {
        git,
        registry: &registry,
        updates: &updates,
        by_name: updates
            .iter()
            .map(|update| (update.name.as_str(), update))
            .collect(),
        appended: RefCell::new(HashMap::new()),
        talk_creates: crate::talk::read_created(git, &talk_creates),
        queue: std::cell::OnceCell::new(),
    };
    for update in &updates {
        let name = update.name.as_str();
        // § 10a: every bay ref name is validated first: ids from the fixed alphabet, operator
        // names short and lower-case, so no bay ref can be a directory of another.
        crate::names::bay_ref(name)?;
        let verdict = if let Err(why) = judge.not_compacted(update) {
            Err(why)
        } else if compact::compactable(name) && is_zero(&update.new) && !is_zero(&update.old) {
            judge.compacted_delete(update)
        } else if name.starts_with(compact::INDEX_PREFIX) {
            judge.landed_index(update)
        } else if name == trunk {
            judge.trunk(update)
        } else if let Some(id) = name.strip_prefix(crate::names::LANDED) {
            judge.landed(update, id, &trunk)
        } else if let Some(id) = name.strip_prefix(crate::names::CHANGES) {
            judge.chain(update, id)
        } else if let Some(id) = name.strip_prefix(crate::names::REQUESTS) {
            judge.request_ref(update, id)
        } else if let Some(producer) = name.strip_prefix(evidence::EVIDENCE_PREFIX) {
            judge.evidence(update, producer)
        } else if let Some(executor) = name.strip_prefix(crate::log::LOG_PREFIX) {
            judge.log(update, executor)
        } else if name == REGISTRY_REF {
            judge.registry(update)
        } else if let Some(seq) = name.strip_prefix(crate::queue::QUEUE_PREFIX) {
            judge.reservation(update, seq)
        } else if let Some(seq) = name.strip_prefix(crate::queue::DONE_PREFIX) {
            judge.queue_release(update, seq)
        } else if let Some(claim) = name.strip_prefix(crate::names::INTENT) {
            judge.intent(update, claim)
        } else if let Some(claim) = name.strip_prefix(crate::names::LANDED_INTENT) {
            judge.landed_intent(update, claim, &trunk)
        } else if let Some(machine) = name.strip_prefix(crate::live::LIVE_PREFIX) {
            judge.live(update, machine)
        } else if let Some(executor) = name.strip_prefix(crate::presence::PRESENCE_PREFIX) {
            judge.presence(update, executor)
        } else if let Some(talk) = name.strip_prefix(crate::talk::TALK_PREFIX) {
            judge.talk(update, talk)
        } else if name.starts_with(crate::names::LOCKS) {
            judge.lock(update)
        } else if name.starts_with(crate::names::ARCHIVE_HEADS) {
            archive(update)
        } else {
            Ok(())
        };
        verdict.map_err(|why| format!("{name}: {why}"))?;
    }
    Ok(())
}

/// NEW-5, NEW-9: each update of a `[[target]]` ref (read from the config at trunk's tip) moves
/// it to a promotable commit (`crate::target`): never a deletion; a fast-forward, or a
/// creation, to a commit on its source's first-parent line after this push, whose required
/// checks pass, past its soak and outside a freeze unless its landing carries `Fixes:` or
/// `Reverts:`. The `requires` and `soak_s` of the config at the target's current tip are added,
/// so a promoted commit never weakens the criteria it is judged by. A push of no ref outside
/// bay's namespaces and trunk reads no config.
fn targets(
    git: &Git,
    updates: &[Update],
    trunk: &str,
    registry: Option<&Registry>,
) -> Result<(), String> {
    let other = |update: &&Update| {
        update.name != trunk
            && !update.name.starts_with(crate::names::BAY)
            && !update.name.starts_with(crate::names::NOTES)
    };
    if !updates.iter().any(|update| other(&update)) {
        return Ok(());
    }
    let Some(trunk_tip) = git.ref_target(trunk).map_err(|error| message(&error))? else {
        return Ok(());
    };
    let config = crate::config::Config::at(git, &trunk_tip).map_err(|why| {
        format!(
            "the config at {trunk} cannot be read, so whether this push moves a promotion target is unknown: {why}"
        )
    })?;
    if config.targets.is_empty() {
        return Ok(());
    }
    let after = |name: &str| -> Result<Option<String>, String> {
        if let Some(update) = updates.iter().find(|update| update.name == name) {
            return Ok((!is_zero(&update.new)).then(|| update.new.clone()));
        }
        git.ref_target(name).map_err(|error| message(&error))
    };
    let now = crate::live::unix_now();
    for update in updates.iter().filter(other) {
        let Some(target) = config
            .targets
            .iter()
            .find(|target| target.reference() == update.name)
        else {
            continue;
        };
        let refused = |why: String| format!("{}: {why}", update.name);
        if is_zero(&update.new) {
            return Err(refused(format!(
                "{}: a promotion target is never deleted by a push",
                crate::result::Reason::PromotionRefused.code()
            )));
        }
        let old = (!is_zero(&update.old)).then_some(update.old.as_str());
        let rule = crate::target::rule_at(git, &config, target, old).map_err(|why| {
            refused(format!(
                "the config at the target's tip cannot be read, so its criteria are unknown: {why}"
            ))
        })?;
        let from_tip = after(&rule.from_ref)?;
        let failed = crate::target::judge(
            git,
            &rule,
            from_tip.as_deref(),
            old,
            &update.new,
            registry,
            now,
        )
        .map_err(refused)?;
        if !failed.is_empty() {
            return Err(refused(crate::target::refusal(&rule, &failed)));
        }
    }
    Ok(())
}

/// N-21: a push to a host whose registry signs only approvals (`[policy] signing =
/// "approvals"` at trunk before the push). Only a registry key moves the registry
/// ([`Judge::registry`]), and an evidence notes ref adds only records that are its producer's,
/// every `approval` among them signed by an enrolled approver under that name, and withdraws
/// none ([`Judge::evidence`]). Every other ref is kept as on a host with no registry: trunk by
/// the append-only update hook, promotion targets by [`targets`].
fn approvals_only(git: &Git, registry: &Registry, updates: &[Update]) -> Result<(), String> {
    let judge = Judge {
        git,
        registry,
        updates,
        by_name: updates
            .iter()
            .map(|update| (update.name.as_str(), update))
            .collect(),
        appended: RefCell::new(HashMap::new()),
        talk_creates: Vec::new(),
        queue: std::cell::OnceCell::new(),
    };
    for update in updates {
        let name = update.name.as_str();
        let verdict = if name == REGISTRY_REF {
            judge.registry(update)
        } else if let Some(producer) = name.strip_prefix(evidence::EVIDENCE_PREFIX) {
            judge.evidence(update, producer)
        } else {
            Ok(())
        };
        verdict.map_err(|why| format!("{name}: {why}"))?;
    }
    Ok(())
}

/// NEW-4: a lane change runs its checks and never lands; the executor refuses it too, and this
/// refuses a publication `commit` of one.
fn never_a_lane(record: &crate::change::ChangeRecord, commit: &str) -> Result<(), String> {
    match &record.lane {
        Some(lane) => Err(format!(
            "{commit} carries change {} of lane {lane}; a lane change never merges",
            record.id
        )),
        None => Ok(()),
    }
}

/// Whether `state` ends a change without a landing (G272): abandoned, superseded or no effect.
fn ended_unlanded(state: crate::change::ChangeState) -> bool {
    matches!(
        state,
        crate::change::ChangeState::Abandoned
            | crate::change::ChangeState::Superseded
            | crate::change::ChangeState::NoEffect
    )
}

/// Whether `record`'s change holds the intent claim `claim` (`<hash>/<generation>`).
fn holds_claim(record: &crate::change::ChangeRecord, claim: &str) -> bool {
    record
        .claim
        .as_ref()
        .is_some_and(|held| format!("{}/{}", held.hash, held.generation) == claim)
}

/// The records one shard index update adds: compacted name to the target it records.
type Appended = Rc<HashMap<String, String>>;

struct Judge<'a> {
    git: &'a Git,
    registry: &'a Registry,
    updates: &'a [Update],
    /// The push's updates by ref name.
    by_name: HashMap<&'a str, &'a Update>,
    /// Each shard index update's added records, read once per push.
    appended: RefCell<HashMap<String, Result<Appended, String>>>,
    talk_creates: Vec<crate::talk::Talk>,
    /// The queue as the host holds it before this push, read once for every queue retention
    /// it judges.
    queue: std::cell::OnceCell<Result<crate::queue::Queue, String>>,
}

impl Judge<'_> {
    /// A publication that is not the head's (§ 6, the trust-agents ruling 2026-10-26) stands
    /// only when it releases its change's own place in this push, and nothing ahead can be
    /// affected: its change needs no pre-merge check (`crate::publish::needs_no_check`), or it
    /// names, by `Fixes:`, `Reverts:` or `Supersedes:`, the change that owns a red post check on
    /// the trunk it moves (`crate::red`).
    fn repairs_red(
        &self,
        queue: &crate::queue::Queue,
        trunk: &str,
        change: &str,
    ) -> Result<bool, String> {
        let Some(own) = queue.of_change(change) else {
            return Ok(false);
        };
        let release = format!("{}{}", crate::queue::DONE_PREFIX, own.seq);
        let releases_own = self
            .updates
            .iter()
            .any(|other| other.name == release && is_zero(&other.old));
        if !releases_own || is_zero(trunk) {
            return Ok(false);
        }
        // Nothing to check, nothing to wait: a change that no pre-merge check covers.
        if let Some(record) = self.change_record(change)?
            && let Some(op) = record
                .operations
                .iter()
                .rev()
                .find(|op| !op.state.is_terminal())
            && crate::publish::needs_no_check(self.git, trunk, &op.revision)?
        {
            return Ok(true);
        }
        let Some(record) = self.change_record(change)? else {
            return Ok(false);
        };
        let names: Vec<&String> = [&record.fixes, &record.reverts, &record.supersedes]
            .into_iter()
            .flatten()
            .collect();
        if names.is_empty() {
            return Ok(false);
        }
        let reds = crate::red::red(self.git, trunk, Some(self.registry))?;
        Ok(reds.iter().any(|red| {
            red.change
                .as_ref()
                .is_some_and(|owner| names.contains(&owner))
        }))
    }

    /// The record of `change` as this push leaves its chain, if the host holds one.
    fn change_record(&self, change: &str) -> Result<Option<crate::change::ChangeRecord>, String> {
        let name = format!("{}{change}", crate::change::CHANGES_PREFIX);
        let Some(head) = self.after(&name)? else {
            return Ok(None);
        };
        crate::change::Chain::new(self.git)
            .record_at(&head)
            .map(Some)
            .map_err(|error| error.message())
    }

    /// A ref's value after this push: its new value if the push moves it, else its current one.
    fn after(&self, name: &str) -> Result<Option<String>, String> {
        if let Some(update) = self.by_name.get(name) {
            return Ok((!is_zero(&update.new)).then(|| update.new.clone()));
        }
        self.git.ref_target(name).map_err(|error| message(&error))
    }

    /// What the shard index records for `name` after this push (§ 10a, ruling R11).
    fn recorded(&self, name: &str) -> Result<Option<String>, String> {
        match self.after(&compact::index_ref(name))? {
            Some(index) => compact::recorded_at(self.git, &index, name),
            None => Ok(None),
        }
    }

    /// A marker after this push: its ref, or the shard index record that stands in for it.
    fn marker(&self, name: &str) -> Result<Option<String>, String> {
        match self.after(name)? {
            Some(target) => Ok(Some(target)),
            None => self.recorded(name),
        }
    }

    /// § 10a: create-only holds after compaction. A create of a name the shard index records
    /// is refused, whatever else the push carries.
    fn not_compacted(&self, update: &Update) -> Result<(), String> {
        if !compact::compactable(&update.name) || !is_zero(&update.old) || is_zero(&update.new) {
            return Ok(());
        }
        if self.recorded(&update.name)?.is_some() {
            return Err(format!(
                "is compacted: {} records it, and a compacted name is never created again",
                compact::index_ref(&update.name)
            ));
        }
        Ok(())
    }

    /// A marker is deleted only by its compaction: the same push appends its shard index with
    /// a record of the marker's target. Only a landed change that retention cut gives up its
    /// markers (§ 10a). A claim and a landed claim go only when the landed claim's publication
    /// is the landing of a change that held that claim, so the landed claim still names it.
    fn compacted_delete(&self, update: &Update) -> Result<(), String> {
        let index = compact::index_ref(&update.name);
        let records = match self.by_name.get(index.as_str()) {
            Some(append) if !is_zero(&append.new) => self.appended(append)?,
            _ => Appended::default(),
        };
        if records.get(&update.name) != Some(&update.old) {
            return Err(format!(
                "a marker is never deleted, except by its compaction into {index} in the same push"
            ));
        }
        if let Some(id) = update.name.strip_prefix(crate::names::LANDED) {
            return self.cut_landing(id, None);
        }
        let (claim, publication) =
            if let Some(claim) = update.name.strip_prefix(crate::names::INTENT) {
                let publication = self
                    .marker(&format!("{}{claim}", crate::names::LANDED_INTENT))?
                    .ok_or("a claim is compacted only once its generation merged")?;
                (claim, publication)
            } else if let Some(claim) = update.name.strip_prefix(crate::names::LANDED_INTENT) {
                (claim, update.old.clone())
            } else {
                return Err("is not a compacted marker".to_owned());
            };
        let change = self.trailer(&publication, "Change-Id")?;
        self.cut_landing(&change, Some(claim))
    }

    /// § 10a: change `id` landed and retention cut its chain, so its markers may be compacted.
    /// With `claim` (`<hash>/<generation>`), the change also held that claim.
    fn cut_landing(&self, id: &str, claim: Option<&str>) -> Result<(), String> {
        let refused = || format!("change {id} has not merged with its chain cut by retention");
        let chain = format!("{}{id}", crate::names::CHANGES);
        crate::names::bay_ref(&chain).map_err(|_| refused())?;
        if self
            .marker(&format!("{}{id}", crate::names::LANDED))?
            .is_none()
        {
            return Err(refused());
        }
        let head = self.after(&chain)?.ok_or_else(refused)?;
        let record = Chain::new(self.git)
            .record_at(&head)
            .map_err(|error| error.message())?;
        if !record.retention_cut || record.state != crate::change::ChangeState::Landed {
            return Err(refused());
        }
        if let Some(claim) = claim
            && !holds_claim(&record, claim)
        {
            return Err(format!("change {id} never held the claim {claim}"));
        }
        Ok(())
    }

    /// The records the shard index update `update` adds, read once per push.
    fn appended(&self, update: &Update) -> Result<Appended, String> {
        if let Some(known) = self.appended.borrow().get(&update.name) {
            return known.clone();
        }
        let read = self.read_appended(update);
        self.appended
            .borrow_mut()
            .insert(update.name.clone(), read.clone());
        read
    }

    /// Each record a shard index update adds: a name of that shard in a compacted namespace,
    /// at its one path, at most [`compact::PUSH_MAX`] of them.
    fn read_appended(&self, update: &Update) -> Result<Appended, String> {
        let shard = update
            .name
            .strip_prefix(compact::INDEX_PREFIX)
            .ok_or("is not a shard index")?;
        let added = self.index_additions(update)?;
        if added.len() > compact::PUSH_MAX {
            return Err(format!(
                "a shard index takes at most {} records per push",
                compact::PUSH_MAX
            ));
        }
        let blobs: Vec<String> = added.iter().map(|(_, blob)| blob.clone()).collect();
        let targets = compact::read_blobs(self.git, &blobs)?;
        let mut records = HashMap::new();
        for (path, blob) in &added {
            let name = compact::name_of(path);
            crate::names::bay_ref(&name)?;
            if !compact::compactable(&name)
                || compact::path(&name) != *path
                || compact::shard(&name).to_string() != shard
            {
                return Err(format!("{path} is not a compacted name of shard {shard}"));
            }
            let target = targets
                .get(blob)
                .ok_or_else(|| format!("the record of {name} cannot be read"))?;
            records.insert(name, target.clone());
        }
        Ok(Rc::new(records))
    }

    /// A shard index (ruling R11): never deleted, fast-forward only, executor-signed, and its
    /// new tree only adds records (`read_appended`), each of a ref the same push deletes from
    /// the target it records.
    fn landed_index(&self, update: &Update) -> Result<(), String> {
        if is_zero(&update.new) {
            return Err("a shard index is never deleted".to_owned());
        }
        if !is_zero(&update.old) && !self.git.is_ancestor(&update.old, &update.new)? {
            return Err("a shard index takes only a fast-forward".to_owned());
        }
        if !matches!(
            self.signer(&update.new, &[KeyClass::Runner])?,
            Some((KeyClass::Runner, _))
        ) {
            return Err("a shard index commit is signed by a runner key".to_owned());
        }
        for (name, target) in self.appended(update)?.iter() {
            let deletes = self
                .by_name
                .get(name.as_str())
                .is_some_and(|other| is_zero(&other.new) && other.old == *target);
            if !deletes {
                return Err(format!(
                    "records {name}, which this push does not delete from the target it records"
                ));
            }
        }
        Ok(())
    }

    /// The (path, blob) records a shard index update adds. Any other change to its tree, or an
    /// entry that is not a plain file at the top, is refused.
    fn index_additions(&self, update: &Update) -> Result<Vec<(String, String)>, String> {
        let not_append = || "a shard index only appends records".to_owned();
        let mut added = Vec::new();
        if is_zero(&update.old) {
            let listed = self
                .git
                .run(&["ls-tree", &update.new])
                .map_err(|error| message(&error))?;
            for line in listed.lines() {
                let (entry, path) = line.split_once('\t').ok_or_else(not_append)?;
                let fields: Vec<&str> = entry.split_whitespace().collect();
                let ["100644", "blob", blob] = fields.as_slice() else {
                    return Err(not_append());
                };
                added.push((path.to_owned(), (*blob).to_owned()));
            }
            return Ok(added);
        }
        let changed = self
            .git
            .run(&[
                "diff-tree",
                "-r",
                "--no-commit-id",
                "--no-renames",
                "--raw",
                "--full-index",
                &update.old,
                &update.new,
            ])
            .map_err(|error| message(&error))?;
        for line in changed.lines() {
            let (entry, path) = line.split_once('\t').ok_or_else(not_append)?;
            let fields: Vec<&str> = entry.split_whitespace().collect();
            let [_, "100644", _, blob, "A"] = fields.as_slice() else {
                return Err(not_append());
            };
            if path.contains('/') {
                return Err(not_append());
            }
            added.push((path.to_owned(), (*blob).to_owned()));
        }
        Ok(added)
    }

    /// The class and principal a commit verifies as, trying `classes` in order.
    fn signer(
        &self,
        commit: &str,
        classes: &[KeyClass],
    ) -> Result<Option<(KeyClass, String)>, String> {
        for class in classes {
            let file = self.registry.class_file(self.git, *class)?;
            if let Some(principal) =
                signing::verify_commit(self.git, commit, &file).map_err(|error| message(&error))?
            {
                return Ok(Some((*class, principal)));
            }
        }
        Ok(None)
    }

    fn trunk(&self, update: &Update) -> Result<(), String> {
        if is_zero(&update.new) {
            return Ok(()); // the append-only update hook refuses it
        }
        let range = if is_zero(&update.old) {
            update.new.clone()
        } else {
            format!("{}..{}", update.old, update.new)
        };
        let listed = self
            .git
            .run(&["rev-list", "--reverse", &range])
            .map_err(|error| message(&error))?;
        let commits: Vec<&str> = listed.lines().collect();
        // `crate::throughput`: a batch lands whole, its members in order, in this one push.
        let tags = commits
            .iter()
            .map(|commit| self.trailer(commit, throughput::BATCH_TRAILER))
            .collect::<Result<Vec<String>, String>>()?;
        let tagged: Vec<Option<&str>> = tags
            .iter()
            .map(|tag| Some(tag.as_str()).filter(|tag| !tag.is_empty()))
            .collect();
        let roles = throughput::roles(&tagged)?;
        // § 6: while a reservation is live only the head publishes, releasing itself in the
        // same transaction. A batch the head leads lands its members with it.
        let queue = crate::queue::Queue::local(self.git)?;
        if let Some(head) = queue.head() {
            let release = format!("{}{}", crate::queue::DONE_PREFIX, head.seq);
            let releases_head = self
                .updates
                .iter()
                .any(|other| other.name == release && is_zero(&other.old));
            let leads = match roles.last() {
                Some(throughput::Role::Tip { first }) => commits.get(*first).copied(),
                _ => Some(update.new.as_str()),
            };
            let change = match leads {
                Some(commit) => self.trailer(commit, "Change-Id")?,
                None => String::new(),
            };
            let heads_it = releases_head && head.change == change;
            if !heads_it && !self.repairs_red(&queue, &update.old, &change)? {
                return Err(format!(
                    "QUEUED: {QUEUE_HELD_BY} {} (place {})",
                    head.op_id, head.seq
                ));
            }
        }
        for (commit, role) in commits.iter().zip(&roles) {
            let judged = match role {
                throughput::Role::Alone => Judged::Parent,
                throughput::Role::Member => Judged::InBatch,
                throughput::Role::Tip { first } => {
                    let base = self
                        .git
                        .run(&["rev-parse", &format!("{}^1", commits[*first])])
                        .map_err(|error| message(&error))?;
                    if crate::config::Config::at(self.git, &base)?
                        .batch()
                        .is_none()
                    {
                        return Err(format!(
                            "{commit} merges a batch, and the config at its base {base} sets no [runner] batch"
                        ));
                    }
                    Judged::BatchBase(base)
                }
            };
            self.publication(commit, &judged)?;
        }
        Ok(())
    }

    fn trailer(&self, commit: &str, key: &str) -> Result<String, String> {
        Ok(self
            .git
            .run(&[
                "log",
                "-1",
                &format!("--format=%(trailers:key={key},valueonly,separator=%x2c)"),
                commit,
            ])
            .map_err(|error| message(&error))?
            .trim()
            .to_owned())
    }

    /// The landed marker of `change` is created by this very push, at `commit`, and did not
    /// exist before it.
    fn creates_marker(&self, commit: &str, change: &str) -> Result<(), String> {
        let marker = format!("{}{change}", crate::names::LANDED);
        let creates = self
            .updates
            .iter()
            .any(|update| update.name == marker && is_zero(&update.old) && update.new == commit);
        if !creates {
            return Err(format!("{commit} must create {marker} in the same push"));
        }
        if self
            .git
            .ref_target(&marker)
            .map_err(|error| message(&error))?
            .is_some()
            || self.recorded(&marker)?.is_some()
        {
            return Err(format!("change {change} already merged"));
        }
        Ok(())
    }

    /// One new trunk commit: an executor-signed publication of a verified, current request,
    /// with passing producer-signed evidence, judged as `judged` says, and its landed marker
    /// created in this push.
    fn publication(&self, commit: &str, judged: &Judged) -> Result<(), String> {
        let change = self.trailer(commit, "Change-Id")?;
        let revision = self.trailer(commit, "Revision-Id")?;
        let registry_id = self.trailer(commit, "Registry-Id")?;
        let request_id = self.trailer(commit, "Request-Id")?;
        let committer = self
            .git
            .run(&["log", "-1", "--format=%cn", commit])
            .map_err(|error| message(&error))?;
        let not_a_publication = || {
            format!(
                "{commit} is not a merge by bay: main takes only commits signed by a runner key"
            )
        };
        if change.is_empty() {
            return Err(not_a_publication());
        }
        // § 10a: the `Registry-Id` is on the registry chain, and the signing key is valid in the
        // current registry, so naming an older registry version cannot revive a revoked key. A
        // key valid under the version C names but not under the current one was revoked after C
        // was signed: that is `REGISTRY_CHANGED`, answered only once every other rule holds.
        let named = self.registry.version_on_chain(self.git, &registry_id)?;
        let key_revoked =
            if crate::publish::verifies_under(self.git, commit, self.registry, &committer) {
                false
            } else if named.as_ref().is_some_and(|named| {
                crate::publish::verifies_under(self.git, commit, named, &committer)
            }) {
                true
            } else {
                return Err(not_a_publication());
            };
        let Some(named) = named else {
            return Err(format!(
                "{commit} names Registry-Id {registry_id}, which is not on the registry chain {REGISTRY_REF}"
            ));
        };
        let request = self.request(&request_id, &change)?;
        if request.revision != revision {
            return Err(format!(
                "{commit} merges {revision}, but its request names {}",
                request.revision
            ));
        }
        // No newer request of the change names another revision (§ 10a).
        let latest = self
            .after(&format!("{}{change}", crate::names::REQUESTS))?
            .ok_or_else(|| format!("change {change} has no requests ref"))?;
        let latest = self.request(&latest, &change)?;
        if latest.revision != revision {
            return Err(format!(
                "{commit} merges a revision a newer request superseded"
            ));
        }
        // The operation is not cancelled, and the change is of no lane.
        // G292: the change's candidates, whose approvals may hold for this one.
        let mut earlier = Vec::new();
        if let Some(head) = self.after(&format!("{}{change}", crate::names::CHANGES))? {
            let record = Chain::new(self.git)
                .record_at(&head)
                .map_err(|error| error.message())?;
            never_a_lane(&record, commit)?;
            earlier = crate::throughput::candidates_of(&record);
            if record
                .operations
                .iter()
                .any(|op| op.revision == revision && op.state == OpState::Superseded)
                && !record
                    .operations
                    .iter()
                    .any(|op| op.revision == revision && op.state != OpState::Superseded)
            {
                return Err(format!("{commit} merges a cancelled operation"));
            }
        }
        self.creates_marker(commit, &change)?;
        // `crate::throughput`: a policy repair is judged by its own policy, never in a batch.
        if request.repair && *judged != Judged::Parent {
            return Err(format!(
                "{commit} is a policy repair, which never merges in a batch"
            ));
        }
        // § 10a, G33: evidence counts only under the current registry. Evidence that passes
        // under the version C names but not under the current one rests on a producer key the
        // registry revoked since: that is `REGISTRY_CHANGED` too, and the executor rechecks.
        let evidence =
            self.judged_evidence(commit, request.repair, self.registry, judged, &earlier);
        let producer_revoked = evidence.is_err()
            && named.commit != self.registry.commit
            && self
                .judged_evidence(commit, request.repair, &named, judged, &earlier)
                .is_ok();
        // Refused for a reason of its own, or accepted: never `REGISTRY_CHANGED`.
        if !producer_revoked && (evidence.is_err() || !key_revoked) {
            return evidence;
        }
        let revoked = match (key_revoked, producer_revoked) {
            (true, true) => "its runner key and a producer key behind its evidence are",
            (true, false) => "its runner key is",
            _ => "a producer key behind its evidence is",
        };
        Err(format!(
            "REGISTRY_CHANGED: {commit} was signed under registry {registry_id}, and {revoked} not valid in the current registry {}; a runner with a valid key re-signs it",
            self.registry.commit
        ))
    }

    /// [`Judge::evidence_passes`] as `judged` says (`crate::throughput`): a batch member before
    /// the last stands on the batch tip's evidence, which the push holds; the tip is judged at
    /// the batch's base. G292: a commit judged at its parent may stand on an approval of one of
    /// its change's `earlier` candidates that still holds; a batch tip lands several changes,
    /// so none carries onto it.
    fn judged_evidence(
        &self,
        commit: &str,
        repair: bool,
        registry: &Registry,
        judged: &Judged,
        earlier: &[crate::throughput::Earlier],
    ) -> Result<(), String> {
        match judged {
            Judged::Parent => self.evidence_passes(commit, repair, registry, (None, earlier)),
            Judged::InBatch => Ok(()),
            Judged::BatchBase(base) => {
                let none: &[crate::throughput::Earlier] = &[];
                self.evidence_passes(commit, repair, registry, (Some(base.as_str()), none))
            }
        }
    }

    /// The producer-signed evidence on the commit's tree, counted under `registry`, passes the
    /// policy at its parent (or, for a registry-signed repair, the candidate's own policy), for
    /// the checks its diff against its parent requires. With `base`, a batch tip's, it is judged
    /// at the batch's base instead, over the batch's whole diff (`crate::throughput`). G292: an
    /// approval of one of the change's `earlier` candidates that still holds answers here too,
    /// as the executor judged it (`crate::throughput::carried_approvals`).
    fn evidence_passes(
        &self,
        commit: &str,
        repair: bool,
        registry: &Registry,
        (base, earlier): (Option<&str>, &[crate::throughput::Earlier]),
    ) -> Result<(), String> {
        let parent = base.map_or_else(|| format!("{commit}^1"), str::to_owned);
        let judged_by = if repair {
            commit.to_owned()
        } else {
            parent.clone()
        };
        // The missing-policy ruling: no policy where the publication is judged is unknown
        // criteria, never a pass. An unchecked policy has an empty manifest, so it needs no
        // evidence.
        let Some(policy) = policy::load_at(self.git, &judged_by)? else {
            return Err(format!(
                "POLICY_MISSING: {commit} is judged at {judged_by}, which has no {}; add a policy there, or declare unchecked = true in it",
                policy::POLICY_PATH
            ));
        };
        let tree = self
            .git
            .run(&["rev-parse", &format!("{commit}^{{tree}}")])
            .map_err(|error| message(&error))?;
        // G163: a record from a producer the check's list leaves out never answers it.
        let mut records = policy.answering(evidence::counted(
            self.git,
            evidence::records_for(self.git, &tree)?,
            Some(registry),
        )?);
        // G292: an approval holds while the files it covers are unchanged.
        if !earlier.is_empty() {
            let base_tree = self
                .git
                .run(&["rev-parse", &format!("{parent}^{{tree}}")])
                .map_err(|error| message(&error))?;
            let carried = crate::throughput::carried_approvals(
                self.git,
                &policy,
                Some(registry),
                (tree.as_str(), base_tree.as_str()),
                &records,
                earlier,
            )?;
            records.extend(carried.records);
        }
        // G146: the checks the commit's own diff against its parent requires, as land judged.
        let changed = crate::check::changed_paths(self.git, &parent, commit)?;
        let (required, _) = policy.required(&changed);
        // The executor's rule: a flaky check whose entry says `allow_flaky` is no gap.
        let gaps = policy.gaps(&records, &required, commit);
        if gaps.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "{commit} has no passing producer-signed evidence for {}",
                gaps.iter()
                    .map(|(id, _)| id.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
    }

    /// A request commit of `change`, signed by its actor's agent key under the registry version
    /// it names (so a later revocation does not strand it), or by a registry key (a repair).
    /// That version must be on the registry chain (§ 10a): a request naming a registry commit
    /// off the chain, which anyone can write, is never judged under it.
    fn request(&self, request_id: &str, change: &str) -> Result<Request, String> {
        let text = self
            .git
            .file_at(request_id, "request.json")
            .map_err(|error| message(&error))?
            .ok_or_else(|| format!("{request_id} is not a request"))?;
        let request: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        if request["change"] != change {
            return Err(format!("request {request_id} is for another change"));
        }
        let named = request["registry"].as_str().unwrap_or_default();
        let registry = self
            .registry
            .version_on_chain(self.git, named)?
            .ok_or_else(|| {
                format!(
                    "request {request_id} names registry {named:?}, which is not on the registry chain {REGISTRY_REF}"
                )
            })?;
        let agents = registry.class_file(self.git, KeyClass::Agent)?;
        let actor = request["actor"]["name"].as_str().unwrap_or_default();
        if signing::verify_commit(self.git, request_id, &agents)
            .map_err(|error| message(&error))?
            .as_deref()
            == Some(actor)
        {
            return Ok(Request {
                revision: request["revision"].as_str().unwrap_or_default().to_owned(),
                repair: false,
            });
        }
        let registrars = registry.class_file(self.git, KeyClass::Registry)?;
        if signing::verify_commit(self.git, request_id, &registrars)
            .map_err(|error| message(&error))?
            .is_some()
        {
            return Ok(Request {
                revision: request["revision"].as_str().unwrap_or_default().to_owned(),
                repair: true,
            });
        }
        Err(format!(
            "request {request_id} is not signed by its actor {actor}'s agent key"
        ))
    }

    fn landed(&self, update: &Update, id: &str, trunk: &str) -> Result<(), String> {
        if !is_zero(&update.old) {
            return Err("a merge marker is never moved or deleted".to_owned());
        }
        let mut with_trunk = false;
        for other in self.updates.iter().filter(|other| other.name == trunk) {
            if self.git.is_ancestor(&update.new, &other.new)? {
                with_trunk = true;
                break;
            }
        }
        if !with_trunk {
            return Err(format!(
                "is created only together with the update of main that merges {id}"
            ));
        }
        Ok(())
    }

    /// The state commits a chain update adds, newest first: the first-parent walk from the new
    /// head while commits carry a change record.
    fn state_commits(&self, update: &Update) -> Vec<String> {
        let mut out = Vec::new();
        let mut commit = update.new.clone();
        loop {
            if commit == update.old {
                break;
            }
            let has_record = self
                .git
                .run(&["cat-file", "-e", &format!("{commit}:{RECORD_FILE}")])
                .is_ok();
            if !has_record {
                break;
            }
            out.push(commit.clone());
            match self
                .git
                .run(&["rev-parse", "--verify", "-q", &format!("{commit}^1")])
            {
                Ok(parent) => commit = parent,
                Err(_) => break,
            }
        }
        out
    }

    fn chain(&self, update: &Update, id: &str) -> Result<(), String> {
        if is_zero(&update.new) {
            return Err("a change chain is never deleted".to_owned());
        }
        let chain = Chain::new(self.git);
        let fast_forward =
            is_zero(&update.old) || self.git.is_ancestor(&update.old, &update.new)?;
        if !fast_forward {
            return self.retention_cut(update, id);
        }
        for commit in self.state_commits(update) {
            let record = chain.record_at(&commit).map_err(|error| error.message())?;
            if record.id != id {
                return Err(format!("{commit} carries the record of another change"));
            }
            // An adoption (the actor changes) is accepted only while the change has not landed,
            // and, for an open change, only past its takeover window.
            let parent = self
                .git
                .run(&["rev-parse", "--verify", "-q", &format!("{commit}^1")])
                .ok();
            let previous = parent
                .as_deref()
                .and_then(|parent| chain.record_at(parent).ok());
            // NEW-4: the lane a change was started in is never changed, so no append can turn a
            // lane change into one that lands.
            if let Some(previous) = &previous
                && previous.lane != record.lane
            {
                return Err(format!("{commit} changes the lane of change {id}"));
            }
            // G272: a change that ended without landing stays closed once a newer generation
            // of its intent is claimed; that newer change carries the intent on.
            if let (Some(before), Some(claim)) = (&previous, &record.claim)
                && ended_unlanded(before.state)
                && !ended_unlanded(record.state)
                && self.claim_exists(&claim.hash, claim.generation + 1)?
            {
                return Err(format!(
                    "{}: {commit} re-opens change {id} of generation {}, and generation {} of \
                     its intent is claimed",
                    crate::result::Reason::ChangeSuperseded.code(),
                    claim.generation,
                    claim.generation + 1
                ));
            }
            let before = previous.filter(|before| before.actor.name != record.actor.name);
            if let Some(before) = &before {
                if self
                    .marker(&format!("{}{id}", crate::names::LANDED))?
                    .is_some()
                {
                    return Err(format!("{commit} adopts a change that already merged"));
                }
                if !matches!(
                    before.state,
                    crate::change::ChangeState::Abandoned
                        | crate::change::ChangeState::Superseded
                        | crate::change::ChangeState::NoEffect
                ) {
                    self.takeover_window(update, id, parent.as_deref(), &before.actor.name)?;
                }
            }
            if record.state == crate::change::ChangeState::Abandoned
                && self
                    .marker(&format!("{}{id}", crate::names::LANDED))?
                    .is_some()
            {
                return Err(format!("{commit} abandons a change that already merged"));
            }
            match self.signer(
                &commit,
                &[KeyClass::Agent, KeyClass::Runner, KeyClass::Registry],
            )? {
                Some((KeyClass::Agent, principal)) if principal != record.actor.name => {
                    let code = if record.state == crate::change::ChangeState::Abandoned {
                        "ABANDON_REFUSED: "
                    } else {
                        ""
                    };
                    return Err(format!(
                        "{code}{commit} is signed by agent {principal}, not by the change's actor {}",
                        record.actor.name
                    ));
                }
                Some(_) => {}
                None => {
                    return Err(format!(
                        "{commit} is not signed by an agent, runner or registry key"
                    ));
                }
            }
        }
        Ok(())
    }

    /// § 4: another actor adopts an open change only when its chain head has not moved for the
    /// takeover window, and no heartbeat of its current actor naming it is newer than that
    /// window. Both ages are read from this host's reflog, on this host's clock.
    ///
    /// The work is bounded (§ 10a): the chain's reflog gives one entry; each live ref gives at
    /// most [`LIVE_REFLOG_MAX`] entries, their summaries are read by one batch, and only its
    /// newest entry with a fresh row of the change by that actor is verified (a live ref is
    /// bound to one key, so an older entry is signed by the same key or by one no longer
    /// enrolled). A host holding more than [`LIVE_REFS_MAX`] live refs refuses the adopt.
    fn takeover_window(
        &self,
        update: &Update,
        id: &str,
        adopted_head: Option<&str>,
        actor: &str,
    ) -> Result<(), String> {
        let config = crate::config::Config::load_or_committed(self.git.dir(), self.git)?;
        let window = config.takeover().as_secs();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let refuse = |why: String| {
            Err(format!(
                "CHANGE_SUPERSEDED: change {id} is held by {actor}: {why}; the takeover window is {window} s"
            ))
        };
        if is_zero(&update.old) || adopted_head != Some(update.old.as_str()) {
            return refuse("the adopted head is not the head this host holds".to_owned());
        }
        let name = format!("{}{id}", crate::names::CHANGES);
        let Some((tip, set_at)) = self.reflog(&name, 1)?.into_iter().next() else {
            return refuse(format!("this host has no reflog of {name}"));
        };
        if tip != update.old {
            return refuse(format!(
                "this host's reflog of {name} does not end at its head"
            ));
        }
        let age = now.saturating_sub(set_at);
        if age < window {
            return refuse(format!("its chain head moved {age} s ago"));
        }
        let lives = crate::live::tips(self.git)?;
        if lives.len() > LIVE_REFS_MAX {
            return refuse(format!(
                "this host holds {} live refs, more than the {LIVE_REFS_MAX} it reads",
                lives.len()
            ));
        }
        let agents = self.registry.class_file(self.git, KeyClass::Agent)?;
        // Only a live ref whose tip this hook accepted from the actor holds its heartbeats: the
        // ref is bound to one key, and an older entry by a key since revoked no longer verifies.
        // So the reflogs read are those of the actor's own refs, read from one batch of tips.
        let (names, tips): (Vec<String>, Vec<String>) = lives.into_iter().unzip();
        let own: Vec<&String> = names
            .iter()
            .zip(strict(crate::live::summaries_at(self.git, &tips)?)?)
            .filter(|(_, summary)| {
                summary
                    .as_ref()
                    .is_some_and(|summary| summary["signer"] == actor)
            })
            .map(|(name, _)| name)
            .collect();
        for live in own {
            let entries: Vec<(String, u64)> = self
                .reflog(live, LIVE_REFLOG_MAX)?
                .into_iter()
                .take_while(|(_, at)| now.saturating_sub(*at) < window)
                .collect();
            let commits: Vec<String> = entries.iter().map(|(commit, _)| commit.clone()).collect();
            let summaries = strict(crate::live::summaries_at(self.git, &commits)?)?;
            let newest = entries.iter().zip(&summaries).find(|(_, summary)| {
                summary
                    .as_ref()
                    .is_some_and(|summary| crate::live::names_change(summary, id, actor, window))
            });
            if let Some(((commit, at), _)) = newest
                && crate::live::heartbeat_of(self.git, commit, id, actor, Some(&agents), window)?
            {
                return refuse(format!(
                    "its heartbeat on {live} is {} s old",
                    now.saturating_sub(*at)
                ));
            }
        }
        Ok(())
    }

    /// The newest `limit` entries of the host reflog of `name`, newest first: each entry's new
    /// value and the Unix time it was set.
    fn reflog(&self, name: &str, limit: usize) -> Result<Vec<(String, u64)>, String> {
        let limit = format!("-n{limit}");
        let (code, out, _) = self
            .git
            .run_status(&[
                "log",
                "-g",
                &limit,
                "--date=unix",
                "--format=%H %gd",
                name,
                "--",
            ])
            .map_err(|error| message(&error))?;
        if code != 0 {
            return Ok(Vec::new());
        }
        Ok(out
            .lines()
            .filter_map(|line| {
                let (commit, selector) = line.split_once(' ')?;
                let at = selector
                    .rsplit_once("@{")?
                    .1
                    .strip_suffix('}')?
                    .parse()
                    .ok()?;
                Some((commit.to_owned(), at))
            })
            .collect())
    }

    /// A lock ref (BACKLOG 57, `crate::locks`): its tip is a `bay.lock/1` commit of the path the
    /// ref is named for, signed by the agent key enrolled for the actor it names, so no one
    /// writes a lock as another agent. A deletion is not judged: it frees the path, and a
    /// change whose lock is gone holds nothing at merge.
    fn lock(&self, update: &Update) -> Result<(), String> {
        if is_zero(&update.new) {
            return Ok(());
        }
        let lock = crate::locks::signed_lock(self.git, Some(self.registry), &update.new)
            .ok_or_else(|| {
                format!(
                    "{} is not a {} commit signed by the agent key of the actor it names",
                    update.new,
                    crate::locks::SCHEMA
                )
            })?;
        if crate::locks::lock_ref(&lock.path) != update.name {
            return Err(format!(
                "{} is the lock of another path ({})",
                update.new, lock.path
            ));
        }
        Ok(())
    }

    /// A live summary ref (§ 10a, ruling R1): never deleted; one parentless, agent-signed
    /// commit whose tree is `live.json` alone, at most [`crate::live::LIVE_MAX_BYTES`], a
    /// `bay.live/1` summary of this machine naming the fingerprint of the key that signed it.
    /// The ref is bound to its first key: an update is accepted only from the key of the tip
    /// it replaces (or, when that tip no longer verifies because its key left the registry,
    /// from any agent key). One key holds one live ref: a create by a key that another live
    /// ref's summary names is refused, so no agent claims a second machine's name.
    fn live(&self, update: &Update, machine: &str) -> Result<(), String> {
        if is_zero(&update.new) {
            return Err("a live ref is never deleted".to_owned());
        }
        let file = self.registry.class_file(self.git, KeyClass::Agent)?;
        let Some((signer, key)) = signing::verify_commit_key(self.git, &update.new, &file)
            .map_err(|error| message(&error))?
        else {
            return Err(format!("{} is not signed by an agent key", update.new));
        };
        let parents = self
            .git
            .run(&["show", "-s", "--format=%P", &update.new])
            .map_err(|error| message(&error))?;
        if !parents.trim().is_empty() {
            return Err(format!("{} is not a parentless live commit", update.new));
        }
        let tree = self
            .git
            .run(&["ls-tree", "-l", &update.new])
            .map_err(|error| message(&error))?;
        if !live_tree_is_small(&tree) {
            return Err(format!(
                "{} must hold {} alone, at most {} bytes",
                update.new,
                crate::live::LIVE_FILE,
                crate::live::LIVE_MAX_BYTES
            ));
        }
        let summary = crate::live::summary_at(self.git, &update.new)?
            .filter(|summary| summary["schema"] == crate::live::LIVE_SCHEMA)
            .ok_or_else(|| format!("{} holds no {}", update.new, crate::live::LIVE_SCHEMA))?;
        if summary["machine"] != machine {
            return Err(format!("{} summarises another machine", update.new));
        }
        if summary["key"] != key.as_str() || summary["signer"] != signer.as_str() {
            return Err(format!(
                "{} does not name the key and agent that signed it ({key}, {signer})",
                update.new
            ));
        }
        if is_zero(&update.old) {
            let lives = crate::live::tips(self.git)?;
            if lives.len() >= LIVE_REFS_MAX {
                return Err(format!("this host already holds {LIVE_REFS_MAX} live refs"));
            }
            let (names, tips): (Vec<String>, Vec<String>) = lives
                .into_iter()
                .filter(|(name, _)| *name != update.name)
                .unzip();
            let summaries = strict(crate::live::summaries_at(self.git, &tips)?)?;
            if let Some((other, _)) = names.iter().zip(&summaries).find(|(_, summary)| {
                summary
                    .as_ref()
                    .is_some_and(|summary| summary["key"] == key.as_str())
            }) {
                return Err(format!(
                    "agent {signer}'s key already holds {other}; one key writes one machine's live ref"
                ));
            }
        } else if let Some((bound, bound_key)) =
            signing::verify_commit_key(self.git, &update.old, &file)
                .map_err(|error| message(&error))?
            && bound_key != key
        {
            return Err(format!(
                "is bound to agent {bound}; agent {signer} may update only its own machine's live ref"
            ));
        }
        Ok(())
    }

    /// G67: an executor's presence ref (`crate::presence`): never deleted; one parentless
    /// commit whose tree is `presence.json` alone, at most
    /// [`crate::presence::PRESENCE_MAX_BYTES`], a `bay.executor/1` record naming the executor
    /// the ref is named for and signed by that executor's key, so no executor speaks for another.
    fn presence(&self, update: &Update, executor: &str) -> Result<(), String> {
        if is_zero(&update.new) {
            return Err("a runner presence ref is never deleted".to_owned());
        }
        match self.signer(&update.new, &[KeyClass::Runner])? {
            Some((_, principal)) if principal == executor => {}
            _ => {
                return Err(format!("{} is not signed by runner {executor}", update.new));
            }
        }
        let parents = self
            .git
            .run(&["show", "-s", "--format=%P", &update.new])
            .map_err(|error| message(&error))?;
        if !parents.trim().is_empty() {
            return Err(format!(
                "{} is not a parentless presence commit",
                update.new
            ));
        }
        let tree = self
            .git
            .run(&["ls-tree", "-l", &update.new])
            .map_err(|error| message(&error))?;
        if !one_small_file(
            &tree,
            crate::presence::PRESENCE_FILE,
            crate::presence::PRESENCE_MAX_BYTES,
        ) {
            return Err(format!(
                "{} must hold {} alone, at most {} bytes",
                update.new,
                crate::presence::PRESENCE_FILE,
                crate::presence::PRESENCE_MAX_BYTES
            ));
        }
        let record = crate::presence::record_at(self.git, &update.new)?
            .filter(|record| record["schema"] == crate::presence::PRESENCE_SCHEMA)
            .ok_or_else(|| {
                format!(
                    "{} holds no {}",
                    update.new,
                    crate::presence::PRESENCE_SCHEMA
                )
            })?;
        if record["executor"] != executor {
            return Err(format!("{} names another runner", update.new));
        }
        Ok(())
    }

    /// A talk chain (`crate::talk::judge`).
    fn talk(&self, update: &Update, talk: &str) -> Result<(), String> {
        let (who, id) = talk
            .split_once('/')
            .ok_or_else(|| format!("a talk ref is {}<who>/<id>", crate::names::TALK))?;
        let after = |name: &str| self.after(name);
        let marker = |name: &str| self.marker(name);
        let push = crate::talk::Push {
            git: self.git,
            registry: self.registry,
            after: &after,
            marker: &marker,
            creates: &self.talk_creates,
        };
        let old = (!is_zero(&update.old)).then_some(update.old.as_str());
        let new = (!is_zero(&update.new)).then_some(update.new.as_str());
        crate::talk::judge(&push, old, new, who, id)
    }

    /// The one non-fast-forward update of a chain: an executor-signed cut of a landed change
    /// that keeps the latest land request (§ 5, § 10a).
    fn retention_cut(&self, update: &Update, id: &str) -> Result<(), String> {
        let record = Chain::new(self.git)
            .record_at(&update.new)
            .map_err(|error| error.message())?;
        if !record.retention_cut {
            return Err("a chain only appends; its one rewrite is the retention cut".to_owned());
        }
        if !matches!(
            self.signer(&update.new, &[KeyClass::Runner])?,
            Some((KeyClass::Runner, _))
        ) {
            return Err("a retention cut is signed by a runner key".to_owned());
        }
        // § 10a: only a landed change, or one whose chain (before the cut) is abandoned.
        let landed = self
            .marker(&format!("{}{id}", crate::names::LANDED))?
            .is_some();
        let abandoned = !landed
            && Chain::new(self.git)
                .record_at(&update.old)
                .is_ok_and(|before| before.state == crate::change::ChangeState::Abandoned);
        if !landed && !abandoned {
            return Err("a retention cut of an open change is refused".to_owned());
        }
        let parents = self
            .git
            .run(&["rev-list", "--parents", "-n", "1", &update.new])
            .map_err(|error| message(&error))?;
        let latest = self.after(&format!("{}{id}", crate::names::REQUESTS))?;
        // A landed change always has a land request; an abandoned one may never have had one.
        let keeps_latest = latest.as_deref().is_some_and(|latest| {
            parents
                .split_whitespace()
                .skip(1)
                .any(|parent| parent == latest)
        });
        if !keeps_latest && (landed || latest.is_some()) {
            return Err("a retention cut keeps the latest merge request".to_owned());
        }
        Ok(())
    }

    fn request_ref(&self, update: &Update, id: &str) -> Result<(), String> {
        if is_zero(&update.new) {
            return Err("a requests ref is never deleted".to_owned());
        }
        let text = self
            .git
            .file_at(&update.new, "request.json")
            .map_err(|error| message(&error))?
            .ok_or_else(|| format!("{} is not a request", update.new))?;
        let request: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        if request["change"] != id {
            return Err("names a request of another change".to_owned());
        }
        // Judged under the registry version it names, which must already be on the registry
        // chain (§ 10a), as a publication of it will be judged ...
        self.request(&update.new, id)?;
        // ... and against the current registry: a request signed by a key already revoked is
        // refused at its push.
        let actor = request["actor"]["name"].as_str().unwrap_or_default();
        match self.signer(&update.new, &[KeyClass::Agent, KeyClass::Registry])? {
            Some((KeyClass::Agent, principal)) if principal == actor => Ok(()),
            Some((KeyClass::Registry, _)) => Ok(()),
            _ => Err(format!(
                "{} is not signed by its actor {actor}'s agent key",
                update.new
            )),
        }
    }

    /// A producer's or a reviewer's notes ref: every record it adds is signed by that name, with
    /// a key of the class the record's kind needs (Q12; under `signing = "approvals"` only an
    /// `approval` record, N-21), and every record it held stays (G159, [`kept`]).
    fn evidence(&self, update: &Update, producer: &str) -> Result<(), String> {
        if is_zero(&update.new) {
            return Err("evidence is never deleted".to_owned());
        }
        let changed = if is_zero(&update.old) {
            self.git.run(&["ls-tree", "-r", "--name-only", &update.new])
        } else {
            self.git.run(&[
                "diff-tree",
                "-r",
                "--name-only",
                "--no-commit-id",
                &update.old,
                &update.new,
            ])
        }
        .map_err(|error| message(&error))?;
        for path in changed.lines() {
            // A note's path is the object it annotates, in fan-out form: a record sits only on
            // the note of the tree it is about, never copied onto another's.
            let annotated: String = path.chars().filter(|c| *c != '/').collect();
            let old_text = if is_zero(&update.old) {
                None
            } else {
                self.note(&update.old, &annotated)?
            };
            let before: Vec<evidence::EvidenceRecord> = old_text
                .as_deref()
                .and_then(|text| serde_json::from_str(text).ok())
                .unwrap_or_default();
            let new_text = self.note(&update.new, &annotated)?;
            let Some(text) = new_text else {
                if let Some(old_text) = &old_text {
                    kept(old_text, None, &annotated)?;
                }
                return Err(format!("the note on {annotated} is removed"));
            };
            let records: Vec<evidence::EvidenceRecord> =
                serde_json::from_str(&text).map_err(|error| format!("{path}: {error}"))?;
            let mut ids = HashSet::new();
            for record in &records {
                if !ids.insert(record.id.clone()) {
                    return Err(format!("two records under one evidence id {}", record.id));
                }
                if before.contains(record) {
                    continue;
                }
                if record.producer.name != producer
                    || !record.id.starts_with(&format!("{producer}/"))
                {
                    return Err(format!("record {} is not producer {producer}'s", record.id));
                }
                if record.subject.tree != annotated {
                    return Err(format!(
                        "record {} is about tree {}, not the {annotated} its note is on",
                        record.id, record.subject.tree
                    ));
                }
                // Q12, G245: the record's kind names the one class file it verifies against, so
                // a reviewer key writes only `review` records, an approver key only `approval`
                // and `waiver` records (N-9), and a producer key none of them. N-21: under
                // `signing = "approvals"` only an approval or a waiver is signed; every other
                // record is taken unsigned.
                if evidence::signed_kind(self.registry, &record.kind)
                    && !record.verifies_in(self.git, self.registry)?
                {
                    let class = evidence::signer_class(&record.kind);
                    let file = class.file();
                    return Err(format!(
                        "record {} of kind {} is not signed by {file} {producer}'s key; a {file} key signs {}",
                        record.id,
                        record.kind,
                        match class {
                            KeyClass::Reviewer => "only review records",
                            KeyClass::Approver => "only approval and waiver records",
                            _ => "every kind but review, approval and waiver",
                        }
                    ));
                }
            }
            // G159: every record of the old note stays in the new one, byte for byte. Judged
            // after what the note adds, so a forged addition is named as such first.
            if let Some(old_text) = &old_text {
                kept(old_text, Some(&text), &annotated)?;
            }
        }
        Ok(())
    }

    /// The note on `object` in notes commit `notes`, or `None` when it has none. Git moves a
    /// note to a fan-out path (`ab/cdef…`) as a notes tree grows, so a note that only moved is
    /// found at its new path and is never read as removed. G104b: a note the tree names whose
    /// blob cannot be read is an error, never a note absent here, so no record of it is lost.
    fn note(&self, notes: &str, object: &str) -> Result<Option<String>, String> {
        let mut path = object.to_owned();
        for depth in 0..=NOTE_FANOUT_MAX {
            if let Some(text) = self
                .git
                .file_at(notes, &path)
                .map_err(|error| message(&error))?
            {
                return Ok(Some(text));
            }
            let split = 3 * depth + 2;
            if split >= path.len() {
                break;
            }
            path.insert(split, '/');
        }
        Ok(None)
    }

    fn log(&self, update: &Update, executor: &str) -> Result<(), String> {
        if is_zero(&update.new) {
            return Err("a runner log is never deleted".to_owned());
        }
        let range = if is_zero(&update.old) {
            update.new.clone()
        } else {
            if !self.git.is_ancestor(&update.old, &update.new)? {
                return Err("a runner log only appends".to_owned());
            }
            format!("{}..{}", update.old, update.new)
        };
        let commits = self
            .git
            .run(&["rev-list", &range])
            .map_err(|error| message(&error))?;
        for commit in commits.lines() {
            match self.signer(commit, &[KeyClass::Runner])? {
                Some((_, principal)) if principal == executor => {}
                _ => return Err(format!("{commit} is not signed by runner {executor}")),
            }
        }
        Ok(())
    }

    /// Only a registry key fast-forwards the registry, each version signed under the one before
    /// it, and every version must be valid (one key, one class file, one principal).
    fn registry(&self, update: &Update) -> Result<(), String> {
        if is_zero(&update.new) || is_zero(&update.old) {
            return Err(
                "the registry is created once by its first push and never deleted".to_owned(),
            );
        }
        if !self.git.is_ancestor(&update.old, &update.new)? {
            return Err("the registry only fast-forwards".to_owned());
        }
        let commits = self
            .git
            .run(&[
                "rev-list",
                "--reverse",
                &format!("{}..{}", update.old, update.new),
            ])
            .map_err(|error| message(&error))?;
        for commit in commits.lines() {
            let previous = Registry::load_at(self.git, &format!("{commit}^1"))?;
            let registrars = previous.class_file(self.git, KeyClass::Registry)?;
            if signing::verify_commit(self.git, commit, &registrars)
                .map_err(|error| message(&error))?
                .is_none()
            {
                return Err(format!("{commit} is not signed by a registry key"));
            }
            let next = Registry::load_at(self.git, commit)?;
            // A removal stands (owner ruling 2026-10-22): a revoked key is never trusted again.
            let kept = next.removed_text();
            if let Some(dropped) = previous
                .removed()
                .iter()
                .map(crate::registry::Removal::line)
                .find(|line| !kept.lines().any(|kept| kept == line))
            {
                return Err(format!(
                    "{commit} drops the removal `{dropped}`; a removal is never undone"
                ));
            }
        }
        Ok(())
    }
}

impl Judge<'_> {
    /// A reservation: executor-signed, created above every existing number, or replaced in
    /// place by a newer operation of the same change when it is not the head.
    fn reservation(&self, update: &Update, seq: &str) -> Result<(), String> {
        let seq: u64 = seq
            .parse()
            .map_err(|_| "a queue number is decimal".to_owned())?;
        if is_zero(&update.new) {
            return self.queue_retention(update, seq);
        }
        if !matches!(
            self.signer(&update.new, &[KeyClass::Runner])?,
            Some((KeyClass::Runner, _))
        ) {
            return Err("a reservation is signed by a runner key".to_owned());
        }
        let queue = crate::queue::Queue::local(self.git)?;
        if is_zero(&update.old) {
            let highest = queue
                .entries
                .keys()
                .chain(queue.released.iter())
                .max()
                .copied()
                .unwrap_or(0);
            if seq <= highest {
                return Err(format!(
                    "a new reservation must be above every existing one ({highest})"
                ));
            }
            return Ok(());
        }
        let old = queue.entries.get(&seq).ok_or("replaces no reservation")?;
        // A newer revision keeps its entry's place, the head's too (the trust-agents ruling,
        // 2026-10-26); a released place is never taken again.
        if queue.released.contains(&seq) {
            return Err("a released place is never replaced".to_owned());
        }
        let text = self
            .git
            .run(&["cat-file", "blob", &format!("{}:entry.json", update.new)])
            .map_err(|error| message(&error))?;
        let entry: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        if entry["change"] != old.change.as_str() || entry["replaces"] != old.op_id.as_str() {
            return Err(
                "a reservation is replaced only by a newer operation of its own change".to_owned(),
            );
        }
        Ok(())
    }

    /// An intent claim: created once, together with its change, by the change's agent; the
    /// next generation only after the previous one's change closed: it landed, or it ended
    /// without landing (abandoned, superseded or no effect, G272) (§ 4).
    fn intent(&self, update: &Update, claim: &str) -> Result<(), String> {
        if !is_zero(&update.old) || is_zero(&update.new) {
            return Err("an intent claim is created once and never moved or deleted".to_owned());
        }
        let (hash, generation) = claim.split_once('/').ok_or("names no generation")?;
        let generation: u64 = generation
            .parse()
            .map_err(|_| "a generation is decimal".to_owned())?;
        if hash.len() != 32
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("an intent hash is 32 lower-case hex characters".to_owned());
        }
        let record = Chain::new(self.git)
            .record_at(&update.new)
            .map_err(|error| error.message())?;
        let matches = record
            .claim
            .as_ref()
            .is_some_and(|held| held.hash == hash && held.generation == generation);
        let with_change = self.updates.iter().any(|other| {
            other.name == format!("{}{}", crate::names::CHANGES, record.id)
                && is_zero(&other.old)
                && other.new == update.new
        });
        if !matches || !with_change {
            return Err(
                "an intent claim is created in the same push as the change that holds it"
                    .to_owned(),
            );
        }
        if generation > 1 && !self.generation_closed(hash, generation - 1)? {
            return Err(format!(
                "generation {generation} needs generation {} merged or given up",
                generation - 1
            ));
        }
        Ok(())
    }

    /// Whether generation `generation` of the intent `hash` is closed: its change landed, or
    /// its chain's current record ended it without a landing (abandoned, superseded or no
    /// effect, G272). A compacted claim was compacted only with its landed claim
    /// (`compacted_delete`), so its landed claim's publication names the change, which carries
    /// its landed marker (§ 10a).
    fn generation_closed(&self, hash: &str, generation: u64) -> Result<bool, String> {
        let claim = format!("{}{hash}/{generation}", crate::names::INTENT);
        if let Some(previous) = self.after(&claim)? {
            let before = Chain::new(self.git)
                .record_at(&previous)
                .map_err(|error| error.message())?;
            if self
                .marker(&format!("{}{}", crate::names::LANDED, before.id))?
                .is_some()
            {
                return Ok(true);
            }
            let chain = format!("{}{}", crate::names::CHANGES, before.id);
            if crate::names::bay_ref(&chain).is_err() {
                return Ok(false);
            }
            let Some(head) = self.after(&chain)? else {
                return Ok(false);
            };
            let current = Chain::new(self.git)
                .record_at(&head)
                .map_err(|error| error.message())?;
            return Ok(ended_unlanded(current.state));
        }
        if self.recorded(&claim)?.is_none() {
            return Err("the previous generation has no claim".to_owned());
        }
        let Some(publication) = self.marker(&format!(
            "{}{hash}/{generation}",
            crate::names::LANDED_INTENT
        ))?
        else {
            return Ok(false);
        };
        let change = self.trailer(&publication, "Change-Id")?;
        if crate::names::bay_ref(&format!("{}{change}", crate::names::LANDED)).is_err() {
            return Ok(false);
        }
        Ok(self
            .marker(&format!("{}{change}", crate::names::LANDED))?
            .is_some())
    }

    /// Whether generation `generation` of the intent `hash` is claimed after this push: by its
    /// claim ref, or by the shard index record that stands in for it (§ 10a).
    fn claim_exists(&self, hash: &str, generation: u64) -> Result<bool, String> {
        let claim = format!("{}{hash}/{generation}", crate::names::INTENT);
        Ok(self.marker(&claim)?.is_some())
    }

    /// A landed-intent marker: created once, with the trunk update that lands its generation,
    /// whose change held that claim (§ 6: two changes may hold one generation; the one that
    /// lands names it).
    fn landed_intent(&self, update: &Update, claim: &str, trunk: &str) -> Result<(), String> {
        if !is_zero(&update.old) {
            return Err("this generation of the intent already merged".to_owned());
        }
        // `crate::throughput`: a batch member's marker names its own commit, which the trunk
        // update lands as part of its whole batch (`Judge::trunk` judges the batch).
        let with_trunk = self.by_name.get(trunk).is_some_and(|other| {
            other.new == update.new || self.lands_in_batch(&update.new, other).unwrap_or(false)
        });
        if !with_trunk {
            return Err(
                "is created only together with the update of main that merges it".to_owned(),
            );
        }
        let change = self.trailer(&update.new, "Change-Id")?;
        let chain = format!("{}{change}", crate::names::CHANGES);
        let names_holder =
            || format!("is created only by the merge of a change that holds {claim}");
        crate::names::bay_ref(&chain).map_err(|_| names_holder())?;
        let head = self.after(&chain)?.ok_or_else(names_holder)?;
        let record = Chain::new(self.git)
            .record_at(&head)
            .map_err(|error| error.message())?;
        if !holds_claim(&record, claim) {
            return Err(names_holder());
        }
        Ok(())
    }

    /// Whether `commit` carries a `Bay-Batch` trailer and the trunk update `trunk` lands it:
    /// it is new on trunk with this push.
    fn lands_in_batch(&self, commit: &str, trunk: &Update) -> Result<bool, String> {
        if self.trailer(commit, throughput::BATCH_TRAILER)?.is_empty() {
            return Ok(false);
        }
        Ok(self.git.is_ancestor(commit, &trunk.new)?
            && (is_zero(&trunk.old) || !self.git.is_ancestor(commit, &trunk.old)?))
    }

    /// A release: created once, executor-signed, for a reservation that exists.
    fn queue_release(&self, update: &Update, seq: &str) -> Result<(), String> {
        if !is_zero(&update.old) && is_zero(&update.new) {
            let seq: u64 = seq
                .parse()
                .map_err(|_| "a queue number is decimal".to_owned())?;
            return self.queue_retention(update, seq);
        }
        if !is_zero(&update.old) || is_zero(&update.new) {
            return Err("a release is created once and never moved".to_owned());
        }
        if !matches!(
            self.signer(&update.new, &[KeyClass::Runner])?,
            Some((KeyClass::Runner, _))
        ) {
            return Err("a release is signed by a runner key".to_owned());
        }
        let reserved = format!("{}{seq}", crate::queue::QUEUE_PREFIX);
        if self.after(&reserved)?.is_none() {
            return Err("releases no reservation".to_owned());
        }
        Ok(())
    }

    /// § 10a queue retention: a queue ref is removed only with its partner, as one whole
    /// (reservation, release) pair, below the head, never the highest pair, and only when the
    /// operation the reservation names is finished in its change chain (terminal, or gone from
    /// a chain the retention cut closed), so no place a later judgement counts is lost. One
    /// push removes at most [`crate::queue::PRUNE_MAX`] pairs.
    fn queue_retention(&self, update: &Update, seq: u64) -> Result<(), String> {
        use crate::queue::{DONE_PREFIX, PRUNE_MAX, QUEUE_PREFIX};
        let removed = |name: &str| {
            self.by_name
                .get(name)
                .is_some_and(|other| is_zero(&other.new) && !is_zero(&other.old))
        };
        let pair = |seq: u64| {
            removed(&format!("{QUEUE_PREFIX}{seq}")) && removed(&format!("{DONE_PREFIX}{seq}"))
        };
        if is_zero(&update.old) || !pair(seq) {
            return Err(
                "a queue ref is removed only with its partner, as one (reservation, release) pair, by retention"
                    .to_owned(),
            );
        }
        let removing = self
            .updates
            .iter()
            .filter(|other| other.name.starts_with(QUEUE_PREFIX) && is_zero(&other.new))
            .count();
        if removing > PRUNE_MAX {
            return Err(format!(
                "a push removes at most {PRUNE_MAX} queue pairs; this one removes {removing}"
            ));
        }
        let queue = self
            .queue
            .get_or_init(|| crate::queue::Queue::local(self.git))
            .as_ref()
            .map_err(Clone::clone)?;
        let entry = queue
            .entries
            .get(&seq)
            .filter(|_| queue.released.contains(&seq))
            .ok_or("retention removes only a released reservation")?;
        if queue.head().is_some_and(|head| head.seq < seq) {
            return Err("retention removes only pairs below the queue's head".to_owned());
        }
        let kept_above = queue
            .entries
            .keys()
            .any(|above| *above > seq && queue.released.contains(above) && !pair(*above));
        if !kept_above {
            return Err("retention always keeps the highest queue pair".to_owned());
        }
        let (_, record) = Chain::new(self.git).read(&entry.change).map_err(|error| {
            format!(
                "the chain of {} cannot be read: {}",
                entry.change,
                error.message()
            )
        })?;
        if !record.finished(&entry.op_id) {
            return Err(format!(
                "retention removes only the places of a finished operation; {} is not",
                entry.op_id
            ));
        }
        Ok(())
    }
}

struct Request {
    revision: String,
    repair: bool,
}
