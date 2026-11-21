//! A runner's lease on the operation it holds, `refs/bay/lease/<change>` (G787; owner ruling
//! 2026-11-18, "own lease ref").
//!
//! A holder renews its lease once a heartbeat (`[runner] heartbeat_s`) while a check runs. Each
//! beat used to be a commit on the change's history, pushed: on one fleet a change's history held
//! about 4,300 of them, every store that fetched the history took them all, and an agent's own
//! write to its change lost "fetch first" to a beat. A beat now replaces the lease ref in place:
//! one parentless commit whose tree is `lease.json` alone, a `bay.lease/1` record naming the
//! change, the operation, the epoch of its claim and its holder. The beat's time is the commit's.
//! Each beat is pushed alone, by compare-and-swap on the lease the holder last saw, so a holder
//! taken over never writes over its successor's lease. The history moves only on real state, and
//! the claims stay on it: a takeover is still one compare-and-swap of the history (§ 9a).
//!
//! A reader honours a lease only while it is the current holder's: the history names its
//! operation, at its epoch, held by its holder ([`Lease::holds`]). Any other lease is an old one,
//! and reads as none. A holder on a bay before G787 beats the history instead, and its last beat
//! is the history's last state, so one rule reads both: the newer of the two.
//!
//! The holder releases its lease when its visit of the operation ends (`crate::publish`): the ref
//! is deleted, as a lock is. A claimer that never beat deletes an older holder's lease it finds,
//! so a holder that died mid-check leaves its lease only until the next claim of the change.
//!
//! G845 (a fleet finding, 2026-11-21: a head was checked alone for 954 s while nine changes
//! joined the line behind it, and the line's other runner checked nothing): a holder's lease also
//! names the operations its check covers, its own first, and its own alone when it checks alone
//! ([`Holding::checks`]). A runner that does not hold the head plans the next batch on the main
//! that batch predicts ([`batch_of`]), not on a batch the head would form from the line as it
//! stands now. A lease from a holder before G845 names none, and is read as before.
//!
//! A fetch that judges a holder's liveness brings the leases in the same round trip, pruned, so a
//! store's copy is the remote's ([`refspec`], [`refspec_of`]). Under a key registry a beat is
//! signed by the holder's runner key, and the host takes it only from the runner the history names
//! as the holder (`crate::hook`).

use serde::{Deserialize, Serialize};

use crate::change::{OpState, Operation};
use crate::git::{Git, GitError};
use crate::repo::Repo;
use crate::signing::Signer;

pub const LEASE_PREFIX: &str = crate::names::LEASE;
pub const LEASE_SCHEMA: &str = "bay.lease/1";
pub const LEASE_FILE: &str = "lease.json";
/// The largest `lease.json` the host accepts.
pub const LEASE_MAX_BYTES: u64 = 1024;

#[must_use]
pub fn lease_ref(change: &str) -> String {
    format!("{LEASE_PREFIX}{change}")
}

/// Every lease, forced. A fetch carries it with `--prune`, so the store holds the remote's leases
/// and none the remote released.
#[must_use]
pub fn refspec() -> String {
    format!("+{LEASE_PREFIX}*:{LEASE_PREFIX}*")
}

/// [`refspec`] for `change`'s lease alone: change ids have one length, so the pattern names that
/// one ref, and matches nothing when the remote holds none.
#[must_use]
pub fn refspec_of(change: &str) -> String {
    format!("+{LEASE_PREFIX}{change}*:{LEASE_PREFIX}{change}*")
}

/// One `bay.lease/1` record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub schema: String,
    pub change: String,
    pub op_id: String,
    pub epoch: u64,
    pub holder: String,
    /// G845: the operations the holder's check covers, its own first; its own alone when it
    /// checks alone. `None` until it decides, and from a holder before G845.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<Vec<String>>,
}

impl Lease {
    #[must_use]
    pub fn new(change: &str, op_id: &str, epoch: u64, holder: &str) -> Self {
        Self {
            schema: LEASE_SCHEMA.to_owned(),
            change: change.to_owned(),
            op_id: op_id.to_owned(),
            epoch,
            holder: holder.to_owned(),
            batch: None,
        }
    }

    /// Whether this is the lease of `op`'s current holder: the history names this operation, at
    /// this epoch, held by this holder.
    #[must_use]
    pub fn holds(&self, op: &Operation) -> bool {
        self.op_id == op.op_id
            && self.epoch == op.epoch
            && op.owner.as_deref() == Some(self.holder.as_str())
            && matches!(op.state, OpState::Running | OpState::Checked)
    }

    /// Whether this lease is later than `other`: of a later operation of the change, or of the
    /// same one at a later epoch, so its holder took over from `other`'s.
    fn after(&self, other: &Self) -> bool {
        (number(&self.op_id), self.epoch) > (number(&other.op_id), other.epoch)
    }
}

