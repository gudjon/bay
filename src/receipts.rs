//! Outcome receipts (`DESIGN` § 9).
//!
//! Every failure writes a receipt before the reply: operation, change, actor and session,
//! failing step, reason, detail and the recovery condition. A pending receipt is a local
//! file under `<git-common-dir>/bay/receipts/pending/`, outside every view, so a failure
//! that breaks the view cannot erase its own record. It synchronizes into the change chain
//! (and the remote) on a later bay command; until then `status` lists it with its age. A
//! machine that dies before the sync loses the file, and `status` then shows the outcome as
//! missing, never as success.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::change::{Actor, Chain, change_ref};
use crate::clock::now_rfc3339;
use crate::ids::new_change_id;
use crate::repo::Repo;
use crate::result::BayResult;

pub const RECEIPT_SCHEMA: &str = "bay.receipt/1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub schema: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op_id: Option<String>,
    pub change: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<Actor>,
    pub step: String,
    /// The reason code, or `fault` for an operation fault.
    pub reason: String,
    pub detail: serde_json::Value,
    pub recovery: String,
    pub at: String,
    pub at_unix: u64,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[must_use]
pub fn pending_dir(repo: &Repo) -> PathBuf {
    repo.common_dir.join("bay").join("receipts").join("pending")
}

/// Builds the receipt for a failed or blocked result.
#[must_use]
pub fn from_result(change: &str, actor: Option<Actor>, step: &str, result: &BayResult) -> Receipt {
    let reason = result
        .reason
        .map_or_else(|| "fault".to_owned(), |reason| reason.code().to_owned());
    let recovery = result.retry_when.map_or_else(
        || "fix the cause, then run bay merge again".to_owned(),
        str::to_owned,
    );
    Receipt {
        schema: RECEIPT_SCHEMA.to_owned(),
        id: format!(
            "r-{}",
            new_change_id().unwrap_or_else(|_| unix_now().to_string())
        ),
        op_id: result.op_id.clone(),
        change: change.to_owned(),
        actor,
        step: step.to_owned(),
        reason,
        detail: result.detail.clone().unwrap_or(serde_json::Value::Null),
        recovery,
        at: now_rfc3339(),
        at_unix: unix_now(),
    }
}

/// Writes the receipt as a pending file, durably, before the caller replies.
///
/// # Errors
///
/// The file cannot be written.
pub fn write_pending(repo: &Repo, receipt: &Receipt) -> Result<PathBuf, String> {
    let dir = pending_dir(repo);
    std::fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let path = dir.join(format!("{}.json", receipt.id));
    let tmp = dir.join(format!(".{}.tmp", receipt.id));
    let text = serde_json::to_string_pretty(receipt).map_err(|error| error.to_string())?;
    std::fs::write(&tmp, text).map_err(|error| format!("{}: {error}", tmp.display()))?;
    crate::durable::rename(&tmp, &path).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(path)
}

/// Every pending receipt on this machine, oldest first.
#[must_use]
pub fn pending(repo: &Repo) -> Vec<(PathBuf, Receipt)> {
    let Ok(entries) = std::fs::read_dir(pending_dir(repo)) else {
        return Vec::new();
    };
    let mut receipts: Vec<(PathBuf, Receipt)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            let receipt = serde_json::from_str(&text).ok()?;
            Some((path, receipt))
        })
        .collect();
    receipts.sort_by_key(|(_, receipt)| receipt.at_unix);
    receipts
}

/// Moves pending receipts into their change chains and pushes the chains. A receipt whose
/// chain cannot be updated or pushed stays pending. Returns how many were synchronized.
#[must_use]
pub fn sync(repo: &Repo) -> usize {
    let _ = sync_chains(repo);
    // N-21: receipts are signed only where the registry signs everything.
    let signing = crate::registry::Registry::current(&repo.git)
        .ok()
        .flatten()
        .is_some_and(|registry| registry.signs_all());
    let keys = crate::signing::Keys::load(&repo.git);
    let mut synced = 0;
    for (path, receipt) in pending(repo) {
        let mut chain = Chain::new(&repo.git);
        let Ok((head, mut record)) = chain.read(&receipt.change) else {
            continue;
        };
        // With a registry the receipt is signed: by this machine's executor key, else by its
        // agent key under the change's actor.
        if signing {
            let signer = keys.executor.clone().or_else(|| {
                keys.agent.clone().map(|key| crate::signing::Signer {
                    key,
                    name: record.actor.name.clone(),
                })
            });
            chain.sign_with(signer);
        }
        let mut appended = None;
        if !record.receipts.iter().any(|kept| kept.id == receipt.id) {
            let Ok(drafts) = chain.drafts(&receipt.change) else {
                continue;
            };
            record.receipts.push(receipt.clone());
            let Ok(commit) = chain.append(
                &head,
                &record,
                &drafts[0],
                &format!("receipt {}", receipt.id),
            ) else {
                continue;
            };
            appended = Some(commit);
        }
        let name = change_ref(&receipt.change);
        if repo
            .git
            .run(&["push", "-q", repo.remote(), &format!("{name}:{name}")])
            .is_ok()
        {
            let _ = std::fs::remove_file(&path);
            synced += 1;
        } else if let Some(commit) = appended {
            // G294: the receipt stays pending and is appended again by the next sync; its
            // commit the remote refused leaves the chain ref, so no compare-and-swap on the ref
            // keeps losing to it.
            let _ = repo.git.run(&["update-ref", &name, &head, &commit]);
        }
    }
    synced
}

