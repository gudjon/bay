//! Throughput: more landings per check slot, never a weaker verdict (owner ruling
//! 2026-10-20 on a 130-agent trunk; NEW-2, NEW-10 and batching with bisection; BACKLOG 3).
//!
//! One queue checks only its head, one check after another, so check slots, not merges, set a
//! trunk's ceiling (`DESIGN` § 6: about 120 landings a day for a 12-minute check). Three levers
//! raise it. Each is off by default behind a key of `[runner]` in `.bay/config.toml`, and each
//! is a rule of evidence reuse, never a new verdict: every one needs `[policy] evidence_reuse =
//! "exact-inputs"` and a producer registry that signs everything, or, where evidence counts
//! unsigned (no registry, or `signing = "approvals"`), `[policy] reuse_unsigned = true` in the
//! policy at the base (G318); without them nothing here runs. What they keep:
//!
//! - **G1, what lands is exactly the checked tree on the trunk it was checked against.** The
//!   publishing push is still judged per commit by the executor and by the host hook: a commit
//!   lands on evidence for its own tree under the policy at its base, producer-signed unless
//!   the policy reuses unsigned evidence, whose results still name their trust. Two
//!   widenings are each the policy's to declare: a carried record (below) holds for a tree only
//!   where the check's policy entry says its verdict depends on nothing outside its read set,
//!   and a batch member before the last stands on its batch tip's evidence, the atomic group
//!   `DESIGN` § 6 already allows, under a config that sets `batch`.
//! - **Unknown is never pass.** A prediction that misses, a footprint that differs, a stamp
//!   bay cannot compute, a record of a shape it cannot read: each only means the evidence does
//!   not match, and the check runs.
//! - **One publication at a time is the single commit point.** The queue, the head's lease,
//!   the progress deadline and the one atomic push are unchanged. Nothing here publishes out of
//!   queue order.
//! - **Evidence names exactly what it covers.** A pre-check's record names the predicted tree
//!   and base it ran on; a carried record names the tree it is about and the record it was
//!   carried from; a batch member's landing names the batch tip whose tree was checked.
//!
//! # Pre-checks (NEW-2 `ahead`, NEW-10 `footprint_lanes`)
//!
//! An executor that reaches a queued operation it may not run (`DESIGN` § 6: while a queue is
//! live only the head runs) checks it now, ahead of its turn, instead of only holding its
//! place. Nothing is published, no attempt is recorded and no ownership is taken; the run's
//! records go to this executor's producer ref, and the operation's chain lists the check under
//! `prechecks` (base, base tree, tree, verdict). It runs on one of two bases:
//!
//! - **Predicted** (`[runner] ahead = N`, entries 2 to N + 1). The base is what the entries
//!   ahead predict ([`predict`]): from the trunk tip, each live entry ahead folds onto the base
//!   in queue order ([`fold`]). A checked entry's persisted candidate is used as it is; any
//!   other becomes an unsigned predicted commit of its merged tree on the base, never
//!   published. That commit stands for the entry's real candidate exactly: `merge_onto` is
//!   deterministic, and its result depends only on the two sides' trees and their merge bases.
//!   A predicted commit and the real candidate have one tree, their single parents stand for
//!   each other in the same way down to the trunk tip, and no revision descends from either; so
//!   a revision's merge bases with either are its merge bases with the trunk tip, and every
//!   later entry merges onto them to the same tree. A predicted base can therefore be built
//!   before the head's own candidate exists, and checked while the head is checked. An entry ahead that conflicts or whose work is already
//!   on the base is predicted not to land and is skipped; one that changes the policy or an
//!   adapter path stops the prediction, since what follows it is judged by another policy.
//!   When the entry becomes head and trunk is what was predicted, its real merge is the same
//!   tree on the same base tree, and exact-inputs reuse (G7) lands it with no adapter run. When
//!   the prediction misses (an entry ahead failed, conflicted or was replaced), the record is
//!   about another tree or base and is ignored: up to N entries of work are wasted, never
//!   trusted.
//! - **The tip** (`[runner] footprint_lanes = true`, an entry beyond `ahead`). An entry
//!   whose footprint is disjoint from every live entry's ahead of it is checked on the current
//!   tip. Its exact tree will not be the one it lands with, so only footprint-keyed evidence
//!   (below) carries it over.
//!
//! Capacity is the number of executors (each its own producer and check slot): an executor
//! that does not hold the head does useful work instead of idling with `QUEUED`. Two
//! executors that pre-check one entry on one base run it at most twice, and the second finds
//! the first's records through reuse before it runs anything.
//!
//! # Footprint-keyed evidence (NEW-10)
//!
//! An entry's **footprint** is the union of the `when_paths` of the checks its candidate's
//! diff requires; a required check with no `when_paths` makes it the whole tree ([`Footprint`]).
//! Two footprints are disjoint only when bay can show it from their globs' literal prefixes;
//! otherwise they overlap. A footprint only schedules work: no verdict rests on it.
//!
//! A check's **read set** is what its verdict may depend on: its `paths` (the only part of the
//! tree its materialization holds) when it has them, else its `when_paths`. A check declared
//! with `footprint = true` in the policy says its verdict depends on nothing but the read set's
//! content, in the candidate's tree and in its base's. Its records then carry a stamp: digests
//! of exactly the read set's entries in both trees. When a later candidate of the same
//! operation (a rebuild after trunk moved, or the head's candidate after a pre-check) has the
//! same stamp under the same policy revision, provenance, manifest digest and fixtures, the
//! record holds for it too: this executor's producer writes a record on the new tree that
//! carries the verdict and names the record it carries ([`carry`]), and the check does not run.
//! An unrelated landing no longer stales a candidate.
//!
//! The opt-in is required: a check whose verdict depends on files outside its `when_paths` (a
//! test suite that imports a library elsewhere) would be carried onto a tree it never judged,
//! which is G1 broken. `footprint = true` is judged at the base like every policy field and
//! enters the definition digest, so a candidate can neither grant it to itself nor reuse
//! records made before it was declared. It is the "narrow only where a check provider accounts
//! for its dependencies" of `DESIGN` § 7, stated by the policy owner per check. A check keyed by
//! commit is never carried.
//!
//! # Queue order
//!
//! Publication stays one fast-forward at a time in queue order, even for a disjoint entry
//! behind a blocked head. Queue order is a contract: the hook refuses every trunk push but the
//! head's, an agent reads its position, and the livelock and deadline arguments of § 6 count
//! places in order. Letting a disjoint entry pass would need the hook to admit a non-head
//! publisher and would reopen them. Little is lost by keeping it: a blocked head releases at
//! once (the head always releases), a stuck one within its progress deadline, and the entry
//! behind it was pre-checked meanwhile, so when it becomes head its evidence carries over and it
//! lands with one merge and one push and no check.
//!
//! # Batching with bisection (`[runner] batch = { min, max, max_failure_rate }`)
//!
//! The head's executor takes the head and the live entries behind it, in queue order, while
//! each folds onto the one before it ([`plan_batch`]); with footprint lanes, only while each
//! one's footprint overlaps the head's (one lane). At least `max(min, 2)` and at most `max`
//! members, none of a lane, with named inputs, a policy change, or a culprit's note. It checks
//! the predicted tip once on the trunk tip: the tree of every member merged in order, with the
//! paths the whole batch changes against trunk. A batch is skipped when the recent failure
//! rate of its lane (the last [`RATE_WINDOW`] check outcomes of released queue places whose
//! footprint overlaps the head's) is above `max_failure_rate` (default 0.3).
//!
//! On a pass every member lands in one atomic push: each member keeps its own executor-signed
//! commit with its own trailers, parented on the member before it, plus `Bay-Batch: <id> <i>/<k>`;
//! trunk moves once, from its tip to the last member; each member's landed marker, intent
//! marker and queue release move with it. Trunk never points at a member before the last, and
//! the batch lands or fails together, which is the atomic group of `DESIGN` § 6 ("a tested
//! combined tree does not prove each member"). The host hook admits a `Bay-Batch` commit only
//! as part of a whole batch in one push, under a config at its base that sets `batch`, judging
//! the tip's evidence by the policy at the batch's base over the batch's whole diff; every other
//! rule of a publication holds for each member. Each member's landing says it was checked as
//! that batch's tip (`detail.batch`), so its evidence is named as what it is.
//!
//! On a failure it bisects over prefixes on the same trunk tip ([`Bisect`]): every prefix is a
//! tree on that tip, so a prefix checked once is never run again (exact reuse). It finds the
//! largest passing prefix `p` and the first failing member `p + 1`, the culprit. When `p` is two
//! or more, the prefix lands as a batch; the culprit's chain notes the batch, and it runs alone
//! when it is head, judged by its own candidate. When `p` is one or zero, nothing lands as a
//! batch: the head runs its ordinary path, and its prefix's records are its own candidate's
//! tree on its own base, so it lands or fails on them with no new run.
//!
//! Every operation still reaches a stated outcome through its own chain: members that landed
//! derive it from trunk, the culprit and the members behind it keep their places.

