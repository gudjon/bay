//! The publication queue (`DESIGN` § 6): no livelock, and no shared tip to contend on.
//!
//! Each reservation is one create-only ref `refs/bay/queue/<seq>` whose commit names the
//! operation; each release is one create-only ref `refs/bay/queue-done/<seq>`. The reservation
//! ref is the sequence claim: a joiner creates the next number, and if another joiner took it
//! first the create fails and it tries the next one. The head is the lowest reservation with no
//! release. The head publishes with one atomic push of trunk, its release and the landed
//! marker; every other outcome of the head creates its release at once.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::change::RECORD_IDENTITY;
use crate::git::Git;
use crate::repo::Repo;
use crate::signing::Signer;

pub const QUEUE_PREFIX: &str = crate::names::QUEUE;
pub const DONE_PREFIX: &str = crate::names::QUEUE_DONE;

/// The release reason of a head that could not start its check because no worker answered
/// (`CHECK_UNAVAILABLE`, `DESIGN` § 6). It counts toward neither `QUEUE_DEADLINE_EXCEEDED`
/// nor `PUBLISH_RETRY_EXHAUSTED`.
pub const UNAVAILABLE: &str = "unavailable";

/// One reservation's entry.
#[derive(Clone, Debug)]
pub struct Entry {
    pub seq: u64,
    pub commit: String,
    pub op_id: String,
    pub change: String,
}

/// The queue as the remote holds it.
#[derive(Debug, Default)]
pub struct Queue {
    pub entries: BTreeMap<u64, Entry>,
    pub released: BTreeSet<u64>,
    /// The reason each release names (`published`, `deadline`, `check_failed`, …).
    pub release_reasons: BTreeMap<u64, String>,
}

fn entry_json(git: &Git, commit: &str, file: &str) -> Option<Value> {
    let text = git
        .run(&["cat-file", "blob", &format!("{commit}:{file}")])
        .ok()?;
    serde_json::from_str(&text).ok()
}

impl Queue {
    /// Reads the queue from the remote (fetching its refs).
    ///
    /// # Errors
    ///
    /// A git fault other than an unreachable remote (then the local copy is read).
    pub fn read(repo: &Repo) -> Result<Self, String> {
        let _ = repo.git.run(&[
            "fetch",
            "-q",
            "--no-tags",
            "--prune",
            repo.remote(),
            &format!("+{QUEUE_PREFIX}*:{QUEUE_PREFIX}*"),
            &format!("+{DONE_PREFIX}*:{DONE_PREFIX}*"),
        ]);
        Self::local(&repo.git)
    }

    /// Reads the queue refs this repository holds.
    ///
    /// # Errors
    ///
    /// A git fault.
    pub fn local(git: &Git) -> Result<Self, String> {
        let mut queue = Self::default();
        let refs = git
            .run(&[
                "for-each-ref",
                "--format=%(objectname) %(refname)",
                QUEUE_PREFIX,
                DONE_PREFIX,
            ])
            .map_err(|error| error.message())?;
        for line in refs.lines() {
            let Some((commit, name)) = line.split_once(' ') else {
                continue;
            };
            if let Some(seq) = name
                .strip_prefix(QUEUE_PREFIX)
                .and_then(|seq| seq.parse().ok())
            {
                let entry = entry_json(git, commit, "entry.json").unwrap_or(Value::Null);
                queue.entries.insert(
                    seq,
                    Entry {
                        seq,
                        commit: commit.to_owned(),
                        op_id: entry["op_id"].as_str().unwrap_or_default().to_owned(),
                        change: entry["change"].as_str().unwrap_or_default().to_owned(),
                    },
                );
            } else if let Some(seq) = name
                .strip_prefix(DONE_PREFIX)
                .and_then(|seq| seq.parse().ok())
            {
                queue.released.insert(seq);
                let reason = entry_json(git, commit, "release.json")
                    .and_then(|release| release["reason"].as_str().map(str::to_owned))
                    .unwrap_or_default();
                queue.release_reasons.insert(seq, reason);
            }
        }
        Ok(queue)
    }

