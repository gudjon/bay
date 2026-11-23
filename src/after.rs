//! G273 (N-16): a change that lands only after others. `bay start --after <change>`
//! (repeatable) records the changes a change depends on in its record's `after`. The change may
//! be checked and queued at once, but the executor publishes it only once every change it names
//! has landed on trunk: the remote holds that change's landed marker and trunk reaches the
//! commit it names ([`crate::undo::landed_commit`]). Until then the operation waits with
//! `DEPENDENCY_PENDING` and holds no queue place (`crate::publish`). A named change that ended
//! without landing (abandoned, superseded or no effect) can never be followed, so the dependent
//! is refused `DEPENDENCY_ABANDONED`. The landing of the dependent carries one trailer per
//! change it named, `Bay-After: <change>`.
//!
//! Unknown is never landed: a change whose chain the remote does not answer for is pending, with
//! the fault named.
//!
//! BACKLOG 157 (a fleet finding, 2026-10-30: an operation waited on a change that had no merge
//! request, and 39% of one runner's journal lines were that wait, which said only that it waits):
//! a named change with no operation under way is not submitted. The wait names it and its actor,
//! who must `bay merge` it, and the chain head it was read at, which only a request, a landing or
//! an end moves (`crate::executor` visits the wait again then).
//!
//! G869 (a fleet finding, 2026-11-23: the later changes of one landing batch kept their landed
//! records but the remote held no landed marker for them, and a change after one of them waited
//! for ever): a named change the remote holds no marker for, whose record says it landed, is on
//! trunk at its one publication the trunk scan finds ([`crate::undo::unmarked`]). Its marker
//! rides the push that lands the change after it (`crate::publish`), so every other reader of
//! the markers finds it too.
//!
//! G604 (a fleet finding, 2026-10-31: an operation waited 22 minutes on a change whose only
//! operation a runner had blocked, and each of 34 journal lines said only that it waits): a
//! named change whose every open operation is blocked is stopped. It moves again only once a
//! person acts on it (its actor, or an approver for an approval), so the wait names it, its
//! actor, the blocked operation and its reason, and is parked on its chain head like a change
//! that is not submitted.

use serde_json::{Value, json};
use std::fmt::Write as _;

use crate::change::{Chain, ChangeRecord, ChangeState, OpState, Operation};
use crate::repo::Repo;
use crate::result::Reason;

/// The fixed trailer a dependent's trunk commit names each of its dependencies in.
pub const AFTER_TRAILER: &str = "Bay-After";

/// The release reason of a queue place given back because a dependency has not landed. It
/// counts toward neither `QUEUE_DEADLINE_EXCEEDED` nor `MERGE_RETRY_EXHAUSTED` (`DESIGN` § 6).
pub const RELEASE: &str = "dependency_pending";

/// Where one dependency stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Standing {
    /// On trunk, at this commit.
    Landed(String),
    /// On trunk, at this commit, though the remote holds no landed marker for it: its record
    /// says it landed, and the trunk scan finds its one publication (G869).
    Unmarked(String),
    /// Not on trunk yet: its record's state, or why it could not be read.
    Pending(String),
    /// Not on trunk, and not submitted: no operation of it is under way, so it lands only once
    /// its actor runs `bay merge` for it. Its record's state, its actor, and its chain head.
    Unsubmitted {
        state: String,
        actor: String,
        head: String,
    },
    /// Not on trunk, and stopped: every operation of it that has not ended is blocked, so it
    /// lands only once a person acts on it (G604). Its record's state, its actor, its chain
    /// head, and the newest blocked operation with its reason code (none for a fault).
    Blocked {
        state: String,
        actor: String,
        head: String,
        op: String,
        reason: Option<String>,
    },
    /// Ended without landing, in this state.
    Ended(String),
    /// Ended `no_effect`: its work was already on trunk, so a change after it may land.
    Met,
}

/// One dependency of a change, and where it stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dependency {
    pub change: String,
    pub standing: Standing,
}

