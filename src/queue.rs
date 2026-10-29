//! The publication queue (`DESIGN` § 6): no livelock, and no shared tip to contend on.
//!
//! Each reservation is one create-only ref `refs/bay/queue/<seq>` whose commit names the
//! operation; each release is one create-only ref `refs/bay/queue-done/<seq>`. The reservation
//! ref is the sequence claim: a joiner creates the next number, and if another joiner took it
//! first the create fails and it tries the next one. The head is the lowest reservation with no
//! release. The head publishes with one atomic push of trunk, its release and the landed
//! marker; every other outcome of the head creates its release at once.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde_json::{Value, json};

use crate::change::RECORD_IDENTITY;
use crate::git::Git;
use crate::repo::Repo;
use crate::signing::Signer;

pub const QUEUE_PREFIX: &str = crate::names::QUEUE;
pub const DONE_PREFIX: &str = crate::names::QUEUE_DONE;

/// The release reason of a head that could not start its check because no worker answered
/// (`CHECK_UNAVAILABLE`, `DESIGN` § 6). It counts toward neither `QUEUE_DEADLINE_EXCEEDED`
/// nor `MERGE_RETRY_EXHAUSTED`.
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
#[derive(Clone, Debug, Default)]
pub struct Queue {
    pub entries: BTreeMap<u64, Entry>,
    pub released: BTreeSet<u64>,
    /// The reason each release names (`published`, `deadline`, `check_failed`, …).
    pub release_reasons: BTreeMap<u64, String>,
    /// Each release's commit and its committer time in Unix seconds, for retention (§ 10a).
    pub release_commits: BTreeMap<u64, (String, u64)>,
}

/// The JSON `file` at `commit`: `None` when the commit holds none or it is not JSON. G104b: a
/// file the commit holds that cannot be read is an error naming it, never a blank entry.
fn entry_json(git: &Git, commit: &str, file: &str) -> Result<Option<Value>, String> {
    Ok(git
        .file_at(commit, file)
        .map_err(|error| error.message())?
        .and_then(|text| serde_json::from_str(&text).ok()))
}

/// One queue ref as `for-each-ref` lists it.
struct Listed {
    seq: u64,
    commit: String,
    committed: u64,
    release: bool,
}

