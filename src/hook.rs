//! The scoped host hook (`DESIGN` § 10a): ref-prefix scoping by key class.
//!
//! Git does not tell a hook which key pushed, so the hook judges a push by the signatures on
//! what it adds. It runs as `pre-receive`, which sees every ref of the push at once and
//! rejects the whole push on any refusal. It applies only on a host with a key registry;
//! without one the host keeps just the append-only trunk (`host.rs`).
//!
//! Judged here: bay ref names (the fixed alphabet), trunk publications, the landed marker,
//! change chains (agent, executor and registry signatures; adoption; no deletion; the
//! retention cut), requests, evidence notes, executor logs, the registry, queue reservations
//! and releases, intent claims, the landed intent, live summary refs, talks (`crate::talk`), and
//! marker compaction into the shard indexes (`crate::compact`): a compacted name is never created
//! again, a marker is deleted only by its compaction, and a shard index only appends.
//!
//! An adopt of another actor's open change is judged by the host's own clock (ruling R1): the
//! chain head's age is the age of its newest entry in the host reflog, and the last heartbeat
//! of the change's actor is the newest entry of a live ref's host reflog with a fresh row of the
//! change; both must be at least the takeover window (`[executor] takeover_s` on trunk) old.

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

struct Update {
    old: String,
    new: String,
    name: String,
}

fn is_zero(oid: &str) -> bool {
    !oid.is_empty() && oid.bytes().all(|byte| byte == b'0')
}

/// The most live refs a host holds and an adopt check reads (§ 10a: the hook's work is bounded).
const LIVE_REFS_MAX: usize = 256;
/// The most host reflog entries of one live ref an adopt check reads. An honest machine pushes
/// at most one summary per heartbeat, so this covers a takeover window of 64 heartbeats.
const LIVE_REFLOG_MAX: usize = 64;

/// Whether `git ls-tree -l` of a live commit lists `live.json` alone, a blob of at most
/// [`crate::live::LIVE_MAX_BYTES`].
fn live_tree_is_small(tree: &str) -> bool {
    let mut lines = tree.lines();
    let (Some(line), None) = (lines.next(), lines.next()) else {
        return false;
    };
    let Some((entry, path)) = line.split_once('\t') else {
        return false;
    };
    let fields: Vec<&str> = entry.split_whitespace().collect();
    path == crate::live::LIVE_FILE
        && fields.len() == 4
        && fields[0] == "100644"
        && fields[1] == "blob"
        && fields[3]
            .parse::<u64>()
            .is_ok_and(|size| size <= crate::live::LIVE_MAX_BYTES)
}

fn message(error: &GitError) -> String {
    error.message()
}

/// Judges one push. `input` is pre-receive's stdin: `<old> <new> <ref>` per line.
///
/// # Errors
///
/// The refusal, naming the ref and the rule.
pub fn pre_receive(git: &Git, input: &str) -> Result<(), String> {
    let Some(registry) = Registry::current(git)? else {
        return Ok(());
    };
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
    let trunk = format!(
        "refs/heads/{}",
        git.run(&["config", "--get", "bay.trunk"])
            .unwrap_or_else(|_| "main".to_owned())
    );
    // § 10a: one push's compaction is bounded, so its judgement stays cheap.
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
        } else if let Some(id) = name.strip_prefix("refs/bay/landed/") {
            judge.landed(update, id, &trunk)
        } else if let Some(id) = name.strip_prefix("refs/bay/changes/") {
            judge.chain(update, id)
        } else if let Some(id) = name.strip_prefix("refs/bay/requests/") {
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
        } else if let Some(claim) = name.strip_prefix("refs/bay/intent/") {
            judge.intent(update, claim)
        } else if let Some(claim) = name.strip_prefix("refs/bay/landed-intent/") {
            judge.landed_intent(update, claim, &trunk)
        } else if let Some(machine) = name.strip_prefix(crate::live::LIVE_PREFIX) {
            judge.live(update, machine)
        } else if let Some(talk) = name.strip_prefix(crate::talk::TALK_PREFIX) {
            judge.talk(update, talk)
        } else {
            Ok(())
        };
        verdict.map_err(|why| format!("{name}: {why}"))?;
    }
    Ok(())
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
}