    /// The head: the lowest reservation without a release.
    #[must_use]
    pub fn head(&self) -> Option<&Entry> {
        self.entries
            .values()
            .find(|entry| !self.released.contains(&entry.seq))
    }

    /// The live reservation that names `op_id`.
    #[must_use]
    pub fn of(&self, op_id: &str) -> Option<&Entry> {
        self.entries
            .values()
            .find(|entry| entry.op_id == op_id && !self.released.contains(&entry.seq))
    }

    /// A live reservation of `change` for another operation.
    #[must_use]
    pub fn of_change(&self, change: &str) -> Option<&Entry> {
        self.entries
            .values()
            .find(|entry| entry.change == change && !self.released.contains(&entry.seq))
    }

    /// 1 for the head, 2 for the next, … among live reservations.
    #[must_use]
    pub fn position(&self, seq: u64) -> usize {
        self.entries
            .keys()
            .filter(|key| **key <= seq && !self.released.contains(key))
            .count()
    }

    /// How many times `op_id` was released by a progress deadline, leaving out the places in
    /// `mid_publish`: a release created while the head was publishing is a publish retry, not
    /// a deadline release that counts toward `QUEUE_DEADLINE_EXCEEDED` (`DESIGN` § 6).
    ///
    /// Only a release whose reason is `deadline` counts. A head that could not start its check
    /// because no worker answered is released as [`UNAVAILABLE`], which never counts, so a
    /// worker outage never turns queued work into `QUEUE_DEADLINE_EXCEEDED` (§ 6). Of the
    /// places released mid-publish, only those whose refused push the head's own executor saw
    /// and saved are left out; a head that crashed before saving it still counts that place.
    #[must_use]
    pub fn deadline_releases(&self, op_id: &str, mid_publish: &[u64]) -> usize {
        self.entries
            .values()
            .filter(|entry| entry.op_id == op_id && !mid_publish.contains(&entry.seq))
            .filter(|entry| {
                self.release_reasons
                    .get(&entry.seq)
                    .is_some_and(|reason| reason == "deadline")
            })
            .count()
    }

    /// Whether a place of `op_id` was released with `reason`.
    #[must_use]
    pub fn released_as(&self, op_id: &str, reason: &str) -> bool {
        self.entries.values().any(|entry| {
            entry.op_id == op_id
                && self
                    .release_reasons
                    .get(&entry.seq)
                    .is_some_and(|released| released == reason)
        })
    }
}