/// The number of operation `op_id` (`<change>.<n>`) in its change.
fn number(op_id: &str) -> u64 {
    op_id
        .rsplit_once('.')
        .and_then(|(_, number)| number.parse().ok())
        .unwrap_or(0)
}

/// The record a lease commit holds; `None` where its tree holds no `bay.lease/1` record.
///
/// # Errors
///
/// The tree names `lease.json` and it cannot be read.
pub fn read(git: &Git, commit: &str) -> Result<Option<Lease>, String> {
    match git.file_at(commit, LEASE_FILE) {
        Ok(text) => Ok(text
            .and_then(|text| serde_json::from_str::<Lease>(&text).ok())
            .filter(|lease| lease.schema == LEASE_SCHEMA)),
        Err(GitError::NotUtf8 { .. }) => Ok(None),
        Err(error) => Err(error.message()),
    }
}

/// The tip of `change`'s lease in this store, if it holds one.
#[must_use]
pub fn tip(git: &Git, change: &str) -> Option<String> {
    git.ref_target(&lease_ref(change)).ok().flatten()
}

/// G845: the operations `op`'s current holder checks together, as its lease of `change` names
/// them in this store: `op` first, and `op` alone when it checks alone. `None` when this store
/// holds no lease of that holder, or the lease names none (a holder still planning, or one
/// before G845).
#[must_use]
pub fn batch_of(git: &Git, change: &str, op: &Operation) -> Option<Vec<String>> {
    let lease = read(git, &tip(git, change)?).ok().flatten()?;
    if !lease.holds(op) {
        return None;
    }
    lease
        .batch
        .filter(|members| members.first() == Some(&op.op_id))
}

/// What a watcher of `change`'s holder sees move (§ 9a): the history's `head` and the lease, as
/// this store holds them. Either one moving shows the holder alive.
#[must_use]
pub fn watched(git: &Git, change: &str, head: &str) -> String {
    match tip(git, change) {
        Some(lease) => format!("{head} {lease}"),
        None => head.to_owned(),
    }
}