use serde_json::{Value, json};

use crate::change::{Chain, OpState};
use crate::evidence::{EvidenceRecord, Producer, Subject};
use crate::git::Git;
use crate::merge::{Merge, merge_onto};
use crate::policy::{CheckEntry, Policy};

/// The most entries behind the head `[runner] ahead` may pre-check: each costs a check run
/// that a missed prediction wastes.
pub const MAX_AHEAD: usize = 16;

/// The most pre-checks one operation's chain keeps, newest last.
pub const PRECHECKS_KEPT: usize = 8;

/// The message of a predicted commit: no trailer, so it is never taken for a publication.
const PREDICTED: &str = "bay: a predicted candidate, checked ahead of its turn and never published";

/// One queue entry as a prediction reads it from the change chain this repository holds.
#[derive(Clone, Debug)]
pub struct Queued {
    pub seq: u64,
    pub change: String,
    pub op_id: String,
    pub revision: String,
    /// The operation can still land: it is neither terminal nor blocked.
    pub live: bool,
    /// It may join a batch: its change is of no sandbox lane (NEW-4, it never lands), names no
    /// inputs (no record shows their versions matched), and no bisection found it a batch's
    /// first failing member (then it runs alone).
    pub batchable: bool,
    /// The trunk commit and candidate of a checked attempt the operation would publish.
    pub checked: Option<(String, String)>,
    /// The base trees its pre-checks found a product failure on: on such a base it will not
    /// land, so a prediction and a batch pass over it (a fleet finding, 2026-10-26: a member
    /// whose pre-check had failed went into a batch, turned it red and cost a bisection, and
    /// every pre-check behind it ran on a base that counted it as landed).
    pub fails_on: Vec<String>,
}

impl Queued {
    /// Whether its pre-check found a product failure on `base_tree`.
    #[must_use]
    pub fn fails_on(&self, base_tree: &str) -> bool {
        self.fails_on.iter().any(|tree| tree == base_tree)
    }
}

impl Queued {
    /// `entry`'s operation as its change chain here holds it; `None` when the chain or the
    /// operation cannot be read.
    #[must_use]
    pub fn read(git: &Git, entry: &crate::queue::Entry) -> Option<Self> {
        let (_, record) = Chain::new(git).read(&entry.change).ok()?;
        let op = record
            .operations
            .iter()
            .find(|op| op.op_id == entry.op_id)?;
        let checked = op
            .attempts
            .last()
            .filter(|_| op.state == OpState::Checked)
            .map(|attempt| (attempt.base.clone(), attempt.commit.clone()));
        Some(Self {
            seq: entry.seq,
            change: entry.change.clone(),
            op_id: op.op_id.clone(),
            revision: op.revision.clone(),
            live: !op.state.is_terminal() && op.state != OpState::Blocked,
            batchable: record.lane.is_none()
                && crate::inputs::names(&record.declared_inputs).is_empty()
                && op.batch_culprit.is_none(),
            checked,
            fails_on: op
                .prechecks
                .iter()
                .filter(|precheck| precheck.verdict == PRODUCT_FAILURE)
                .map(|precheck| precheck.base_tree.clone())
                .collect(),
        })
    }
}

/// A pre-check verdict that names a failure of the change itself, never of a check's machinery.
const PRODUCT_FAILURE: &str = "product_failure";

/// What one queue entry does to a predicted base.
pub enum Fold {
    /// It merges onto the base: the predicted candidate (an unsigned commit of the merged
    /// tree on the base, never published), its tree, and the paths it changes against the base.
    Applied {
        commit: String,
        tree: String,
        changed: Vec<String>,
    },
    /// It will not land on this base: it conflicts, or its work is already there.
    Skipped,
    /// It changes the policy or an adapter path, so what follows it is judged by another
    /// policy: nothing is predicted past it.
    Stop,
}