impl Dependency {
    #[must_use]
    pub fn to_json(&self) -> Value {
        match &self.standing {
            Standing::Landed(commit) => {
                json!({"change": self.change, "state": "landed", "commit": commit})
            }
            Standing::Unmarked(commit) => json!({
                "change": self.change,
                "state": "landed",
                "commit": commit,
                "marker": false,
            }),
            Standing::Pending(state) => json!({"change": self.change, "state": state}),
            Standing::Unsubmitted { state, actor, head } => json!({
                "change": self.change,
                "state": state,
                "submitted": false,
                "actor": actor,
                "head": head,
            }),
            Standing::Blocked {
                state,
                actor,
                head,
                op,
                reason,
            } => json!({
                "change": self.change,
                "state": state,
                "blocked": {"op": op, "reason": reason},
                "actor": actor,
                "head": head,
            }),
            Standing::Ended(state) => {
                json!({"change": self.change, "state": state, "landed": false})
            }
            Standing::Met => json!({"change": self.change, "state": "no_effect", "met": true}),
        }
    }

    /// This dependency as [`Stalled`], when nothing frees it by waiting.
    #[must_use]
    pub fn stalled(&self) -> Option<Stalled> {
        match &self.standing {
            Standing::Unsubmitted { actor, head, .. } => Some(Stalled {
                change: self.change.clone(),
                actor: actor.clone(),
                head: head.clone(),
                blocked: None,
            }),
            Standing::Blocked {
                actor,
                head,
                op,
                reason,
                ..
            } => Some(Stalled {
                change: self.change.clone(),
                actor: actor.clone(),
                head: head.clone(),
                blocked: Some(Block {
                    op: op.clone(),
                    reason: reason.clone(),
                }),
            }),
            _ => None,
        }
    }
}

/// The trailer lines of a landing that comes after `after`: one `Bay-After: <change>` each.
#[must_use]
pub fn trailers(after: &[String]) -> String {
    after.iter().fold(String::new(), |mut lines, change| {
        let _ = writeln!(lines, "{AFTER_TRAILER}: {change}");
        lines
    })
}

/// Whether `state` ends a change without a landing and without its work on trunk: abandoned or
/// superseded. A `no_effect` change ended because its work was already on trunk, so it is met.
#[must_use]
pub const fn ended_unlanded(state: ChangeState) -> bool {
    matches!(state, ChangeState::Abandoned | ChangeState::Superseded)
}

/// Where each of `after` stands against trunk at `trunk` (a commit), in the order given.
///
/// # Errors
///
/// The remote could not be read for a landed marker, so whether a dependency landed is unknown.
pub fn standing(repo: &Repo, after: &[String], trunk: &str) -> Result<Vec<Dependency>, String> {
    let mut out = Vec::new();
    for change in after {
        let marker = crate::undo::marker(repo, change)?;
        let on_trunk = match &marker {
            Some(landed) => crate::undo::marked_commit(repo, change, trunk, landed)?,
            None => None,
        };
        let standing = match on_trunk {
            Some(commit) => Standing::Landed(commit),
            None => unlanded(repo, change, marker.is_none().then_some(trunk)),
        };
        out.push(Dependency {
            change: change.clone(),
            standing,
        });
    }
    Ok(out)
}

/// The standing of `change`, which no landed marker puts on trunk: its record's state, read from
/// the remote. With `unmarked_on`, the trunk the remote holds no marker of it on: a record that
/// says it landed is on that trunk at the publication the scan finds there (G869).
fn unlanded(repo: &Repo, change: &str, unmarked_on: Option<&str>) -> Standing {
    let head = match repo.remote_chain(change) {
        Ok(head) => head,
        Err(error) => {
            return Standing::Pending(format!(
                "unknown: its chain could not be read from the remote: {}",
                error.message()
            ));
        }
    };
    match Chain::new(&repo.git).record_at(&head) {
        Ok(record) => {
            let state = state_word(record.state);
            if let Some(trunk) = unmarked_on {
                match crate::undo::unmarked(repo, &record, change, trunk) {
                    Ok(Some(commit)) => return Standing::Unmarked(commit),
                    Ok(None) => {}
                    Err(error) => {
                        return Standing::Pending(format!(
                            "unknown: trunk could not be scanned for it: {error}"
                        ));
                    }
                }
            }
            if record.state == ChangeState::NoEffect {
                Standing::Met
            } else if ended_unlanded(record.state) {
                Standing::Ended(state)
            } else if let Some(op) = stopped_at(&record) {
                Standing::Blocked {
                    state,
                    actor: record.actor.name.clone(),
                    head,
                    op: op.op_id.clone(),
                    reason: op.outcome.clone(),
                }
            } else if submitted(&record) {
                Standing::Pending(state)
            } else {
                Standing::Unsubmitted {
                    state,
                    actor: record.actor.name,
                    head,
                }
            }
        }
        Err(error) => Standing::Pending(format!(
            "unknown: its record could not be read: {}",
            error.message()
        )),
    }
}