/// When `op`'s current holder last beat its lease of `change`, as this store holds it: the lease
/// commit's time, in Unix seconds. `None` when this store holds no lease of that holder.
#[must_use]
pub fn beat_at(git: &Git, change: &str, op: &Operation) -> Option<u64> {
    let tip = tip(git, change)?;
    if !read(git, &tip).ok().flatten()?.holds(op) {
        return None;
    }
    git.run(&["show", "-s", "--format=%ct", &tip])
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Brings `change`'s lease from the remote, pruned: the store then holds the remote's.
fn fetch(repo: &Repo, change: &str) {
    let _ = repo.git.run(&[
        "fetch",
        "-q",
        "--prune",
        "--no-tags",
        repo.remote(),
        &refspec_of(change),
    ]);
}

/// What a holder keeps between its beats, from its claim until its visit ends.
pub struct Holding {
    lease: Lease,
    /// The lease tree, written at the first beat: every beat of one claim holds the same record,
    /// until the holder names the batch it checks (G845).
    tree: Option<String>,
    /// The lease commit this holder last pushed.
    pushed: Option<String>,
    /// A later holder's lease stands: this holder beats no more.
    lost: bool,
}

impl Holding {
    #[must_use]
    pub const fn new(lease: Lease) -> Self {
        Self {
            lease,
            tree: None,
            pushed: None,
            lost: false,
        }
    }

    /// Whether this is the holding of operation `op_id`.
    #[must_use]
    pub fn of(&self, op_id: &str) -> bool {
        self.lease.op_id == op_id
    }

    /// G845: this holder's check covers `members` (operation ids, its own first), and its next
    /// beat says so ([`batch_of`]). A record that would pass [`LEASE_MAX_BYTES`] names none.
    pub fn checks(&mut self, members: Vec<String>) {
        let mut lease = self.lease.clone();
        lease.batch = Some(members);
        let limit = usize::try_from(LEASE_MAX_BYTES).unwrap_or(usize::MAX);
        // The record is written with a newline after it.
        if serde_json::to_string_pretty(&lease).map_or(true, |text| text.len() >= limit) {
            lease.batch = None;
        }
        if lease != self.lease {
            self.lease = lease;
            self.tree = None;
        }
    }

    /// One beat: a new lease commit, pushed alone by compare-and-swap on the lease this store
    /// holds. A lease of a later holder there means this holder was taken over: it beats no
    /// more. A push the remote refused brings the remote's lease, for the next beat to judge.
    /// Best effort: a holder that lost its claim learns it from the history (§ 9a).
    pub fn beat(&mut self, repo: &Repo, signer: Option<&Signer>) {
        if self.lost {
            return;
        }
        let git = &repo.git;
        let name = lease_ref(&self.lease.change);
        let seen = tip(git, &self.lease.change);
        if let Some(seen) = &seen
            && self.pushed.as_ref() != Some(seen)
            && read(git, seen)
                .ok()
                .flatten()
                .is_some_and(|other| other.after(&self.lease))
        {
            self.lost = true;
            return;
        }
        let Ok(commit) = self.commit(git, signer) else {
            return;
        };
        let expected = format!("--force-with-lease={name}:{}", seen.unwrap_or_default());
        if git
            .run(&[
                "push",
                "-q",
                &expected,
                repo.remote(),
                &format!("+{commit}:{name}"),
            ])
            .is_ok()
        {
            let _ = git.run(&["update-ref", &name, &commit]);
            self.pushed = Some(commit);
        } else {
            fetch(repo, &self.lease.change);
        }
    }

    /// The lease commit of this beat: the record's tree, written once, and no parent.
    fn commit(&mut self, git: &Git, signer: Option<&Signer>) -> Result<String, String> {
        let tree = if let Some(tree) = &self.tree {
            tree.clone()
        } else {
            let mut text =
                serde_json::to_string_pretty(&self.lease).map_err(|error| error.to_string())?;
            text.push('\n');
            let blob = git
                .run_with(
                    &["hash-object", "-w", "--stdin"],
                    &[],
                    Some(text.as_bytes()),
                )
                .map_err(|error| error.message())?;
            let entry = format!("100644 blob {blob}\t{LEASE_FILE}\n");
            let tree = git
                .run_with(&["mktree"], &[], Some(entry.as_bytes()))
                .map_err(|error| error.message())?;
            self.tree.insert(tree).clone()
        };
        let mut args: Vec<String> = signer.map(Signer::config_args).unwrap_or_default();
        args.extend(["commit-tree".to_owned(), tree]);
        args.push(
            if signer.is_some() {
                "-S"
            } else {
                "--no-gpg-sign"
            }
            .to_owned(),
        );
        args.extend([
            "-m".to_owned(),
            format!(
                "lease {} epoch {} by {}",
                self.lease.op_id, self.lease.epoch, self.lease.holder
            ),
        ]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        // Unsigned, the committer names the holder.
        let holder = self.lease.holder.as_str();
        let email = format!("{holder}@bay.invalid");
        let envs = [
            ("GIT_AUTHOR_NAME", holder),
            ("GIT_AUTHOR_EMAIL", email.as_str()),
            ("GIT_COMMITTER_NAME", holder),
            ("GIT_COMMITTER_EMAIL", email.as_str()),
        ];
        git.run_with(&args, &envs, None)
            .map_err(|error| error.message())
    }

    /// Releases the lease as the holder's visit ends: its own last beat, or an older holder's
    /// lease this store holds, is deleted from the remote by compare-and-swap, and from this
    /// store. A later holder's lease is left alone.
    pub fn release(&mut self, repo: &Repo) {
        let git = &repo.git;
        let name = lease_ref(&self.lease.change);
        let Some(seen) = tip(git, &self.lease.change) else {
            return;
        };
        let own = self.pushed.take().is_some_and(|pushed| pushed == seen);
        if !own
            && read(git, &seen)
                .ok()
                .flatten()
                .is_none_or(|other| other.after(&self.lease))
        {
            return;
        }
        let _ = git.run(&[
            "push",
            "-q",
            &format!("--force-with-lease={name}:{seen}"),
            repo.remote(),
            &format!(":{name}"),
        ]);
        let _ = git.run(&["update-ref", "-d", &name, &seen]);
    }
}

#[cfg(test)]
mod tests {
    use super::{Holding, Lease};

    #[test]
    fn a_later_operation_or_epoch_is_a_later_holder() {
        let lease = Lease::new("c", "c.2", 3, "a");
        assert!(Lease::new("c", "c.2", 4, "b").after(&lease));
        assert!(Lease::new("c", "c.3", 1, "b").after(&lease));
        assert!(!Lease::new("c", "c.2", 3, "a").after(&lease));
        assert!(!Lease::new("c", "c.1", 9, "b").after(&lease));
        assert!(!Lease::new("c", "c.10", 1, "b").after(&Lease::new("c", "c.11", 1, "a")));
    }

    #[test]
    fn a_holder_names_its_batch_unless_the_record_would_be_too_long() {
        let mut holding = Holding::new(Lease::new("c", "c.1", 1, "a"));
        holding.checks(vec!["c.1".to_owned(), "d.1".to_owned()]);
        assert_eq!(
            holding.lease.batch,
            Some(vec!["c.1".to_owned(), "d.1".to_owned()])
        );
        holding.checks((0..200).map(|n| format!("m{n:07}.1")).collect());
        assert_eq!(holding.lease.batch, None);
    }

    #[test]
    fn a_record_from_before_g845_reads_with_no_batch_and_writes_none() {
        let text = r#"{"schema":"bay.lease/1","change":"c","op_id":"c.1","epoch":1,"holder":"a"}"#;
        let lease: Lease = serde_json::from_str(text).unwrap();
        assert_eq!(lease.batch, None);
        assert!(!serde_json::to_string(&lease).unwrap().contains("batch"));
    }
}
