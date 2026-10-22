//! Overlapping changes (NEW-8): the live changes whose declared paths share files with a
//! change's own.
//!
//! `bay start` lists them in its detail (`overlaps`), and the agent hook's `session.start`
//! names them for the session's change, so two agents that declare the same files hear of each
//! other before either lands. A listing never refuses: it is what the work may meet, not a
//! claim. It reads the chains this repository already holds (`crate::change::Chain::read_all`,
//! one `for-each-ref` and one `cat-file --batch`) and the landed markers it holds: no fetch, so
//! a change another clone opened is listed once a fetching command (`bay status`, `bay start`,
//! `land`) brought its chain.
//!
//! A change is live while its chain says it is open, invalidated, repairing or ready, it holds
//! no landed marker here, and it is in no sandbox lane (a lane change never lands, NEW-4). Two
//! declared paths overlap when one holds the other (`crate::inputs::within`); a change that
//! declares none covers the whole tree, named `.`. Each overlap names the narrower path.

use std::collections::{BTreeSet, HashMap};

use serde_json::{Value, json};

use crate::change::{Chain, ChangeState, HeldChain};
use crate::repo::Repo;

/// The live changes other than `id` this repository holds whose declared paths overlap
/// `declared`, in ref order: each its change id, intent, actor and the overlapping paths.
///
/// # Errors
///
/// The chains or the landed markers cannot be listed.
pub fn overlaps(repo: &Repo, id: &str, declared: &[String]) -> Result<Vec<Value>, String> {
    let (chains, landed) = read_chains(repo)?;
    Ok(chains
        .iter()
        .filter(|held| held.id != id && counts_live(held, &landed) && held.record.lane.is_none())
        .filter_map(|held| {
            let theirs = crate::inputs::paths(&held.record.declared_inputs);
            let paths = shared(declared, &theirs);
            (!paths.is_empty()).then(|| {
                json!({
                    "change": held.id,
                    "intent": held.record.intent.reference,
                    "actor": held.record.actor,
                    "paths": paths,
                })
            })
        })
        .collect())
}

/// G264: the live changes the actor named `actor` holds, by the chains this repository holds,
/// in ref order: each its change id and intent. A lane change counts: it holds its actor's
/// attention until it is abandoned, though it never lands. A change with a landed marker here
/// has ended.
///
/// # Errors
///
/// The chains or the landed markers cannot be listed.
pub fn held_by(repo: &Repo, actor: &str) -> Result<Vec<Value>, String> {
    let (chains, landed) = read_chains(repo)?;
    Ok(chains
        .iter()
        .filter(|held| held.record.actor.name == actor && counts_live(held, &landed))
        .map(|held| {
            json!({
                "change": held.id,
                "intent": held.record.intent.reference,
                "lane": held.record.lane,
            })
        })
        .collect())
}

/// Every chain this repository holds (`Chain::read_all`) and its landed markers: change id to
/// the publication its `refs/bay/landed/<id>` names. No fetch. The one read of what counts as
/// live: [`held_by`] (G264), [`overlaps`], `bay status`'s `detail.flow` (G309) and the review
/// queue of `bay merge` (G310) all judge [`counts_live`] on it.
///
/// # Errors
///
/// The chains or the landed markers cannot be listed.
pub fn read_chains(repo: &Repo) -> Result<(Vec<HeldChain>, HashMap<String, String>), String> {
    let chains = Chain::new(&repo.git)
        .read_all()
        .map_err(|error| error.message())?;
    Ok((chains, landed(repo)?))
}

/// Whether `held` is live: its chain says it has not ended ([`live`]) and `landed`, the
/// markers [`read_chains`] read, holds no landing of it. A lane change counts.
#[must_use]
pub fn counts_live<S: std::hash::BuildHasher>(
    held: &HeldChain,
    landed: &HashMap<String, String, S>,
) -> bool {
    live(held.record.state) && !landed.contains_key(&held.id)
}

/// The landed markers this repository holds: change id to the publication the marker names.
fn landed(repo: &Repo) -> Result<HashMap<String, String>, String> {
    Ok(repo
        .git
        .run(&[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            crate::names::LANDED,
        ])
        .map_err(|error| error.message())?
        .lines()
        .filter_map(|line| {
            let (commit, name) = line.split_once(' ')?;
            let id = name.strip_prefix(crate::names::LANDED)?;
            Some((id.to_owned(), commit.to_owned()))
        })
        .collect())
}

/// A change that has not ended by its chain.
pub(crate) fn live(state: ChangeState) -> bool {
    matches!(
        state,
        ChangeState::Open | ChangeState::Invalidated | ChangeState::Repairing | ChangeState::Ready
    )
}

/// A declared path without a leading `./` or `/`; the whole tree is empty.
fn plain(path: &str) -> &str {
    path.trim_start_matches("./").trim_start_matches('/')
}

/// The declared paths, or the whole tree when there are none.
fn whole_or(declared: &[String]) -> Vec<&str> {
    if declared.is_empty() {
        vec![""]
    } else {
        declared.iter().map(String::as_str).map(plain).collect()
    }
}

/// Whether `outer` holds `inner`; the whole tree holds every path.
fn holds(outer: &str, inner: &str) -> bool {
    outer.is_empty() || crate::inputs::within(outer, inner)
}

/// The paths two declarations share: for each pair where one holds the other, the narrower one,
/// sorted and once; `.` for the whole tree. NEW-10's landing notice judges a landing by it
/// too (`crate::notice`), so the two name the same paths.
pub(crate) fn shared(mine: &[String], theirs: &[String]) -> Vec<String> {
    let (mine, theirs) = (whole_or(mine), whole_or(theirs));
    let mut out = BTreeSet::new();
    for a in &mine {
        for b in &theirs {
            let narrower = if holds(a, b) {
                b
            } else if holds(b, a) {
                a
            } else {
                continue;
            };
            out.insert(if narrower.is_empty() {
                ".".to_owned()
            } else {
                (*narrower).to_owned()
            });
        }
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::shared;

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|path| (*path).to_owned()).collect()
    }

    #[test]
    fn a_directory_overlaps_the_files_under_it_and_names_the_narrower() {
        assert_eq!(
            shared(&paths(&["src/"]), &paths(&["src/a.rs", "docs/"])),
            ["src/a.rs"]
        );
        assert_eq!(
            shared(&paths(&["src/a.rs"]), &paths(&["src"])),
            ["src/a.rs"]
        );
        assert_eq!(shared(&paths(&["./src/"]), &paths(&["src/"])), ["src/"]);
    }

    #[test]
    fn disjoint_paths_and_a_shared_prefix_do_not_overlap() {
        assert_eq!(
            shared(&paths(&["src/"]), &paths(&["docs/"])),
            Vec::<String>::new()
        );
        assert_eq!(
            shared(&paths(&["src/a"]), &paths(&["src/ab.rs"])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn no_declared_path_is_the_whole_tree() {
        assert_eq!(shared(&[], &paths(&["docs/"])), ["docs/"]);
        assert_eq!(shared(&[], &[]), ["."]);
    }
}