/// Folds `revision` onto the predicted base `base` under `policy`, the policy at the trunk tip
/// the prediction starts from.
///
/// # Errors
///
/// A git fault.
pub fn fold(git: &Git, policy: &Policy, base: &str, revision: &str) -> Result<Fold, String> {
    let tree = match merge_onto(git, base, revision).map_err(|error| error.message())? {
        Merge::Clean(tree) => tree,
        Merge::Conflict(..) => return Ok(Fold::Skipped),
    };
    if tree == tree_of(git, base)? {
        return Ok(Fold::Skipped);
    }
    let changed = crate::check::changed_paths(git, base, &tree)?;
    if policy.touches_policy(&changed) {
        return Ok(Fold::Stop);
    }
    let commit = predicted_commit(git, &tree, base)?;
    Ok(Fold::Applied {
        commit,
        tree,
        changed,
    })
}

/// The base the live entries `ahead` (head first) predict for the entry behind them, from
/// `trunk` under its `policy`; `None` when an entry ahead changes the policy.
///
/// # Errors
///
/// A git fault.
pub fn predict(
    git: &Git,
    policy: &Policy,
    trunk: &str,
    ahead: &[Queued],
) -> Result<Option<String>, String> {
    let mut base = trunk.to_owned();
    for queued in ahead.iter().filter(|queued| queued.live) {
        // A checked entry's persisted candidate on this very base is what it publishes.
        if let Some((on, commit)) = &queued.checked
            && *on == base
        {
            if policy.touches_policy(&crate::check::changed_paths(git, &base, commit)?) {
                return Ok(None);
            }
            base.clone_from(commit);
            continue;
        }
        if queued.fails_on(&tree_of(git, &base)?) {
            continue;
        }
        match fold(git, policy, &base, &queued.revision)? {
            Fold::Applied { commit, .. } => base = commit,
            Fold::Skipped => {}
            Fold::Stop => return Ok(None),
        }
    }
    Ok(Some(base))
}

/// An unsigned commit of `tree` on `parent` that stands for a candidate not yet built: it
/// carries no trailer, so it is never a publication, and bay never pushes it.
///
/// # Errors
///
/// A git fault.
pub fn predicted_commit(git: &Git, tree: &str, parent: &str) -> Result<String, String> {
    git.run_with(
        &[
            "commit-tree",
            tree,
            "-p",
            parent,
            "--no-gpg-sign",
            "-m",
            PREDICTED,
        ],
        crate::change::RECORD_IDENTITY,
        None,
    )
    .map_err(|error| error.message())
}

/// The tree of `commit`.
///
/// # Errors
///
/// A git fault.
pub fn tree_of(git: &Git, commit: &str) -> Result<String, String> {
    git.run(&["rev-parse", &format!("{commit}^{{tree}}")])
        .map_err(|error| error.message())
}

/// The key under which a record of a check with `footprint = true` holds its stamp: the read
/// set, and the digests of its entries in the candidate's tree and in its base's. A carried
/// record adds `from`, the record it carries.
pub const FOOTPRINT: &str = "footprint";

/// A candidate's footprint (NEW-10): the union of the `when_paths` of the checks its diff
/// requires, or the whole tree when one of them has no `when_paths`. It only schedules work:
/// no verdict rests on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Footprint {
    Whole,
    Globs(Vec<String>),
}

impl Footprint {
    /// The footprint of a candidate whose diff against its base changes `changed`, under
    /// `policy`.
    #[must_use]
    pub fn of(policy: &Policy, changed: &[String]) -> Self {
        let (required, _) = policy.required(changed);
        let mut globs = Vec::new();
        for entry in &required {
            let id = crate::evidence::CheckId::new(&entry.id, &entry.environment);
            match policy
                .check(&id)
                .and_then(|check| check.when_paths.as_ref())
            {
                Some(paths) => globs.extend(paths.iter().cloned()),
                None => return Self::Whole,
            }
        }
        globs.sort_unstable();
        globs.dedup();
        Self::Globs(globs)
    }

    /// Whether the two may share a path. They are disjoint only when bay can show it: a
    /// footprint that requires no check shares nothing, and two globs are apart when neither's
    /// literal prefix (the part before its first wildcard) starts the other's.
    #[must_use]
    pub fn overlaps(&self, other: &Self) -> bool {
        let empty = |footprint: &Self| matches!(footprint, Self::Globs(globs) if globs.is_empty());
        if empty(self) || empty(other) {
            return false;
        }
        match (self, other) {
            (Self::Globs(ours), Self::Globs(theirs)) => ours
                .iter()
                .any(|glob| theirs.iter().any(|other| globs_overlap(glob, other))),
            _ => true,
        }
    }
}

/// Whether `a` and `b` may match one path: every path a glob matches starts with its literal
/// prefix, so two globs whose prefixes do not start one another match none in common.
fn globs_overlap(a: &str, b: &str) -> bool {
    let (a, b) = (literal_prefix(a), literal_prefix(b));
    a.starts_with(b) || b.starts_with(a)
}

/// The part of `glob` before its first wildcard.
fn literal_prefix(glob: &str) -> &str {
    glob.find(['*', '?', '[']).map_or(glob, |at| &glob[..at])
}

