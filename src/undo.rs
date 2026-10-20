//! `bay undo <change>`: a compensating change, checked against current trunk like any other;
//! never a rewrite (`DESIGN` § 4, `INTERFACES` § 1, ruling R10).
//!
//! The undo is a new change opened through `start` on the intent `undo of <Change-Id>`, so it
//! uses intent generations like any start: a second undo of one change claims the next
//! generation, and only once the previous undo landed. Its tree is
//! `git merge-tree --write-tree --merge-base=C <trunk> C^`, written into its view, and it lands
//! through the normal publication with a `Reverts: <Change-Id>` trailer. It never claims a
//! generation of the undone change's intent. When the revert tree is trunk's own tree (the effect
//! is already gone), no change is opened. A remote that cannot be read is a failure, never
//! `CHANGE_NOT_LANDED`. An undo never moves a ref back (G199): trunk moves only by the
//! never-forced publishing push, and the undone change's chain is read beside this store's copy,
//! which only ever moves forward.

use std::path::Path;

use serde_json::json;

use crate::change::{Chain, change_ref};
use crate::git::Git;
use crate::ids::is_change_id;
use crate::merge::{Merge, revert_onto};
use crate::publish;
use crate::repo::Repo;
use crate::result::{BayResult, Decision, Reason};
use crate::start::{self, StartRequest};

/// The revision of every undo intent: the undone change is its whole reference.
const UNDO_REVISION: &str = "1";

#[must_use]
pub fn undo(dir: &Path, change: &str, wait: bool) -> BayResult {
    run(dir, change, wait).unwrap_or_else(BayResult::failed)
}

fn run(dir: &Path, change: &str, wait: bool) -> Result<BayResult, String> {
    if !is_change_id(change) {
        return Err(format!("{change} is not a change id"));
    }
    let repo = match Repo::open_fetched(dir) {
        Ok(repo) => repo,
        Err(error) => return Ok(error.into_result()),
    };
    // G56 (ruling T9): the talks on the undone change, fetched before the undo and named in
    // its result. A talk never blocks an undo.
    let open_talks = crate::talk::open_on(&repo, change);
    // G80: every result says which trunk it reflects, and when that trunk was fetched.
    let mut result = undo_in(dir, &repo, change, wait)?;
    result.freshness = result.freshness.or_else(|| repo.freshness());
    crate::talk::name_open_talks(&mut result, open_talks);
    Ok(result)
}

fn undo_in(dir: &Path, repo: &Repo, change: &str, wait: bool) -> Result<BayResult, String> {
    let trunk = repo.fetch_trunk().map_err(|error| error.message())?;
    let Some(commit) = landed_commit(repo, change, &trunk)? else {
        return Ok(BayResult::blocked(Reason::ChangeNotLanded)
            .with_change(change)
            .with_detail(json!({"trunk": trunk})));
    };
    let paths: Vec<String> = repo
        .git
        .run(&[
            "diff-tree",
            "-r",
            "--no-commit-id",
            "--name-only",
            &format!("{commit}^"),
            &commit,
        ])
        .map_err(|error| error.message())?
        .lines()
        .map(str::to_owned)
        .collect();

    // The effect is already gone from trunk: open no change, never land an empty one.
    if let Merge::Clean(tree) =
        revert_onto(&repo.git, &trunk, &commit).map_err(|error| error.message())?
        && tree == tree_of(&repo.git, &trunk)?
    {
        return Ok(BayResult::ok(json!({
            "undo": "nothing to undo",
            "reverts": change,
            "trunk": trunk,
        })));
    }

    let intent = format!("undo of {change}");
    let request = StartRequest {
        new: start::claimed(repo, &intent, UNDO_REVISION)?,
        intent: Some(intent),
        intent_revision: UNDO_REVISION.to_owned(),
        paths,
        inputs: Vec::new(),
        resume: None,
        reverts: Some(change.to_owned()),
        fixes: None,
    };
    let started = start::start(dir, &request);
    if started.decision != Decision::Ok {
        return Ok(started);
    }
    let id = started.change.clone().ok_or("start returned no change")?;
    let detail = started.detail.clone().unwrap_or_default();
    let (Some(view), Some(base)) = (detail["view"].as_str(), detail["base"].as_str()) else {
        return Err(format!("start of {id} returned no view and base"));
    };
    let view = Path::new(view);

    let (tree, conflicts) =
        match revert_onto(&repo.git, base, &commit).map_err(|error| error.message())? {
            Merge::Clean(tree) => (tree, None),
            Merge::Conflict(paths, tree) => (tree, Some(paths)),
        };
    // G212: the sparse view's read-tree fetches only the declared files it writes.
    Git::new(view)
        .fetching()
        .run(&["read-tree", "-u", "--reset", &tree])
        .map_err(|error| error.message())?;
    if let Some(conflicts) = conflicts {
        // The view holds the revert with conflict markers, open for repair; `bay land` in it
        // lands the repaired undo (INTERFACES § 2 MERGE_CONFLICT).
        let refused = BayResult::blocked(Reason::MergeConflict)
            .with_change(id.clone())
            .with_detail(json!({
                "conflicts": conflicts,
                "view": view.display().to_string(),
                "reverts": change,
                "trunk": base,
            }));
        return Ok(publish::with_receipt(repo, &id, "undo", refused));
    }
    Ok(publish::land(view, Some(&id), wait, None, false))
}