/// Whether `record` has landed or has an operation that has not ended without landing: one under
/// way, blocked, or published. A change whose every operation failed or was superseded, or that
/// has none, waits for its actor's `bay merge`.
fn submitted(record: &ChangeRecord) -> bool {
    record.state == ChangeState::Landed
        || record
            .operations
            .iter()
            .any(|op| !matches!(op.state, OpState::Failed | OpState::Superseded))
}

/// The operation `record`, which has not landed, is stopped at: its newest blocked one, when
/// every operation of it that has not ended is blocked (G604). A blocked operation moves again
/// only once its actor's `bay merge`, an approval or a runner's retry rewrites it, and each of
/// those moves its chain.
fn stopped_at(record: &ChangeRecord) -> Option<&Operation> {
    let open = || {
        record
            .operations
            .iter()
            .filter(|op| !matches!(op.state, OpState::Failed | OpState::Superseded))
    };
    if record.state == ChangeState::Landed || !open().all(|op| op.state == OpState::Blocked) {
        return None;
    }
    open().next_back()
}

/// A dependency a `DEPENDENCY_PENDING` result names as stalled: not submitted
/// ([`Standing::Unsubmitted`]) or stopped ([`Standing::Blocked`]). Nothing frees it by waiting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stalled {
    pub change: String,
    pub actor: String,
    pub head: String,
    /// The operation it is stopped at, or `None` when it is not submitted.
    pub blocked: Option<Block>,
}

/// A stopped dependency's blocked operation, and its reason code (`None` for a fault).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub op: String,
    pub reason: Option<String>,
}

impl Stalled {
    /// Whether only an approver frees it: it waits for an approval.
    #[must_use]
    pub fn awaits_approval(&self) -> bool {
        self.blocked
            .as_ref()
            .and_then(|block| block.reason.as_deref())
            == Some(Reason::ApprovalPending.code())
    }

    /// The clause a wait's `why` and a runner's journal line name it with: what stalls it, and
    /// who frees it.
    #[must_use]
    pub fn clause(&self) -> String {
        let change = &self.change;
        match &self.blocked {
            None => format!(
                "{change} is not submitted: its actor {} must run bay merge for it",
                self.actor
            ),
            Some(block) if self.awaits_approval() => format!(
                "{change} waits for an approval ({}): an approver must run bay approve {change}",
                block.op
            ),
            Some(block) => format!(
                "{change} is blocked ({}{}): its actor {} must act on it, as bay status {change} says",
                block.op,
                block
                    .reason
                    .as_deref()
                    .map_or_else(String::new, |reason| format!(" {reason}")),
                self.actor
            ),
        }
    }
}

/// The dependencies `detail`, a `DEPENDENCY_PENDING` result's, names as stalled, in the order
/// the change named them.
#[must_use]
pub fn stalled(detail: Option<&Value>) -> Vec<Stalled> {
    let Some(pending) = detail
        .and_then(|detail| detail.get("pending"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    pending
        .iter()
        .filter_map(|dependency| {
            let blocked = match dependency.get("blocked") {
                Some(block) => Some(Block {
                    op: block["op"].as_str()?.to_owned(),
                    reason: block["reason"].as_str().map(str::to_owned),
                }),
                None if dependency["submitted"] == Value::Bool(false) => None,
                None => return None,
            };
            Some(Stalled {
                change: dependency["change"].as_str()?.to_owned(),
                actor: dependency["actor"].as_str()?.to_owned(),
                head: dependency["head"].as_str()?.to_owned(),
                blocked,
            })
        })
        .collect()
}

/// `state` as the change record spells it.
fn state_word(state: ChangeState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{state:?}").to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::{AFTER_TRAILER, trailers};

    /// One fixed trailer per dependency, in the order the change named them.
    #[test]
    fn each_dependency_is_one_trailer() {
        let after = ["chg_a".to_owned(), "chg_b".to_owned()];
        assert_eq!(trailers(&after), "Bay-After: chg_a\nBay-After: chg_b\n");
        assert_eq!(trailers(&[]), "");
        assert_eq!(AFTER_TRAILER, "Bay-After");
    }
}