/// Whether `revision`'s footprint on `trunk` is disjoint from that of every live entry
/// `ahead` of it on `trunk`, each merged onto it alone, under the trunk's `policy`. A revision
/// that does not merge onto trunk has no footprint there and is disjoint from none.
///
/// # Errors
///
/// A git fault.
pub fn disjoint(
    git: &Git,
    policy: &Policy,
    trunk: &str,
    revision: &str,
    ahead: &[Queued],
) -> Result<bool, String> {
    let Some(ours) = footprint_on(git, policy, trunk, revision)? else {
        return Ok(false);
    };
    for queued in ahead.iter().filter(|queued| queued.live) {
        if let Some(theirs) = footprint_on(git, policy, trunk, &queued.revision)?
            && ours.overlaps(&theirs)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The footprint of `revision` merged onto `trunk`; `None` when it conflicts there.
fn footprint_on(
    git: &Git,
    policy: &Policy,
    trunk: &str,
    revision: &str,
) -> Result<Option<Footprint>, String> {
    match merge_onto(git, trunk, revision).map_err(|error| error.message())? {
        Merge::Clean(tree) => Ok(Some(Footprint::of(
            policy,
            &crate::check::changed_paths(git, trunk, &tree)?,
        ))),
        Merge::Conflict(..) => Ok(None),
    }
}

/// The stamp of check `entry`'s read set for a candidate `tree` on `base_tree`, or `None`
/// when the check declares no footprint or a digest cannot be made: then its record carries
/// no stamp, and nothing is ever carried from it.
#[must_use]
pub fn stamp(git: &Git, entry: &CheckEntry, tree: &str, base_tree: &str) -> Option<Value> {
    let read = entry.read_set()?;
    let ours = read_digest(git, tree, read).ok()?;
    let base = read_digest(git, base_tree, read).ok()?;
    Some(json!({"paths": read, "tree": ours, "base_tree": base}))
}

/// The digest of the entries of `tree` that `read` covers (`when_paths` globs, or `paths`
/// files and directories): each one's mode, type, object id and path, in Git's order, so two
/// trees with one digest hold the same bytes under the same names there.
///
/// # Errors
///
/// A git fault.
pub fn read_digest(git: &Git, tree: &str, read: &[String]) -> Result<String, String> {
    let listed = git
        .run(&["ls-tree", "-r", "-z", "--full-tree", tree])
        .map_err(|error| error.message())?;
    let entries: Vec<&str> = listed
        .split('\0')
        .filter(|line| {
            line.split_once('\t').is_some_and(|(_, path)| {
                read.iter()
                    .any(|glob| crate::policy::glob_matches(glob, path))
            })
        })
        .collect();
    crate::canonical::digest(&json!(entries))
}

/// Items 1 and 2: the observed footprint of a check that read the files `read` and listed the
/// directories `listed` (paths relative to the tree, `""` for its root, [`crate::check::observe`]):
/// each read file with its blob id, each listed directory with the hash of its sorted entry
/// names, and the digest of both in `tree` and in `base_tree`. A record stamped with it holds
/// for a later candidate whose tree, and base, have the same bytes and entries there
/// ([`same_observed`]).
///
/// # Errors
///
/// A git fault.
pub fn observed_stamp(
    git: &Git,
    (tree, base_tree): (&str, &str),
    read: &[String],
    listed: &[String],
) -> Result<Value, String> {
    let ours = observed_digest(git, tree, read, listed)?;
    let base = observed_digest(git, base_tree, read, listed)?;
    Ok(json!({
        "observed": true,
        "read": ours.read,
        "listed": ours.listed,
        "tree": ours.digest,
        "base_tree": base.digest,
    }))
}

/// What a tree holds at an observed footprint's paths.
struct Seen {
    read: Vec<(String, String)>,
    listed: Vec<(String, String)>,
    digest: String,
}

fn observed_digest(
    git: &Git,
    tree: &str,
    read: &[String],
    listed: &[String],
) -> Result<Seen, String> {
    let all = git
        .run(&["ls-tree", "-r", "-t", "-z", "--full-tree", tree])
        .map_err(|error| error.message())?;
    // path -> object id, for every blob and tree; and dir -> its entry names.
    let mut objects = std::collections::BTreeMap::new();
    let mut entries: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for line in all.split('\0').filter(|line| !line.is_empty()) {
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        let id = meta
            .split_whitespace()
            .nth(2)
            .unwrap_or_default()
            .to_owned();
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        entries
            .entry(dir.to_owned())
            .or_default()
            .push(name.to_owned());
        objects.insert(path.to_owned(), id);
    }
    let read: Vec<(String, String)> = read
        .iter()
        .map(|path| (path.clone(), objects.get(path).cloned().unwrap_or_default()))
        .collect();
    let listed: Vec<(String, String)> = listed
        .iter()
        .map(|dir| {
            let mut names = entries.get(dir).cloned().unwrap_or_default();
            names.sort();
            let hash = crate::canonical::digest(&json!(names)).unwrap_or_default();
            (dir.clone(), hash)
        })
        .collect();
    let digest = crate::canonical::digest(&json!({"read": read, "listed": listed}))?;
    Ok(Seen {
        read,
        listed,
        digest,
    })
}

/// Whether `record`, stamped with an observed footprint, holds for `tree` and `base_tree`: its
/// read files and listed directories are the same there as in the tree it ran on.
///
/// # Errors
///
/// A git fault.
pub fn same_observed(
    git: &Git,
    record: &EvidenceRecord,
    (tree, base_tree): (&str, &str),
) -> Result<bool, String> {
    let Some(held) = record
        .unknown
        .get(FOOTPRINT)
        .filter(|held| held["observed"] == true)
    else {
        return Ok(false);
    };
    let paths = |field: &str| -> Vec<String> {
        held[field]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| row.get(0).and_then(Value::as_str).map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let (read, listed) = (paths("read"), paths("listed"));
    let ours = observed_digest(git, tree, &read, &listed)?;
    let base = observed_digest(git, base_tree, &read, &listed)?;
    Ok(held["tree"] == json!(ours.digest) && held["base_tree"] == json!(base.digest))
}

/// `record`'s observed footprint as it is in `tree` and `base_tree`: the same paths, read
/// there.
///
/// # Errors
///
/// A git fault, or a record with no observed footprint.
pub fn restamp(git: &Git, record: &EvidenceRecord, trees: (&str, &str)) -> Result<Value, String> {
    let held = record
        .unknown
        .get(FOOTPRINT)
        .filter(|held| held["observed"] == true)
        .ok_or("the record has no observed footprint")?;
    let paths = |field: &str| -> Vec<String> {
        held[field]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| row.get(0).and_then(Value::as_str).map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    observed_stamp(git, trees, &paths("read"), &paths("listed"))
}

/// Whether `record` holds exactly `stamp`: the same read set, and the same digests of it in
/// its tree and its base's.
#[must_use]
pub fn same_stamp(record: &EvidenceRecord, stamp: &Value) -> bool {
    record.unknown.get(FOOTPRINT).is_some_and(|held| {
        held["paths"] == stamp["paths"]
            && held["tree"] == stamp["tree"]
            && held["base_tree"] == stamp["base_tree"]
    })
}

/// What a carried record is about: the candidate's tree and commit and its base's tree.
pub struct Onto<'a> {
    pub tree: &'a str,
    pub commit: &'a str,
    pub base_tree: &'a str,
}

/// The record `producer` writes under `id` on `onto`, in place of a run of `record`'s check:
/// `record`'s verdict, units and definition, a subject of its own, this producer, this time, no
/// run time, and `stamp` with `from`, the record it carries. Unsigned: the caller signs it.
#[must_use]
pub fn carry(
    record: &EvidenceRecord,
    producer: &str,
    id: String,
    onto: &Onto,
    stamp: &Value,
) -> EvidenceRecord {
    let mut carried = record.clone();
    carried.id = id;
    carried.subject = Subject {
        tree: onto.tree.to_owned(),
        commit: onto.commit.to_owned(),
        base_tree: onto.base_tree.to_owned(),
        unknown: serde_json::Map::new(),
    };
    carried.producer = Producer {
        name: producer.to_owned(),
        version: crate::version::BAY.to_owned(),
        unknown: serde_json::Map::new(),
    };
    carried.at = crate::clock::now_rfc3339();
    carried.seconds = Some(0.0);
    carried.signature = None;
    let mut held = stamp.clone();
    held["from"] = json!({
        "id": record.id,
        "tree": record.subject.tree,
        "base_tree": record.subject.base_tree,
    });
    carried.unknown.insert(FOOTPRINT.to_owned(), held);
    carried
}

/// G292: a candidate of one change as its chain names it: its commit and tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Earlier {
    pub commit: String,
    pub tree: String,
}

/// G292: every candidate the operations of `record` built, oldest first: what an approval of
/// the change may have answered.
#[must_use]
pub fn candidates_of(record: &crate::change::ChangeRecord) -> Vec<Earlier> {
    record
        .operations
        .iter()
        .flat_map(|op| op.attempts.iter())
        .map(|attempt| Earlier {
            commit: attempt.commit.clone(),
            tree: attempt.tree.clone(),
        })
        .collect()
}

/// G292: the candidates of the change whose `Change-Id` trailer `commit` carries, as the change
/// chain this repository holds names them; none for a commit of no one change, or of a change
/// whose chain this repository does not hold.
#[must_use]
pub fn change_candidates(git: &Git, commit: &str) -> Vec<Earlier> {
    let Ok(change) = git.run(&[
        "log",
        "-1",
        "--format=%(trailers:key=Change-Id,valueonly,separator=%x2c)",
        commit,
    ]) else {
        return Vec::new();
    };
    let change = change.trim();
    if change.is_empty() || change.contains(',') {
        return Vec::new();
    }
    Chain::new(git)
        .read(change)
        .map(|(_, record)| candidates_of(&record))
        .unwrap_or_default()
}

/// G292: what the approval checks of one candidate take from the approvals of its change's
/// earlier candidates ([`carried_approvals`]).
#[derive(Debug, Default)]
pub struct CarriedApprovals {
    /// The approver-signed passes of earlier candidates that answer here: each the record as
    /// its approver signed it, about the candidate it named. Nothing new is written for them.
    pub records: Vec<EvidenceRecord>,
    /// Each earlier pass set aside because the files it covers changed since:
    /// `{"check", "environment", "record", "commit", "paths"}`, `paths` the gated paths whose
    /// content differs in the candidate's tree or in its base's.
    pub set_aside: Vec<Value>,
}

impl CarriedApprovals {
    /// What a result names of each carried record: `{"check", "environment", "carried_from",
    /// "commit", "approver"}`, `carried_from` the record that answers.
    #[must_use]
    pub fn named(&self) -> Vec<Value> {
        self.records
            .iter()
            .map(|record| {
                json!({
                    "check": record.check.id,
                    "environment": record.check.environment,
                    "carried_from": record.id,
                    "commit": record.subject.commit,
                    "approver": record.producer.name,
                })
            })
            .collect()
    }

    /// The set-aside rows of approval check `check` on `environment`.
    #[must_use]
    pub fn set_aside_of(&self, check: &str, environment: &str) -> Vec<Value> {
        self.set_aside
            .iter()
            .filter(|row| row["check"] == check && row["environment"] == environment)
            .cloned()
            .collect()
    }
}

/// G292: an approval holds while the files it covers are unchanged. For each approval check of
/// `policy` that carries ([`CheckEntry::carries`]) and that the candidate's own counted records
/// (`own`) leave unanswered, the counted passes of that check, under the same manifest, on an
/// earlier candidate of the same change (`earlier`, as its chain names them), whose stamp is
/// this candidate's: the check's `when_paths` content, byte for byte, in the candidate's `tree`
/// and in its `base_tree` ([`stamp`], [`same_stamp`]). That is the footprint rule of NEW-10,
/// applied to a person's yes without any `[runner]` setting.
///
/// A rejection never carries forward, and it outweighs every pass: a rejection of an earlier
/// candidate with the same stamp (or with none, which bay cannot compare) carries nothing for
/// the check, and a rejection of this candidate is its own answer. An earlier pass with a
/// stamp that differs is set aside, with the gated paths that changed; one with no stamp
/// carries nothing. Every reader takes this from here, landing, `bay status`, a target's
/// promotion rule and the host hook, so an approval counts the same everywhere (G288).
///
/// # Errors
///
/// A git fault, an unreadable note, or a class file that cannot be written.
pub fn carried_approvals(
    git: &Git,
    policy: &Policy,
    registry: Option<&crate::registry::Registry>,
    (tree, base_tree): (&str, &str),
    own: &[EvidenceRecord],
    earlier: &[Earlier],
) -> Result<CarriedApprovals, String> {
    use crate::evidence::{CheckId, Combined, KIND_APPROVAL, Verdict};
    let mut out = CarriedApprovals::default();
    let listed = policy.answering(own.to_vec());
    let open: Vec<&CheckEntry> = policy
        .checks
        .iter()
        .filter(|entry| entry.carries())
        .filter(|entry| {
            let id = CheckId::new(&entry.id, &entry.environment);
            crate::evidence::combine(&listed, &id, &policy.manifest_digest, None)
                == Combined::Unknown
        })
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    let earlier: Vec<&Earlier> = earlier
        .iter()
        .filter(|candidate| candidate.tree != tree && seen.insert(candidate.commit.as_str()))
        .collect();
    if open.is_empty() || earlier.is_empty() {
        return Ok(out);
    }
    // Each earlier candidate's counted approvals about it, under this manifest, read once.
    let mut approvals: Vec<EvidenceRecord> = Vec::new();
    for candidate in earlier {
        let records = policy.answering(crate::evidence::counted(
            git,
            crate::evidence::records_for(git, &candidate.tree)?,
            registry,
        )?);
        approvals.extend(records.into_iter().filter(|record| {
            record.kind == KIND_APPROVAL
                && record.subject.commit == candidate.commit
                && record.manifest_digest == policy.manifest_digest
        }));
    }
    for entry in open {
        let id = CheckId::new(&entry.id, &entry.environment);
        let Some(stamp) = stamp(git, entry, tree, base_tree) else {
            continue;
        };
        let mut passes = Vec::new();
        let mut rejected = false;
        for record in approvals.iter().filter(|record| record.check == id) {
            let held = record.unknown.get(FOOTPRINT);
            let same = same_stamp(record, &stamp);
            match record.verdict {
                Verdict::ProductFailure if same || held.is_none() => rejected = true,
                Verdict::Pass if same => passes.push(record.clone()),
                Verdict::Pass if held.is_some() => out.set_aside.push(json!({
                    "check": entry.id,
                    "environment": entry.environment,
                    "record": record.id,
                    "commit": record.subject.commit,
                    "paths": gated_changes(git, entry, record, tree, base_tree),
                })),
                _ => {}
            }
        }
        if !rejected {
            out.records.extend(passes);
        }
    }
    Ok(out)
}

/// G292: the paths approval check `entry` covers whose content differs between what `record`
/// approved and the candidate `tree` on `base_tree`, in either tree; the check's globs when a
/// tree cannot be read here.
fn gated_changes(
    git: &Git,
    entry: &CheckEntry,
    record: &EvidenceRecord,
    tree: &str,
    base_tree: &str,
) -> Vec<String> {
    let globs = entry.when_paths.clone().unwrap_or_default();
    let mut paths = std::collections::BTreeSet::new();
    for (from, to) in [
        (record.subject.tree.as_str(), tree),
        (record.subject.base_tree.as_str(), base_tree),
    ] {
        if from == to {
            continue;
        }
        match git.run(&[
            "diff-tree",
            "-r",
            "-z",
            "--name-only",
            "--no-renames",
            from,
            to,
        ]) {
            Ok(listed) => paths.extend(
                listed
                    .split('\0')
                    .filter(|path| {
                        !path.is_empty()
                            && globs
                                .iter()
                                .any(|glob| crate::policy::glob_matches(glob, path))
                    })
                    .map(str::to_owned),
            ),
            Err(_) => paths.extend(globs.iter().cloned()),
        }
    }
    paths.into_iter().collect()
}

/// `[runner] batch` (`crate::config`): a batch has at least `min` members (and always two)
/// and at most `max`, and none forms while its lane's recent failure rate is above
/// `max_failure_rate`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub min: usize,
    pub max: usize,
    pub max_failure_rate: f64,
}

/// A batch's most members when `[runner] batch` names no `max`.
pub const DEFAULT_BATCH_MAX: usize = 8;

/// The most members `[runner] batch` may allow: each one more is a longer bisection.
pub const MAX_BATCH: usize = 64;

/// The lane failure rate above which no batch forms, when `[runner] batch` names none.
pub const DEFAULT_MAX_FAILURE_RATE: f64 = 0.3;

/// How many recent check outcomes of a lane its failure rate reads.
pub const RATE_WINDOW: usize = 20;

/// The trailer each member's commit of a batch carries: `<id> <position>/<size>`.
pub const BATCH_TRAILER: &str = "Bay-Batch";

/// One member of a planned batch: its queue place and operation, and its predicted candidate
/// (an unsigned commit of its merged tree on the member before it, the first on trunk).
#[derive(Clone, Debug)]
pub struct Member {
    pub seq: u64,
    pub change: String,
    pub op_id: String,
    pub revision: String,
    pub commit: String,
    pub tree: String,
}

/// The batch the head and the live entries `behind` it (nearest first) form on `trunk` under
/// its `policy`: members in queue order while each folds onto the one before it, at most `max`,
/// none of a lane, with named inputs or a culprit's note, and none past a policy change, a
/// conflict or a merge with no effect. With `by_lane`, only while each one's footprint overlaps
/// the head's. The head is always the first member when it folds onto trunk.
///
/// # Errors
///
/// A git fault.
pub fn plan_batch(
    git: &Git,
    policy: &Policy,
    trunk: &str,
    head: &Queued,
    behind: &[Queued],
    max: usize,
    by_lane: bool,
) -> Result<Vec<Member>, String> {
    let mut members: Vec<Member> = Vec::new();
    let mut lane: Option<Footprint> = None;
    for queued in std::iter::once(head).chain(behind) {
        if members.len() >= max {
            break;
        }
        if !members.is_empty() && (!queued.live || !queued.batchable) {
            break;
        }
        let base = members
            .last()
            .map_or_else(|| trunk.to_owned(), |member| member.commit.clone());
        // A member known to fail on this base is passed over, never batched: it runs alone as
        // the head in its turn.
        if !members.is_empty() && queued.fails_on(&tree_of(git, &base)?) {
            continue;
        }
        let Fold::Applied {
            commit,
            tree,
            changed,
        } = fold(git, policy, &base, &queued.revision)?
        else {
            break;
        };
        if by_lane {
            let footprint = Footprint::of(policy, &changed);
            match &lane {
                None => lane = Some(footprint),
                Some(first) if first.overlaps(&footprint) => {}
                Some(_) => break,
            }
        }
        members.push(Member {
            seq: queued.seq,
            change: queued.change.clone(),
            op_id: queued.op_id.clone(),
            revision: queued.revision.clone(),
            commit,
            tree,
        });
    }
    Ok(members)
}

/// The search for a batch's largest passing prefix on one trunk tip: the whole batch first,
/// then, once a prefix failed, halving the range between the largest prefix known to pass and
/// the smallest known to fail. Every prefix is a tree on that one tip, so none is run twice.
#[derive(Clone, Copy, Debug)]
pub struct Bisect {
    size: usize,
    passed: usize,
    failed: Option<usize>,
}

impl Bisect {
    #[must_use]
    pub const fn new(size: usize) -> Self {
        Self {
            size,
            passed: 0,
            failed: None,
        }
    }

    /// The prefix (its member count) to check next; `None` once the search ended.
    #[must_use]
    pub const fn next_prefix(&self) -> Option<usize> {
        match self.failed {
            None if self.passed < self.size => Some(self.size),
            Some(failed) if failed - self.passed > 1 => {
                Some(self.passed + (failed - self.passed) / 2)
            }
            _ => None,
        }
    }

    /// Records the verdict of `prefix`.
    pub fn record(&mut self, prefix: usize, passed: bool) {
        if passed {
            self.passed = self.passed.max(prefix);
        } else {
            self.failed = Some(self.failed.map_or(prefix, |failed| failed.min(prefix)));
        }
    }

    /// The largest prefix that passed; 0 when none did.
    #[must_use]
    pub const fn passed(&self) -> usize {
        self.passed
    }

    /// The first member that fails (1 for the head), once a prefix failed.
    #[must_use]
    pub const fn culprit(&self) -> Option<usize> {
        match self.failed {
            Some(_) => Some(self.passed + 1),
            None => None,
        }
    }
}

/// Whether a queue place's release `reason` is a check's verdict on its candidate.
#[must_use]
pub fn is_check_outcome(reason: &str) -> bool {
    matches!(
        reason,
        "published" | "check_failed" | "check_flaky" | "check_unknown"
    )
}

/// The share of `outcomes` (release reasons, each a check outcome) that is not a landing; 0
/// for none.
#[must_use]
pub fn failure_rate<'a>(outcomes: impl IntoIterator<Item = &'a str>) -> f64 {
    let (mut checked, mut failed) = (0_u32, 0_u32);
    for outcome in outcomes {
        checked += 1;
        if outcome != "published" {
            failed += 1;
        }
    }
    if checked == 0 {
        0.0
    } else {
        f64::from(failed) / f64::from(checked)
    }
}