fn tree_of(git: &Git, commit: &str) -> Result<String, String> {
    git.run(&["rev-parse", &format!("{commit}^{{tree}}")])
        .map_err(|error| error.message())
}

/// The trunk commit that published `change`: the remote's `refs/bay/landed/<change>`, or the
/// shard index record that stands in for it (§ 10a), names it and the trunk scan from the
/// change's `base_first` finds it (§ 6). `None` when the change is not on trunk.
pub(crate) fn landed_commit(
    repo: &Repo,
    change: &str,
    trunk: &str,
) -> Result<Option<String>, String> {
    // Only a marker the remote answered without is "not landed": a remote that cannot be read
    // is a fault, never CHANGE_NOT_LANDED, whose retry is never.
    let unreadable = |error: crate::git::GitError| {
        format!(
            "the remote could not be read, so whether {change} landed is unknown: {}",
            error.message()
        )
    };
    // § 10a: a compacted marker's shard index record stands in for the ref.
    let marker = format!("{}{change}", crate::names::LANDED);
    let Some(landed) = crate::compact::remote_marker(repo, &marker).map_err(|error| {
        format!("the remote could not be read, so whether {change} landed is unknown: {error}")
    })?
    else {
        return Ok(None);
    };
    let remote = fetch_chain(repo, change).map_err(unreadable)?;
    let record = Chain::new(&repo.git)
        .record_at(&remote)
        .map_err(|error| error.message())?;
    let scan = publish::scan(&repo.git, &record.base_first, trunk, change)?;
    Ok(scan
        .publications
        .into_iter()
        .any(|found| found.commit == landed)
        .then_some(landed))
}

/// The remote head of `change`'s chain, read without moving this store's copy back (G199): it
/// is fetched into `refs/bay/fetch/<id>`, and the local chain is created at it, or moved to it
/// only when it descends from the local head, each by compare-and-swap. A local chain that is
/// ahead of the remote, or apart from it, is left where it is.
fn fetch_chain(repo: &Repo, change: &str) -> Result<String, crate::git::GitError> {
    let git = &repo.git;
    let name = change_ref(change);
    let fetched = format!("{}{change}", crate::names::FETCH);
    git.run(&[
        "fetch",
        "-q",
        "--no-tags",
        repo.remote(),
        &format!("+{name}:{fetched}"),
    ])?;
    let remote = git.run(&["rev-parse", "--verify", &fetched])?;
    let _ = git.run(&["update-ref", "-d", &fetched, &remote]);
    match git.ref_target(&name)? {
        None => {
            let _ = git.run(&["update-ref", &name, &remote, ""]);
        }
        Some(local)
            if local != remote
                && git
                    .run(&["merge-base", "--is-ancestor", &local, &remote])
                    .is_ok() =>
        {
            let _ = git.run(&["update-ref", &name, &remote, &local]);
        }
        Some(_) => {}
    }
    Ok(remote)
}