fn commit(
    git: &Git,
    signer: Option<&Signer>,
    file: &str,
    body: &Value,
    message: &str,
) -> Result<String, String> {
    let mut text = serde_json::to_string_pretty(body).map_err(|error| error.to_string())?;
    text.push('\n');
    let blob = git
        .run_with(
            &["hash-object", "-w", "--stdin"],
            &[],
            Some(text.as_bytes()),
        )
        .map_err(|error| error.message())?;
    let entry = format!("100644 blob {blob}\t{file}\n");
    let tree = git
        .run_with(&["mktree"], &[], Some(entry.as_bytes()))
        .map_err(|error| error.message())?;
    let mut args: Vec<String> = Vec::new();
    if let Some(signer) = signer {
        args.extend(signer.config_args());
    }
    args.extend(["commit-tree".to_owned(), tree]);
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
    let identity: Vec<(String, String)> = signer.map_or_else(
        || {
            RECORD_IDENTITY
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

/// Joins the queue: creates the next reservation on the remote, trying the next number when
/// another joiner took it. Returns the sequence number.
///
/// # Errors
///
/// The remote refuses every attempt, or a git fault.
pub fn join(
    repo: &Repo,
    signer: Option<&Signer>,
    op_id: &str,
    change: &str,
) -> Result<u64, String> {
    let reservation = commit(
        &repo.git,
        signer,
        "entry.json",
        &json!({"op_id": op_id, "change": change}),
        &format!("reserve {op_id}"),
    )?;
    for _ in 0..20 {
        let queue = Queue::read(repo)?;
        let above = queue
            .entries
            .keys()
            .chain(queue.released.iter())
            .max()
            .map_or(1, |last| last + 1);
        let name = format!("{QUEUE_PREFIX}{above}");
        if repo
            .git
            .run(&[
                "push",
                "-q",
                // G198: a reservation is create-only, so a lease expects no ref.
                &format!("--force-with-lease={name}:"),
                repo.remote(),
                &format!("{reservation}:{name}"),
            ])
            .is_ok()
        {
            let _ = repo.git.run(&["update-ref", &name, &reservation]);
            // Two executors that queue one operation at once both reserve a place. The later
            // one always sees the earlier, since its number is above it, and releases its own,
            // so the operation holds one place (§ 6).
            let queue = Queue::read(repo)?;
            if let Some(earlier) = queue
                .entries
                .values()
                .find(|entry| {
                    entry.op_id == op_id
                        && entry.seq < above
                        && !queue.released.contains(&entry.seq)
                })
                .map(|entry| entry.seq)
            {
                release(repo, signer, above, op_id, "duplicate")?;
                return Ok(earlier);
            }
            return Ok(above);
        }
    }
    Err(format!(
        "{op_id} could not reserve a place in the publication queue"
    ))
}

/// Replaces a non-head entry in place with a newer operation of the same change (§ 6).
///
/// # Errors
///
/// The remote refuses it, or a git fault.
pub fn replace(
    repo: &Repo,
    signer: Option<&Signer>,
    entry: &Entry,
    op_id: &str,
) -> Result<(), String> {
    let reservation = commit(
        &repo.git,
        signer,
        "entry.json",
        &json!({"op_id": op_id, "change": entry.change, "replaces": entry.op_id}),
        &format!("replace {} with {op_id}", entry.op_id),
    )?;
    let name = format!("{QUEUE_PREFIX}{}", entry.seq);
    repo.git
        .run(&[
            "push",
            "-q",
            &format!("--force-with-lease={name}:{}", entry.commit),
            repo.remote(),
            &format!("{reservation}:{name}"),
        ])
        .map(|_| ())
        .map_err(|error| error.message())
}

/// The release commit for `seq`, to create in an atomic push (with a publication) or alone.
///
/// # Errors
///
/// A git fault.
pub fn release_commit(
    git: &Git,
    signer: Option<&Signer>,
    seq: u64,
    op_id: &str,
    reason: &str,
) -> Result<String, String> {
    commit(
        git,
        signer,
        "release.json",
        &json!({"seq": seq, "op_id": op_id, "reason": reason}),
        &format!("release {seq}: {reason}"),
    )
}

/// Releases `seq` on its own: any outcome of the head other than publishing.
///
/// # Errors
///
/// The release already exists (someone released it first is fine and not an error), or a git
/// fault.
pub fn release(
    repo: &Repo,
    signer: Option<&Signer>,
    seq: u64,
    op_id: &str,
    reason: &str,
) -> Result<(), String> {
    let release = release_commit(&repo.git, signer, seq, op_id, reason)?;
    let name = format!("{DONE_PREFIX}{seq}");
    if repo
        .git
        .run(&[
            "push",
            "-q",
            // G198: a release is create-only, so a lease expects no ref.
            &format!("--force-with-lease={name}:"),
            repo.remote(),
            &format!("{release}:{name}"),
        ])
        .is_ok()
    {
        return Ok(());
    }
    // Released by someone else first: the queue moves on either way.
    if Queue::read(repo)?.released.contains(&seq) {
        return Ok(());
    }
    Err(format!(
        "the release of queue entry {seq} could not be pushed"
    ))
}
