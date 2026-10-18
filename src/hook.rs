//! The scoped host hook (`DESIGN` § 10a): ref-prefix scoping by key class.
//!
//! Git does not tell a hook which key pushed, so the hook judges a push by the signatures on
//! what it adds. It runs as `pre-receive`, which sees every ref of the push at once and
//! rejects the whole push on any refusal. It applies only on a host with a key registry;
//! without one the host keeps just the append-only trunk (`host.rs`).
//!
//! Built here: trunk publications, the landed marker, change chains (agent, executor and
//! registry signatures; no deletion; the retention cut), requests, evidence notes, executor
//! logs and the registry. The queue (item 19d), intent claims and adoption (19e) and the full
//! name validation (19f) add their own rules.

use std::collections::HashSet;

use serde_json::Value;

use crate::change::{Chain, OpState, RECORD_FILE};
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
    let judge = Judge {
        git,
        registry: &registry,
        updates: &updates,
    };
    for update in &updates {
        let name = update.name.as_str();
        // § 10a: every bay ref name is validated first: ids from the fixed alphabet, operator
        // names short and lower-case, so no bay ref can be a directory of another.
        crate::names::bay_ref(name)?;
        let verdict = if name == trunk {
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
        } else if name.starts_with("refs/bay/landed-intent/") {
            judge.landed_intent(update, &trunk)
        } else {
            Ok(())
        };
        verdict.map_err(|why| format!("{name}: {why}"))?;
    }
    Ok(())
}

struct Judge<'a> {
    git: &'a Git,
    registry: &'a Registry,
    updates: &'a [Update],
}

impl Judge<'_> {
    /// A ref's value after this push: its new value if the push moves it, else its current one.
    fn after(&self, name: &str) -> Result<Option<String>, String> {
        if let Some(update) = self.updates.iter().find(|update| update.name == name) {
            return Ok((!is_zero(&update.new)).then(|| update.new.clone()));
        }
        self.git.ref_target(name).map_err(|error| message(&error))
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
        if change.is_empty()
            || !crate::publish::verify_publication(self.git, commit, &registry_id, &committer)
        {
            return Err(format!(
                "{commit} is not a publication: trunk takes only commits signed by an executor key"
            ));
        }
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
        {
            return Err(format!("change {change} already landed"));
        }
        self.evidence_passes(commit, request.repair)
    }

    /// The producer-signed evidence on the commit's tree passes the policy at its parent (or,
    /// for a registry-signed repair, the candidate's own policy).
    fn evidence_passes(&self, commit: &str, repair: bool) -> Result<(), String> {
        let judged_by = if repair {
            commit.to_owned()
        } else {
            format!("{commit}^1")
        };
        let Some(policy) = policy::load_at(self.git, &judged_by)? else {
            return Ok(());
        };
        let tree = self
            .git
            .run(&["rev-parse", &format!("{commit}^{{tree}}")])
            .map_err(|error| message(&error))?;
        let records = evidence::counted(
            self.git,
            evidence::records_for(self.git, &tree)?,
            Some(self.registry),
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
    fn request(&self, request_id: &str, change: &str) -> Result<Request, String> {
        let text = self
            .git
            .run(&["cat-file", "blob", &format!("{request_id}:request.json")])
            .map_err(|_| format!("{request_id} is not a request"))?;
        let request: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        if request["change"] != change {
            return Err(format!("request {request_id} is for another change"));
        }
        let named = request["registry"].as_str().map(str::to_owned);
        let registry = match named {
            Some(commit) => Registry::load_at(self.git, &commit)?,
            None => self.registry.clone(),
        };
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
            // An adoption (the actor changes) is accepted only while the change has not landed.
            let parent = self
                .git
                .run(&["rev-parse", "--verify", "-q", &format!("{commit}^1")])
                .ok();
            let adopted = parent
                .and_then(|parent| chain.record_at(&parent).ok())
                .is_some_and(|before| before.actor.name != record.actor.name);
            if adopted && self.after(&format!("refs/bay/landed/{id}"))?.is_some() {
                return Err(format!("{commit} adopts a change that already landed"));
            }
            if record.state == crate::change::ChangeState::Abandoned
                && self.after(&format!("refs/bay/landed/{id}"))?.is_some()
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
        let landed = self.after(&format!("refs/bay/landed/{id}"))?.is_some();
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
        // Judged against the current registry: a request signed by a key already revoked is
        // refused at its push.
        let text = self
            .git
            .run(&["cat-file", "blob", &format!("{}:request.json", update.new)])
            .map_err(|_| format!("{} is not a request", update.new))?;
        let request: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        if request["change"] != id {
            return Err("names a request of another change".to_owned());
        }
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
        if generation > 1 {
            let previous = self
                .after(&format!("refs/bay/intent/{hash}/{}", generation - 1))?
                .ok_or("the previous generation has no claim")?;
            let before = Chain::new(self.git)
                .record_at(&previous)
                .map_err(|error| error.message())?;
            if self
                .after(&format!("refs/bay/landed/{}", before.id))?
                .is_none()
            {
                return Err(format!(
                    "generation {generation} needs generation {} landed",
                    generation - 1
                ));
            }
        }
        Ok(())
    }

    /// A landed-intent marker: created once, with the trunk update that lands its generation.
    fn landed_intent(&self, update: &Update, trunk: &str) -> Result<(), String> {
        if !is_zero(&update.old) {
            return Err("this generation of the intent already landed".to_owned());
        }
        let with_trunk = self
            .updates
            .iter()
            .any(|other| other.name == trunk && other.new == update.new);
        if !with_trunk {
            return Err("is created only together with the trunk update that lands it".to_owned());
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
