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

use serde_json::{Value, json};
use std::fmt::Write as _;

use crate::change::{Chain, ChangeState};
use crate::repo::Repo;

/// The fixed trailer a dependent's trunk commit names each of its dependencies in.
pub const AFTER_TRAILER: &str = "Bay-After";

/// The release reason of a queue place given back because a dependency has not landed. It
/// counts toward neither `QUEUE_DEADLINE_EXCEEDED` nor `PUBLISH_RETRY_EXHAUSTED` (`DESIGN` § 6).
pub const RELEASE: &str = "dependency_pending";

/// Where one dependency stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Standing {
    /// On trunk, at this commit.
    Landed(String),
    /// Not on trunk yet: its record's state, or why it could not be read.
    Pending(String),
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
            Standing::Pending(state) => json!({"change": self.change, "state": state}),
            Standing::Ended(state) => {
                json!({"change": self.change, "state": state, "landed": false})
            }
            Standing::Met => json!({"change": self.change, "state": "no_effect", "met": true}),
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
        let standing = match crate::undo::landed_commit(repo, change, trunk)? {
            Some(commit) => Standing::Landed(commit),
            None => unlanded(repo, change),
        };
        out.push(Dependency {
            change: change.clone(),
            standing,
        });
    }
    Ok(out)
}

/// The standing of `change`, which is not on trunk: its record's state, read from the remote.
fn unlanded(repo: &Repo, change: &str) -> Standing {
    let head = match repo.remote_chain(change) {
        Ok(remote) => remote.head,
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
            if record.state == ChangeState::NoEffect {
                Standing::Met
            } else if ended_unlanded(record.state) {
                Standing::Ended(state)
            } else {
                Standing::Pending(state)
            }
        }
        Err(error) => Standing::Pending(format!(
            "unknown: its record could not be read: {}",
            error.message()
        )),
    }
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