/// The footprint of the candidate a released queue place's operation last built, under
/// `policy`; `None` when its chain, operation or attempt cannot be read.
#[must_use]
pub fn place_footprint(
    git: &Git,
    policy: &Policy,
    entry: &crate::queue::Entry,
) -> Option<Footprint> {
    let (_, record) = Chain::new(git).read(&entry.change).ok()?;
    let op = record
        .operations
        .iter()
        .find(|op| op.op_id == entry.op_id)?;
    let attempt = op.attempts.last()?;
    let changed = crate::check::changed_paths(git, &attempt.base, &attempt.commit).ok()?;
    Some(Footprint::of(policy, &changed))
}

/// A batch's id: 16 hex digits of the digest of its head's operation and its trunk tip.
///
/// # Errors
///
/// The digest cannot be made.
pub fn batch_id(head: &str, trunk: &str) -> Result<String, String> {
    let digest = crate::canonical::digest(&json!({"head": head, "trunk": trunk}))?;
    Ok(digest
        .trim_start_matches("sha256:")
        .chars()
        .take(16)
        .collect())
}

/// The value of a `Bay-Batch` trailer: batch `id`, the member's `position` from 1, and the
/// batch's `size`, at least two.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchTag {
    pub id: String,
    pub position: usize,
    pub size: usize,
}

