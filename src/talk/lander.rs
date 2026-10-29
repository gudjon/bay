//! `bay talk --to-change <change-id|commit>` (G77; the review dossier's K6): the recipient is the
//! actor that landed the change.
//!
//! The actor is read from bay's publication record only: the trunk commit must verify as an
//! executor-signed publication under the registry version it names
//! ([`crate::publish::verify_publication`]), and the actor is that change's record's, never a
//! trailer typed on a commit (a trailer on a forge commit is a claim, ruling T10). A change id
//! resolves to its landing ([`crate::undo::landed_commit`]); a commit resolves to the change its
//! signed `Change-Id` names. A commit on trunk that is no bay publication is
//! `OUTSIDE_MERGE`; a change that exists, or a commit, that is not on trunk is
//! `CHANGE_NOT_MERGED`, retried after a wait. A target that is neither a change id nor a commit
//! this repository holds, or a change id no change has here or on the remote, never will be on
//! trunk (T13): it is `TALK_REFUSED`, naming what is missing.

use serde_json::{Value, json};

use super::Refusal;
use crate::change::{Chain, change_ref};
use crate::ids::is_change_id;
use crate::repo::Repo;
use crate::result::Reason;

/// The lander of one change: the change, its publication commit, and the actor that landed it.
pub(crate) struct Lander {
    pub change: String,
    pub landing: String,
    pub who: String,
}

impl Lander {
    pub fn to_json(&self) -> Value {
        json!({"change": self.change, "landing": self.landing, "who": self.who})
    }
}

/// Resolves `target` to its lander. The outer error is a fault; the inner one a refusal.
pub(crate) fn resolve(repo: &Repo, target: &str) -> Result<Result<Lander, Refusal>, String> {
    let trunk = match repo.fetch_trunk() {
        Ok(trunk) => trunk,
        Err(_) => repo
            .git
            .run(&["rev-parse", "--verify", &repo.trunk_tracking_ref()])
            .map_err(|error| {
                format!("the local cache holds no main branch: {}", error.message())
            })?,
    };
    let not_landed =
        |why: String| Refusal::new(Reason::ChangeNotLanded, why).with("to_change", json!(target));
    let names_nothing =
        |why: String| Refusal::new(Reason::TalkRefused, why).with("to_change", json!(target));
    if is_change_id(target) {
        let Some(landing) = crate::undo::landed_commit(repo, target, &trunk)? else {
            if !change_exists(repo, target)? {
                return Ok(Err(names_nothing(format!(
                    "no change {target} is here or on the remote: a talk addressed by change goes to the actor that merged it, and this one never will be on main"
                ))));
            }
            return Ok(Err(not_landed(format!(
                "change {target} is not on main: a talk addressed by change goes to the actor that merged it"
            ))));
        };
        return lander_of(repo, target, landing).map(Ok);
    }
    let Ok(commit) = repo.git.run(&[
        "rev-parse",
        "--verify",
        "-q",
        &format!("{target}^{{commit}}"),
    ]) else {
        return Ok(Err(names_nothing(format!(
            "{target:?} is neither a change id nor a commit this repository holds: give a change id or a commit on main"
        ))));
    };
    if !repo.git.is_ancestor(&commit, &trunk)? {
        return Ok(Err(not_landed(format!(
            "commit {commit} is not on main: a talk addressed by change goes to the actor that merged it"
        ))));
    }
    let fields = repo
        .git
        .run(&[
            "show",
            "-s",
            "--format=%(trailers:key=Change-Id,valueonly,separator=%x2c)%x1f%(trailers:key=Registry-Id,valueonly,separator=%x2c)%x1f%cn",
            &commit,
        ])
        .map_err(|error| error.message())?;
    let fields: Vec<&str> = fields.split('\u{1f}').map(str::trim).collect();
    let [change, registry_id, committer] = fields.as_slice() else {
        return Err(format!("commit {commit} could not be read"));
    };
    if !is_change_id(change)
        || !crate::publish::verify_publication(&repo.git, &commit, registry_id, committer)
    {
        return Ok(Err(Refusal::new(
            Reason::UnattributedLanding,
            format!(
                "commit {commit} is on main but is no bay merge (no runner signature under the registry): it has no lander, and a trailer typed on it is a claim"
            ),
        )
        .with("commit", json!(commit))));
    }
    lander_of(repo, change, commit).map(Ok)
}

/// Whether a change `change` exists: its record is here, or the remote holds it.
fn change_exists(repo: &Repo, change: &str) -> Result<bool, String> {
    let name = change_ref(change);
    if repo
        .git
        .ref_target(&name)
        .map_err(|error| error.message())?
        .is_some()
    {
        return Ok(true);
    }
    let listed = repo
        .git
        .run(&["ls-remote", repo.remote(), &name])
        .map_err(|error| {
            format!(
                "whether change {change} exists could not be read from the remote: {}",
                error.message()
            )
        })?;
    Ok(!listed.trim().is_empty())
}

/// The actor of `change`'s record, which landed at `landing`.
fn lander_of(repo: &Repo, change: &str, landing: String) -> Result<Lander, String> {
    let name = change_ref(change);
    if repo.git.ref_target(&name).ok().flatten().is_none() {
        repo.fetch_refs(std::slice::from_ref(&name))
            .map_err(|error| error.message())?;
    }
    let (_, record) = Chain::new(&repo.git)
        .read(change)
        .map_err(|error| format!("the record of change {change}: {}", error.message()))?;
    Ok(Lander {
        change: change.to_owned(),
        landing,
        who: record.actor.name,
    })
}