/// Pushes every local change chain the remote does not hold at its local head: work made
/// while the remote was unreachable (an offline request, a checkpoint) reaches it on the next
/// bay command. A chain only appends, so each push is a fast-forward; a refusal is left for
/// the owner of that chain to resolve, never forced. Returns how many chains were pushed.
///
/// A chain whose latest land request the remote's `refs/bay/requests/<id>` does not name yet
/// takes that ref along in one atomic push (§ 10a): the chain may carry a request another bay
/// process on this box wrote and has not pushed yet, and a remote chain whose latest request
/// the requests ref does not name is one the trunk hook never lets land.
#[must_use]
pub fn sync_chains(repo: &Repo) -> usize {
    let changes = crate::change::CHANGES_PREFIX;
    let requests = crate::names::REQUESTS;
    let Ok(local) = repo.git.run(&[
        "for-each-ref",
        "--format=%(objectname) %(refname)",
        changes,
        requests,
    ]) else {
        return 0;
    };
    let Ok(remote) = repo.git.run(&[
        "ls-remote",
        repo.remote(),
        &format!("{changes}*"),
        &format!("{requests}*"),
    ]) else {
        return 0;
    };
    let mut pushed = 0;
    for line in local.lines() {
        let Some((oid, name)) = line.split_once(' ') else {
            continue;
        };
        let Some(id) = name.strip_prefix(changes) else {
            continue;
        };
        let on_remote = listed(&remote, '\t', name);
        if on_remote == Some(oid) {
            continue;
        }
        // W5: only work the remote lacks. A copy the remote moved past (its tip not under
        // the local one, or not even here) could only be refused.
        if let Some(tip) = on_remote
            && !repo.git.is_ancestor(tip, oid).unwrap_or(false)
        {
            continue;
        }
        let request_ref = format!("{requests}{id}");
        let carried = listed(&local, ' ', &request_ref)
            .filter(|request| listed(&remote, '\t', &request_ref) != Some(*request))
            .filter(|request| latest_request(repo, oid).as_deref() == Some(*request))
            .map(|request| format!("+{request}:{request_ref}"));
        let chain = format!("{oid}:{name}");
        let mut args = vec!["push", "-q"];
        if carried.is_some() {
            args.push("--atomic");
        }
        args.extend([repo.remote(), chain.as_str()]);
        if let Some(carried) = &carried {
            args.push(carried.as_str());
        }
        if repo.git.run(&args).is_ok() {
            pushed += 1;
        }
    }
    pushed
}

/// The object id `listing` (lines of an id, `separator`, a ref name) gives ref `name`.
fn listed<'a>(listing: &'a str, separator: char, name: &str) -> Option<&'a str> {
    listing.lines().find_map(|line| {
        line.split_once(separator)
            .filter(|(_, ref_name)| *ref_name == name)
            .map(|(oid, _)| oid)
    })
}

/// The latest land request the chain state `head` records, if it records one.
fn latest_request(repo: &Repo, head: &str) -> Option<String> {
    Chain::new(&repo.git)
        .record_at(head)
        .ok()?
        .operations
        .iter()
        .rev()
        .find_map(|op| op.request_id.clone())
}

/// Seconds since the receipt was written.
#[must_use]
pub fn age_s(receipt: &Receipt) -> u64 {
    unix_now().saturating_sub(receipt.at_unix)
}

/// Whether `path` is inside `dir`.
#[must_use]
pub fn is_inside(path: &Path, dir: &Path) -> bool {
    path.canonicalize()
        .ok()
        .zip(dir.canonicalize().ok())
        .is_some_and(|(path, dir)| path.starts_with(dir))
}