impl BatchTag {
    /// Reads `<id> <position>/<size>`; `None` for any other value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let (id, place) = value.trim().split_once(' ')?;
        let (position, size) = place.split_once('/')?;
        let (position, size): (usize, usize) = (position.parse().ok()?, size.parse().ok()?);
        let plain = !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_hexdigit());
        (plain && size >= 2 && (1..=size).contains(&position)).then(|| Self {
            id: id.to_owned(),
            position,
            size,
        })
    }

    /// The trailer line, newline included.
    #[must_use]
    pub fn trailer(&self) -> String {
        format!(
            "{BATCH_TRAILER}: {} {}/{}\n",
            self.id, self.position, self.size
        )
    }
}

/// How the host hook judges one commit of a trunk push (`crate::hook`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Of no batch: judged on its own tree's evidence by the policy at its parent.
    Alone,
    /// A batch member before the last: trunk never points at it, and the tip's evidence is
    /// the batch's.
    Member,
    /// A batch's last member, whose first member is at index `first` of the push: judged on its
    /// own tree's evidence by the policy at the batch's base, over the batch's whole diff.
    Tip { first: usize },
}

/// The role of each commit of a trunk push, oldest first, from its `Bay-Batch` trailer value
/// (`None` for none). A batch lands whole: its members are consecutive and in order, all in
/// the push.
///
/// # Errors
///
/// A trailer bay cannot read, or a batch the push holds only part of, or out of order.
pub fn roles(tags: &[Option<&str>]) -> Result<Vec<Role>, String> {
    let mut roles = Vec::with_capacity(tags.len());
    let mut at = 0;
    while at < tags.len() {
        let Some(value) = tags[at] else {
            roles.push(Role::Alone);
            at += 1;
            continue;
        };
        let batch = BatchTag::parse(value)
            .ok_or_else(|| format!("{BATCH_TRAILER}: {value:?} is not <id> <position>/<size>"))?;
        let whole = batch.position == 1
            && at + batch.size <= tags.len()
            && (1..=batch.size).all(|position| {
                tags[at + position - 1]
                    .and_then(BatchTag::parse)
                    .is_some_and(|member| {
                        member.id == batch.id
                            && member.position == position
                            && member.size == batch.size
                    })
            });
        if !whole {
            return Err(format!(
                "batch {} merges whole: its {} members, in order, in one push",
                batch.id, batch.size
            ));
        }
        roles.extend(std::iter::repeat_n(Role::Member, batch.size - 1));
        roles.push(Role::Tip { first: at });
        at += batch.size;
    }
    Ok(roles)
}