impl Judge<'_> {
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
        if let Some(id) = update.name.strip_prefix("refs/bay/landed/") {
            return self.cut_landing(id, None);
        }
        let (claim, publication) = if let Some(claim) = update.name.strip_prefix("refs/bay/intent/")
        {
            let publication = self
                .marker(&format!("refs/bay/landed-intent/{claim}"))?
                .ok_or("a claim is compacted only once its generation landed")?;
            (claim, publication)
        } else if let Some(claim) = update.name.strip_prefix("refs/bay/landed-intent/") {
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
        let refused = || format!("change {id} has not landed with its chain cut by retention");
        let chain = format!("refs/bay/changes/{id}");
        crate::names::bay_ref(&chain).map_err(|_| refused())?;
        if self.marker(&format!("refs/bay/landed/{id}"))?.is_none() {
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
        if !is_zero(&update.old)
            && self
                .git
                .run(&["merge-base", "--is-ancestor", &update.old, &update.new])
                .is_err()
        {
            return Err("a shard index takes only a fast-forward".to_owned());
        }
        if !matches!(
            self.signer(&update.new, &[KeyClass::Executor])?,
            Some((KeyClass::Executor, _))
        ) {
            return Err("a shard index commit is signed by an executor key".to_owned());
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
        // § 6: while a reservation is live only the head publishes, releasing itself in the
        // same transaction.
        let queue = crate::queue::Queue::local(self.git)?;
        if let Some(head) = queue.head() {
            let release = format!("{}{}", crate::queue::DONE_PREFIX, head.seq);
            let releases_head = self
                .updates
                .iter()
                .any(|other| other.name == release && is_zero(&other.old));
            let change = self.trailer(&update.new, "Change-Id")?;
            if !releases_head || head.change != change {
                return Err(format!(
                    "QUEUE_HELD: the publication queue is held by {} (place {})",
                    head.op_id, head.seq
                ));
            }
        }
        let range = if is_zero(&update.old) {
            update.new.clone()
        } else {
            format!("{}..{}", update.old, update.new)
        };
        let commits = self
            .git
            .run(&["rev-list", "--reverse", &range])
            .map_err(|error| message(&error))?;
        for commit in commits.lines() {
            self.publication(commit)?;
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

    /// One new trunk commit: an executor-signed publication of a verified, current request,
    /// with passing producer-signed evidence and its landed marker created in this push.
    fn publication(&self, commit: &str) -> Result<(), String> {
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
                "{commit} is not a publication: trunk takes only commits signed by an executor key"
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
                "{commit} publishes {revision}, but its request names {}",
                request.revision
            ));
        }
        // No newer request of the change names another revision (§ 10a).
        let latest = self
            .after(&format!("refs/bay/requests/{change}"))?
            .ok_or_else(|| format!("change {change} has no requests ref"))?;
        let latest = self.request(&latest, &change)?;
        if latest.revision != revision {
            return Err(format!(
                "{commit} publishes a revision a newer request superseded"
            ));
        }
        // The operation is not cancelled.
        if let Some(head) = self.after(&format!("refs/bay/changes/{change}"))? {
            let record = Chain::new(self.git)
                .record_at(&head)
                .map_err(|error| error.message())?;
            if record
                .operations
                .iter()
                .any(|op| op.revision == revision && op.state == OpState::Superseded)
                && !record
                    .operations
                    .iter()
                    .any(|op| op.revision == revision && op.state != OpState::Superseded)
            {
                return Err(format!("{commit} publishes a cancelled operation"));
            }
        }
        // The landed marker is created by this very push, and did not exist.
        let marker = format!("refs/bay/landed/{change}");
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
            return Err(format!("change {change} already landed"));
        }
        // § 10a, G33: evidence counts only under the current registry. Evidence that passes
        // under the version C names but not under the current one rests on a producer key the
        // registry revoked since: that is `REGISTRY_CHANGED` too, and the executor rechecks.
        let evidence = self.evidence_passes(commit, request.repair, self.registry);
        let producer_revoked = evidence.is_err()
            && named.commit != self.registry.commit
            && self.evidence_passes(commit, request.repair, &named).is_ok();
        // Refused for a reason of its own, or accepted: never `REGISTRY_CHANGED`.
        if !producer_revoked && (evidence.is_err() || !key_revoked) {
            return evidence;
        }
        let revoked = match (key_revoked, producer_revoked) {
            (true, true) => "its executor key and a producer key behind its evidence are",
            (true, false) => "its executor key is",
            _ => "a producer key behind its evidence is",
        };
        Err(format!(
            "REGISTRY_CHANGED: {commit} was signed under registry {registry_id}, and {revoked} not valid in the current registry {}; an executor with a valid key re-signs it",
            self.registry.commit
        ))
    }

    /// The producer-signed evidence on the commit's tree, counted under `registry`, passes the
    /// policy at its parent (or, for a registry-signed repair, the candidate's own policy).
    fn evidence_passes(
        &self,
        commit: &str,
        repair: bool,
        registry: &Registry,
    ) -> Result<(), String> {
        let judged_by = if repair {
            commit.to_owned()
        } else {
            format!("{commit}^1")
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
        let records = evidence::counted(
            self.git,
            evidence::records_for(self.git, &tree)?,
            Some(registry),
        )?;
        let gaps = evidence::aggregate(
            &records,
            &policy.manifest,
            &policy.manifest_digest,
            &|id| policy.commit_keyed(id),
            commit,
        );
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
            .run(&["cat-file", "blob", &format!("{request_id}:request.json")])
            .map_err(|_| format!("{request_id} is not a request"))?;
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
            return Err("a landed marker is never moved or deleted".to_owned());
        }
        let with_trunk = self.updates.iter().any(|other| {
            other.name == trunk
                && self
                    .git
                    .run(&["merge-base", "--is-ancestor", &update.new, &other.new])
                    .is_ok()
        });
        if !with_trunk {
            return Err(format!(
                "is created only together with the trunk update that lands {id}"
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
        let fast_forward = is_zero(&update.old)
            || self
                .git
                .run(&["merge-base", "--is-ancestor", &update.old, &update.new])
                .is_ok();
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
            let before = parent
                .as_deref()
                .and_then(|parent| chain.record_at(parent).ok())
                .filter(|before| before.actor.name != record.actor.name);
            if let Some(before) = &before {
                if self.marker(&format!("refs/bay/landed/{id}"))?.is_some() {
                    return Err(format!("{commit} adopts a change that already landed"));
                }
                if !matches!(
                    before.state,
                    crate::change::ChangeState::Abandoned | crate::change::ChangeState::Superseded
                ) {
                    self.takeover_window(update, id, parent.as_deref(), &before.actor.name)?;
                }
            }
            if record.state == crate::change::ChangeState::Abandoned
                && self.marker(&format!("refs/bay/landed/{id}"))?.is_some()
            {
                return Err(format!("{commit} abandons a change that already landed"));
            }
            match self.signer(
                &commit,
                &[KeyClass::Agent, KeyClass::Executor, KeyClass::Registry],
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
                        "{commit} is not signed by an agent, executor or registry key"
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
        let name = format!("refs/bay/changes/{id}");
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
            .zip(crate::live::summaries_at(self.git, &tips)?)
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
            let summaries = crate::live::summaries_at(self.git, &commits)?;
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
        let summary = crate::live::summary_at(self.git, &update.new)
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
            let summaries = crate::live::summaries_at(self.git, &tips)?;
            if let Some((other, _)) = names.iter().zip(&summaries).find(|(_, summary)| {
                summary
                    .as_ref()
                    .is_some_and(|summary| summary["key"] == key.as_str())
            }) {
                return Err(format!(
                    "agent {signer}'s key already holds {other}; one key publishes one machine's live ref"
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

    /// A talk chain (`crate::talk::judge`).
    fn talk(&self, update: &Update, talk: &str) -> Result<(), String> {
        let (who, id) = talk
            .split_once('/')
            .ok_or_else(|| "a talk ref is refs/bay/talk/<who>/<id>".to_owned())?;
        let after = |name: &str| self.after(name);
        let push = crate::talk::Push {
            git: self.git,
            registry: self.registry,
            after: &after,
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
            self.signer(&update.new, &[KeyClass::Executor])?,
            Some((KeyClass::Executor, _))
        ) {
            return Err("a retention cut is signed by an executor key".to_owned());
        }
        // § 10a: only a landed change, or one whose chain (before the cut) is abandoned.
        let landed = self.marker(&format!("refs/bay/landed/{id}"))?.is_some();
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
        let latest = self.after(&format!("refs/bay/requests/{id}"))?;
        // A landed change always has a land request; an abandoned one may never have had one.
        let keeps_latest = latest.as_deref().is_some_and(|latest| {
            parents
                .split_whitespace()
                .skip(1)
                .any(|parent| parent == latest)
        });
        if !keeps_latest && (landed || latest.is_some()) {
            return Err("a retention cut keeps the latest land request".to_owned());
        }
        Ok(())
    }

    fn request_ref(&self, update: &Update, id: &str) -> Result<(), String> {
        if is_zero(&update.new) {
            return Err("a requests ref is never deleted".to_owned());
        }
        let text = self
            .git
            .run(&["cat-file", "blob", &format!("{}:request.json", update.new)])
            .map_err(|_| format!("{} is not a request", update.new))?;
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

    /// A producer's notes ref: every record it adds is signed by that producer, under its name.
    fn evidence(&self, update: &Update, producer: &str) -> Result<(), String> {
        if is_zero(&update.new) {
            return Err("evidence is never deleted".to_owned());
        }
        let producers = self.registry.class_file(self.git, KeyClass::Producer)?;
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
            let before: Vec<evidence::EvidenceRecord> = if is_zero(&update.old) {
                Vec::new()
            } else {
                self.git
                    .run(&["cat-file", "blob", &format!("{}:{path}", update.old)])
                    .ok()
                    .and_then(|text| serde_json::from_str(&text).ok())
                    .unwrap_or_default()
            };
            let text = self
                .git
                .run(&["cat-file", "blob", &format!("{}:{path}", update.new)])
                .map_err(|error| message(&error))?;
            let records: Vec<evidence::EvidenceRecord> =
                serde_json::from_str(&text).map_err(|error| format!("{path}: {error}"))?;
            // A note's path is the object it annotates, in fan-out form: a record sits only on
            // the note of the tree it is about, never copied onto another's.
            let annotated: String = path.chars().filter(|c| *c != '/').collect();
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
                if !record.verifies(&producers) {
                    return Err(format!(
                        "record {} is not signed by producer {producer}'s key",
                        record.id
                    ));
                }
            }
        }
        Ok(())
    }

    fn log(&self, update: &Update, executor: &str) -> Result<(), String> {
        if is_zero(&update.new) {
            return Err("an executor log is never deleted".to_owned());
        }
        let range = if is_zero(&update.old) {
            update.new.clone()
        } else {
            if self
                .git
                .run(&["merge-base", "--is-ancestor", &update.old, &update.new])
                .is_err()
            {
                return Err("an executor log only appends".to_owned());
            }
            format!("{}..{}", update.old, update.new)
        };
        let commits = self
            .git
            .run(&["rev-list", &range])
            .map_err(|error| message(&error))?;
        for commit in commits.lines() {
            match self.signer(commit, &[KeyClass::Executor])? {
                Some((_, principal)) if principal == executor => {}
                _ => return Err(format!("{commit} is not signed by executor {executor}")),
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
        if self
            .git
            .run(&["merge-base", "--is-ancestor", &update.old, &update.new])
            .is_err()
        {
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
            Registry::load_at(self.git, commit)?;
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
            return Err("a reservation is removed only with its release, by retention".to_owned());
        }
        if !matches!(
            self.signer(&update.new, &[KeyClass::Executor])?,
            Some((KeyClass::Executor, _))
        ) {
            return Err("a reservation is signed by an executor key".to_owned());
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
        if queue.head().is_some_and(|head| head.seq == seq) || queue.released.contains(&seq) {
            return Err("only a queued entry that is not the head is replaced in place".to_owned());
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
    /// next generation only after the previous one's change landed (§ 4).
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
            other.name == format!("refs/bay/changes/{}", record.id)
                && is_zero(&other.old)
                && other.new == update.new
        });
        if !matches || !with_change {
            return Err(
                "an intent claim is created in the same push as the change that holds it"
                    .to_owned(),
            );
        }
        if generation > 1 && !self.generation_landed(hash, generation - 1)? {
            return Err(format!(
                "generation {generation} needs generation {} landed",
                generation - 1
            ));
        }
        Ok(())
    }

    /// Whether generation `generation` of the intent `hash` landed. A compacted claim was
    /// compacted only with its landed claim (`compacted_delete`), so its landed claim's
    /// publication names the change, which carries its landed marker (§ 10a).
    fn generation_landed(&self, hash: &str, generation: u64) -> Result<bool, String> {
        let claim = format!("refs/bay/intent/{hash}/{generation}");
        if let Some(previous) = self.after(&claim)? {
            let before = Chain::new(self.git)
                .record_at(&previous)
                .map_err(|error| error.message())?;
            return Ok(self
                .marker(&format!("refs/bay/landed/{}", before.id))?
                .is_some());
        }
        if self.recorded(&claim)?.is_none() {
            return Err("the previous generation has no claim".to_owned());
        }
        let Some(publication) =
            self.marker(&format!("refs/bay/landed-intent/{hash}/{generation}"))?
        else {
            return Ok(false);
        };
        let change = self.trailer(&publication, "Change-Id")?;
        if crate::names::bay_ref(&format!("refs/bay/landed/{change}")).is_err() {
            return Ok(false);
        }
        Ok(self.marker(&format!("refs/bay/landed/{change}"))?.is_some())
    }

    /// A landed-intent marker: created once, with the trunk update that lands its generation,
    /// whose change held that claim (§ 6: two changes may hold one generation; the one that
    /// lands names it).
    fn landed_intent(&self, update: &Update, claim: &str, trunk: &str) -> Result<(), String> {
        if !is_zero(&update.old) {
            return Err("this generation of the intent already landed".to_owned());
        }
        let with_trunk = self
            .by_name
            .get(trunk)
            .is_some_and(|other| other.new == update.new);
        if !with_trunk {
            return Err("is created only together with the trunk update that lands it".to_owned());
        }
        let change = self.trailer(&update.new, "Change-Id")?;
        let chain = format!("refs/bay/changes/{change}");
        let names_holder =
            || format!("is created only by the landing of a change that holds {claim}");
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

    /// A release: created once, executor-signed, for a reservation that exists.
    fn queue_release(&self, update: &Update, seq: &str) -> Result<(), String> {
        if !is_zero(&update.old) || is_zero(&update.new) {
            return Err("a release is created once and never moved".to_owned());
        }
        if !matches!(
            self.signer(&update.new, &[KeyClass::Executor])?,
            Some((KeyClass::Executor, _))
        ) {
            return Err("a release is signed by an executor key".to_owned());
        }
        let reserved = format!("{}{seq}", crate::queue::QUEUE_PREFIX);
        if self.after(&reserved)?.is_none() {
            return Err("releases no reservation".to_owned());
        }
        Ok(())
    }
}

struct Request {
    revision: String,
    repair: bool,
}