impl Listed {
    const fn file(&self) -> &'static str {
        if self.release {
            "release.json"
        } else {
            "entry.json"
        }
    }
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

    /// [`Queue::read`], or the queue refs this store holds when the pass read them within
    /// [`crate::repo::READ_REUSE`] (G438).
    ///
    /// # Errors
    ///
    /// A git fault.
    pub fn read_shared(repo: &Repo) -> Result<Self, String> {
        if repo.reuses(crate::repo::Shared::Queue) {
            Self::local(&repo.git)
        } else {
            Self::read(repo)
        }
    }

    /// Reads the queue refs this repository holds. G200b: one `for-each-ref` lists them and
    /// one `git cat-file --batch` reads every entry and release, however long the queue's
    /// history; a file the batch cannot give is read on its own ([`Git::file_at`]), which
    /// tells a commit that holds none from a blob that cannot be read.
    ///
    /// # Errors
    ///
    /// A git fault.
    pub fn local(git: &Git) -> Result<Self, String> {
        let mut queue = Self::default();
        let refs = git
            .run(&[
                "for-each-ref",
                "--format=%(objectname) %(committerdate:unix) %(refname)",
                QUEUE_PREFIX,
                DONE_PREFIX,
            ])
            .map_err(|error| error.message())?;
        let listed: Vec<Listed> = refs
            .lines()
            .filter_map(|line| {
                let mut words = line.splitn(3, ' ');
                let commit = words.next()?.to_owned();
                let committed: u64 = words.next()?.parse().unwrap_or(0);
                let name = words.next()?;
                let (release, seq) = match name.strip_prefix(QUEUE_PREFIX) {
                    Some(seq) => (false, seq),
                    None => (true, name.strip_prefix(DONE_PREFIX)?),
                };
                Some(Listed {
                    seq: seq.parse().ok()?,
                    commit,
                    committed,
                    release,
                })
            })
            .collect();
        let names: Vec<String> = listed
            .iter()
            .map(|one| format!("{}:{}", one.commit, one.file()))
            .collect();
        let texts = git.objects(&names).map_err(|error| error.message())?;
        for (one, text) in listed.iter().zip(texts) {
            let json = match text {
                Some(text) => serde_json::from_slice(&text).ok(),
                None => entry_json(git, &one.commit, one.file())?,
            };
            if one.release {
                queue.released.insert(one.seq);
                let reason = json
                    .and_then(|release: Value| release["reason"].as_str().map(str::to_owned))
                    .unwrap_or_default();
                queue.release_reasons.insert(one.seq, reason);
                queue
                    .release_commits
                    .insert(one.seq, (one.commit.clone(), one.committed));
            } else {
                let entry = json.unwrap_or(Value::Null);
                queue.entries.insert(
                    one.seq,
                    Entry {
                        seq: one.seq,
                        commit: one.commit.clone(),
                        op_id: entry["op_id"].as_str().unwrap_or_default().to_owned(),
                        change: entry["change"].as_str().unwrap_or_default().to_owned(),
                    },
                );
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

    /// The live reservations ahead of place `seq`, head first (`crate::throughput`).
    #[must_use]
    pub fn ahead_of(&self, seq: u64) -> Vec<&Entry> {
        self.entries
            .values()
            .filter(|entry| entry.seq < seq && !self.released.contains(&entry.seq))
            .collect()
    }

    /// The live reservations behind place `seq`, nearest first (`crate::throughput`).
    #[must_use]
    pub fn behind(&self, seq: u64) -> Vec<&Entry> {
        self.entries
            .values()
            .filter(|entry| entry.seq > seq && !self.released.contains(&entry.seq))
            .collect()
    }

    /// The released places, newest first, each with the reason its release names.
    pub fn releases_newest_first(&self) -> impl Iterator<Item = (&Entry, &str)> + '_ {
        self.release_reasons
            .iter()
            .rev()
            .filter_map(|(seq, reason)| Some((self.entries.get(seq)?, reason.as_str())))
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

/// The most (reservation, release) pairs one retention push removes, so the host's judgement
/// of it stays bounded (§ 10a); a longer history is pruned over later passes.
pub const PRUNE_MAX: usize = 64;

impl Queue {
    /// The places retention may remove (§ 10a): whole (reservation, release) pairs below the
    /// head, never the highest pair, each one whose operation is `finished` and whose release
    /// is at `cutoff` (Unix seconds) or older, lowest first, at most [`PRUNE_MAX`]. A finished
    /// operation is never run or counted again, so its places hold nothing a later judgement
    /// reads: a deadline release of an operation still waiting keeps counting.
    #[must_use]
    pub fn prunable(&self, finished: impl Fn(&Entry) -> bool, cutoff: u64) -> Vec<u64> {
        let head = self.head().map_or(u64::MAX, |head| head.seq);
        let highest = self
            .entries
            .keys()
            .filter(|seq| self.released.contains(seq))
            .max()
            .copied();
        self.entries
            .values()
            .filter(|entry| entry.seq < head && Some(entry.seq) != highest)
            .filter(|entry| {
                self.release_commits
                    .get(&entry.seq)
                    .is_some_and(|(_, released)| *released <= cutoff)
            })
            .filter(|entry| finished(entry))
            .map(|entry| entry.seq)
            .take(PRUNE_MAX)
            .collect()
    }
}

/// Removes the pairs `seqs` of `queue` from the remote in one atomic push, each ref under a
/// lease on the commit `queue` read, then from this repository. Returns the places removed;
/// none when `seqs` is empty.
///
/// # Errors
///
/// The remote refused the push (a lease that no longer holds, or its hook), or a git fault.
pub fn prune(repo: &Repo, queue: &Queue, seqs: &[u64]) -> Result<Vec<u64>, String> {
    let mut leases = Vec::new();
    let mut refspecs = Vec::new();
    let mut local = String::new();
    let mut pruned = Vec::new();
    for seq in seqs {
        let (Some(entry), Some((release, _))) =
            (queue.entries.get(seq), queue.release_commits.get(seq))
        else {
            continue;
        };
        for (name, commit) in [
            (format!("{QUEUE_PREFIX}{seq}"), &entry.commit),
            (format!("{DONE_PREFIX}{seq}"), release),
        ] {
            leases.push(format!("--force-with-lease={name}:{commit}"));
            refspecs.push(format!(":{name}"));
            let _ = writeln!(local, "delete {name} {commit}");
        }
        pruned.push(*seq);
    }
    if pruned.is_empty() {
        return Ok(pruned);
    }
    let mut args = vec!["push", "-q", "--atomic"];
    args.extend(leases.iter().map(String::as_str));
    args.push(repo.remote());
    args.extend(refspecs.iter().map(String::as_str));
    repo.git.run(&args).map_err(|error| {
        format!(
            "the retention of queue places could not be pushed: {}",
            error.message()
        )
    })?;
    let _ = repo
        .git
        .run_with(&["update-ref", "--stdin"], &[], Some(local.as_bytes()));
    Ok(pruned)
}

pub(crate) fn commit(
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
        "{op_id} could not reserve a place in the merge queue"
    ))
}

/// Whether operation `op_id` was requested after operation `than` of the same change: a
/// change numbers its operations `<change id>.<n>` in the order they were requested (G567).
#[must_use]
pub fn newer(op_id: &str, than: &str) -> bool {
    let number = |op_id: &str| {
        op_id
            .rsplit_once('.')
            .and_then(|(_, n)| n.parse::<u64>().ok())
    };
    number(op_id) > number(than)
}

/// Replaces a non-head entry in place with a newer operation of the same change (§ 6), and says
/// whether it did. A place released since the caller read the queue is never replaced (G424, a
/// fleet finding, 2026-10-27: a revision written onto a place released as published was retaken
/// every pass for ten hours); the newer operation joins at its own turn instead. The lease
/// guards only the entry's ref, so its release is read on the remote just before the push.
///
/// # Errors
///
/// The remote refuses it, or a git fault.
pub fn replace(
    repo: &Repo,
    signer: Option<&Signer>,
    entry: &Entry,
    op_id: &str,
) -> Result<bool, String> {
    let released = format!("{DONE_PREFIX}{}", entry.seq);
    let listed = repo
        .git
        .run(&["ls-remote", repo.remote(), &released])
        .map_err(|error| error.message())?;
    if !listed.trim().is_empty() {
        return Ok(false);
    }
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
        .map(|_| true)
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

#[cfg(test)]
mod tests {
    use super::{Entry, Queue};

    /// Places `1..=places`, the first `released` of them released at time 10.
    fn queue(places: u64, released: u64) -> Queue {
        let mut queue = Queue::default();
        for seq in 1..=places {
            queue.entries.insert(
                seq,
                Entry {
                    seq,
                    commit: format!("r{seq}"),
                    op_id: format!("c{seq}.1"),
                    change: format!("c{seq}"),
                },
            );
            if seq <= released {
                queue.released.insert(seq);
                queue.release_commits.insert(seq, (format!("d{seq}"), 10));
            }
        }
        queue
    }

    /// § 10a: retention takes whole pairs below the head, never the highest pair, only of a
    /// finished operation, and only once its release is old enough.
    #[test]
    fn retention_keeps_the_highest_pair_the_head_and_what_is_not_finished() {
        let held = queue(6, 4);
        assert_eq!(held.head().map(|head| head.seq), Some(5));
        assert_eq!(held.prunable(|_| true, 10), vec![1, 2, 3]);
        assert_eq!(held.prunable(|entry| entry.seq != 2, 10), vec![1, 3]);
        assert_eq!(held.prunable(|_| true, 9).len(), 0);
        // With no head, every pair but the highest.
        assert_eq!(queue(3, 3).prunable(|_| true, 10), vec![1, 2]);
    }
}