#[cfg(test)]
mod tests {
    use super::{
        BatchTag, Bisect, Fold, Footprint, Queued, Role, failure_rate, fold, plan_batch, predict,
        roles, tree_of,
    };
    use crate::git::Git;

    /// A repository with a base on `main` and three revisions on it, each adding one file.
    fn three_on_a_base() -> (tempfile::TempDir, Git, String, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let git = Git::new(dir.path());
        let run = |args: &[&str]| git.run(args).unwrap();
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "user.email", "t@t.invalid"]);
        std::fs::create_dir_all(dir.path().join(".bay")).unwrap();
        std::fs::write(
            dir.path().join(".bay/policy.toml"),
            "revision = 1\nunchecked = true\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "base"]);
        let base = run(&["rev-parse", "HEAD"]);
        let mut revisions = Vec::new();
        for name in ["b", "c", "d"] {
            run(&["checkout", "-q", "--detach", &base]);
            std::fs::write(dir.path().join(format!("{name}.txt")), "x\n").unwrap();
            run(&["add", "."]);
            run(&["commit", "-q", "-m", name]);
            revisions.push(run(&["rev-parse", "HEAD"]));
        }
        (dir, git, base, revisions)
    }

    fn queued(seq: u64, revision: &str, fails_on: Vec<String>) -> Queued {
        Queued {
            seq,
            change: format!("change{seq}"),
            op_id: format!("change{seq}.1"),
            revision: revision.to_owned(),
            live: true,
            batchable: true,
            checked: None,
            fails_on,
        }
    }

    /// A fleet finding (2026-10-26): an entry whose pre-check failed on its predicted base went
    /// into the batch, turned it red and cost a bisection, and the pre-checks behind it ran on a
    /// base that counted it as landed. On that base it is passed over: the batch and the
    /// prediction go on without it.
    #[test]
    fn an_entry_known_to_fail_on_its_base_is_passed_over() {
        let (_dir, git, trunk, revisions) = three_on_a_base();
        let policy = crate::policy::load_at(&git, &trunk).unwrap().unwrap();
        let Fold::Applied { commit, .. } = fold(&git, &policy, &trunk, &revisions[0]).unwrap()
        else {
            panic!("the head folds");
        };
        let red_on = tree_of(&git, &commit).unwrap();
        let head = queued(1, &revisions[0], Vec::new());
        let behind = [
            queued(2, &revisions[1], vec![red_on]),
            queued(3, &revisions[2], Vec::new()),
        ];
        let members = plan_batch(&git, &policy, &trunk, &head, &behind, 8, false).unwrap();
        let seqs: Vec<u64> = members.iter().map(|member| member.seq).collect();
        assert_eq!(seqs, [1, 3]);
        let base = predict(&git, &policy, &trunk, &[head, behind[0].clone()])
            .unwrap()
            .unwrap();
        let files = git.run(&["ls-tree", "--name-only", &base]).unwrap();
        assert!(files.contains("b.txt"), "{files}");
        assert!(!files.contains("c.txt"), "{files}");
    }

    fn globs(globs: &[&str]) -> Footprint {
        Footprint::Globs(globs.iter().map(|glob| (*glob).to_owned()).collect())
    }

    /// NEW-10: two footprints are disjoint only when their globs' literal prefixes show it; a
    /// footprint that requires no check shares nothing, and the whole tree overlaps the rest.
    #[test]
    fn footprints_are_disjoint_only_when_their_prefixes_show_it() {
        assert!(!globs(&["ui/"]).overlaps(&globs(&["api/"])));
        assert!(!globs(&["ui/**/*.rs", "docs/"]).overlaps(&globs(&["api/", "lib/x.rs"])));
        assert!(globs(&["src/"]).overlaps(&globs(&["src/a/"])));
        assert!(globs(&["src/**/*.rs"]).overlaps(&globs(&["src/a.rs"])));
        assert!(globs(&["**/*.rs"]).overlaps(&globs(&["api/"])));
        assert!(globs(&["ap*"]).overlaps(&globs(&["api/"])));
        assert!(Footprint::Whole.overlaps(&globs(&["api/"])));
        assert!(Footprint::Whole.overlaps(&Footprint::Whole));
        assert!(!Footprint::Whole.overlaps(&globs(&[])));
        assert!(!globs(&[]).overlaps(&Footprint::Whole));
    }

    /// A batch of `size` whose first failing member is `culprit` (none: all pass), searched:
    /// the prefixes checked, and the largest passing prefix found.
    fn search(size: usize, culprit: Option<usize>) -> (Vec<usize>, usize, Option<usize>) {
        let mut bisect = Bisect::new(size);
        let mut checked = Vec::new();
        while let Some(prefix) = bisect.next_prefix() {
            checked.push(prefix);
            bisect.record(prefix, culprit.is_none_or(|culprit| prefix < culprit));
        }
        (checked, bisect.passed(), bisect.culprit())
    }

    /// The whole batch is checked first; a failure halves the range until the first failing
    /// member is found, each prefix checked at most once.
    #[test]
    fn a_bisection_finds_the_first_failing_member_in_logarithmic_checks() {
        assert_eq!(search(8, None), (vec![8], 8, None));
        assert_eq!(search(4, Some(2)), (vec![4, 2, 1], 1, Some(2)));
        assert_eq!(search(4, Some(1)), (vec![4, 2, 1], 0, Some(1)));
        assert_eq!(search(8, Some(8)), (vec![8, 4, 6, 7], 7, Some(8)));
        assert_eq!(search(2, Some(2)), (vec![2, 1], 1, Some(2)));
        for size in 2..=16 {
            for culprit in 1..=size {
                let (checked, passed, found) = search(size, Some(culprit));
                assert_eq!(
                    (passed, found),
                    (culprit - 1, Some(culprit)),
                    "{size} {culprit}"
                );
                let mut unique = checked.clone();
                unique.sort_unstable();
                unique.dedup();
                assert_eq!(unique.len(), checked.len(), "no prefix twice: {checked:?}");
                assert!(
                    checked.len() <= 2 + usize::try_from(size.ilog2()).unwrap(),
                    "{checked:?}"
                );
            }
        }
    }

    /// The failure rate counts landings and failures alike, and none is no failure.
    #[test]
    fn the_failure_rate_is_the_share_of_outcomes_that_did_not_land() {
        assert!(failure_rate([]).abs() < f64::EPSILON);
        assert!((failure_rate(["published", "check_failed"]) - 0.5).abs() < f64::EPSILON);
        assert!(failure_rate(["published"; 3]).abs() < f64::EPSILON);
    }

    /// A `Bay-Batch` trailer names its batch, position and size, and a batch lands whole: all
    /// its members, consecutive and in order, in one push.
    #[test]
    fn a_batch_lands_whole_and_in_order() {
        let tag = BatchTag {
            id: "0123abcd".to_owned(),
            position: 2,
            size: 3,
        };
        assert_eq!(tag.trailer(), "Bay-Batch: 0123abcd 2/3\n");
        assert_eq!(BatchTag::parse("0123abcd 2/3"), Some(tag));
        for bad in [
            "0123abcd 0/3",
            "0123abcd 4/3",
            "0123abcd 1/1",
            "xyz 1/2",
            "0123abcd",
            "",
        ] {
            assert_eq!(BatchTag::parse(bad), None, "{bad}");
        }
        let pushed = [
            None,
            Some("aa 1/2"),
            Some("aa 2/2"),
            None,
            Some("bb 1/2"),
            Some("bb 2/2"),
        ];
        assert_eq!(
            roles(&pushed).unwrap(),
            vec![
                Role::Alone,
                Role::Member,
                Role::Tip { first: 1 },
                Role::Alone,
                Role::Member,
                Role::Tip { first: 4 },
            ]
        );
        for partial in [
            vec![Some("aa 1/2")],
            vec![Some("aa 2/2")],
            vec![Some("aa 2/2"), Some("aa 1/2")],
            vec![Some("aa 1/2"), None, Some("aa 2/2")],
            vec![Some("aa 1/2"), Some("bb 2/2")],
            vec![Some("aa 1/2 x")],
        ] {
            assert!(roles(&partial).is_err(), "{partial:?}");
        }
    }
}
