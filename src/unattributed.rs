//! G85: an unchecked trunk commit is an unattributed landing (`DESIGN` § 10a: "On a forge
//! without such hooks, bay detects; it does not enforce").
//!
//! On a forge trunk, people may merge a pull request or push straight to trunk. Such a commit
//! did not land through bay: it is an `unattributed_landing` (`INTERFACES` § 5). It is derived
//! from trunk on each read, never stored:
//!
//! - Over the newest `[red] window` first-parent commits of trunk ([`crate::red::window`],
//!   default [`crate::red::DEFAULT_WINDOW`]) after bay's adoption (the newest
//!   first-parent commit that added `.bay/config.toml`; it and what it stands on predate bay),
//!   each commit that is not a bay publication is named, `landed_as` `merge` when it has more
//!   than one parent and `outside` otherwise.
//! - A commit is a bay publication as [`crate::publish::scan`] judges one: it carries one
//!   `Change-Id`, and, with a registry, it verifies against the executor file of the registry
//!   version its `Registry-Id` names. A commit that only quotes the trailer is unattributed.
//! - A merge also names the commits reached only through its other parents (at most
//!   [`MAX_MERGED`] of them) and counts them all, so a commit behind a merge is never silent
//!   (review K12). `off_first_parent` is their sum.
//! - N-20: the window is read as the landing line ([`crate::trunkline`]): a bay landing that an
//!   outside merge pushed off the first-parent line is a landing, never unattributed, and that
//!   merge names `landings_off_first_parent`, how many it pushed off (the top-level
//!   `landings_off_first_parent` is their sum).
//!
//! - NEW-4: under a config at trunk that declares `[[lane]]`s, each landing also names the lane
//!   commits it brought onto trunk (`lane_leak`, [`crate::lane::of_landing`]): the commits a
//!   merge's other parents reach that a lane ref this clone holds reaches too; `lane` is the
//!   first lane named, and `lane_leaks` counts the landings that name one.
//!
//! `bay status` lists them under `detail.unattributed`, `bay status --all` the same from the
//! local cache, and `bay status --since` names each listed landing outside bay as an
//! `unattributed_landing` event ([`event`]).

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::git::Git;

/// The most commits one merge names; `merged_count` counts them all.
pub const MAX_MERGED: usize = 20;

/// The event kind of a landing outside bay (`INTERFACES` § 5).
pub const EVENT_KIND: &str = "unattributed_landing";

/// Whether trunk commit `commit` is a bay publication, as [`crate::publish::scan`] judges one:
/// it carries exactly one `Change-Id` (`change`, the trailer values joined by commas) and,
/// when `signatures` (the repository has a registry), it verifies as a publication of the
/// registry version `registry_id` names, committed by `committer`.
#[must_use]
pub fn is_publication(
    git: &Git,
    signatures: bool,
    commit: &str,
    change: &str,
    registry_id: &str,
    committer: &str,
) -> bool {
    !change.is_empty()
        && !change.contains(',')
        && (!signatures || crate::publish::verify_publication(git, commit, registry_id, committer))
}

/// How a commit outside bay reached trunk: `merge` for more than one parent, else `outside`.
#[must_use]
pub fn landed_as(parents: &str) -> &'static str {
    if parents.split_whitespace().count() > 1 {
        "merge"
    } else {
        "outside"
    }
}

/// The `unattributed_landing` event for `commit`, which reached trunk outside bay as
/// `landed_as`. Its id names the commit, so the same landing reads as the same event.
#[must_use]
pub fn event(commit: &str, landed_as: &str, subject: &str) -> Value {
    json!({
        "schema": crate::log::EVENT_SCHEMA,
        "id": format!("{EVENT_KIND}:{commit}"),
        "kind": EVENT_KIND,
        "commit": commit,
        "landed_as": landed_as,
        "subject": subject,
        "recurrence_key": format!("{EVENT_KIND}/{commit}"),
    })
}

/// The unattributed landings of `trunk`, newest first, as `detail.unattributed`:
/// `{"window", "count", "by_writer": [{"writer", "count"}], "off_first_parent", "landings":
/// [{"commit", "landed_as", "base", "at", "subject", "merged", "merged_count"}]}`. G315:
/// `by_writer` groups the landings by their committer as Git records it, `Name <email>`, the
/// most first.
///
/// The cost is bounded: one log for the adoption, one log of the window's commits, one
/// signature check per commit that carries a `Change-Id` under a registry, and two
/// `rev-list` calls per merge; under lanes, one ref listing and [`crate::lane::reached`]'s
/// bounded calls per landing.
///
/// # Errors
///
/// A git fault, or a committed config bay cannot read.
pub fn scan(git: &Git, trunk: &str) -> Result<Value, String> {
    let window = crate::red::window(git, trunk)?;
    let adoption = git
        .run(&[
            "log",
            "--first-parent",
            "--diff-filter=A",
            "-n",
            "1",
            "--format=%H",
            trunk,
            "--",
            crate::config::CONFIG_PATH,
        ])
        .map_err(|error| error.message())?;
    let signatures = crate::trunkline::signatures(git)?;
    // N-20: the landing line, so a bay landing an outside merge pushed off the first-parent
    // line is known as one, and the merge names how many it pushed off.
    let line = crate::trunkline::walk(
        git,
        trunk,
        window,
        (!adoption.is_empty()).then_some(adoption.as_str()),
        "%ct%x1f%ce%x1f%s",
    )?;
    let (lanes, tips, _) = crate::lane::tips_at(git, trunk)?;
    let mut landings = Vec::new();
    let mut off_first_parent: u64 = 0;
    let mut pushed_off: u64 = 0;
    let mut lane_leaks: u64 = 0;
    // G305: the window's landings that came through bay, beside the ones that went around it.
    let mut through_bay: u64 = 0;
    // G315: the landings around bay by the committer Git records, `Name <email>`.
    let mut writers: BTreeMap<String, u64> = BTreeMap::new();
    for entry in &line.entries {
        let [time, email, subject] = entry.extra.as_slice() else {
            continue;
        };
        let (commit, parents) = (entry.commit.as_str(), entry.parents.as_str());
        if entry.via.is_some()
            || is_publication(
                git,
                signatures,
                commit,
                &entry.change,
                &entry.registry_id,
                &entry.committer,
            )
        {
            through_bay += 1;
            continue;
        }
        let foreign = line.merges.iter().find(|merge| merge.commit == commit);
        let landed_as = landed_as(parents);
        let (merged, merged_count) = if landed_as == "merge" {
            behind_merge(git, commit)?
        } else {
            (Vec::new(), 0)
        };
        off_first_parent += merged_count;
        let mut landing = json!({
            "commit": commit,
            "landed_as": landed_as,
            "base": parents.split_whitespace().next(),
            "at": time.parse().ok().map(crate::clock::rfc3339_of),
            "subject": subject,
            "merged": merged,
            "merged_count": merged_count,
        });
        if let Some(foreign) = foreign {
            // N-20: of the commits behind it, the bay landings it pushed off the line.
            pushed_off += foreign.landings.len() as u64;
            landing["landings_off_first_parent"] = json!(foreign.landings.len());
            landing["landings_off_first_parent_capped"] = json!(foreign.capped);
        }
        if !lanes.is_empty() {
            let leaks = crate::lane::of_landing(git, &tips, commit, parents)?;
            lane_leaks += u64::from(!leaks.is_empty());
            let (leaked, named) = crate::lane::row(&leaks);
            landing["lane"] = leaked;
            landing["lane_leak"] = named;
        }
        *writers
            .entry(format!("{} <{email}>", entry.committer))
            .or_insert(0_u64) += 1;
        landings.push(landing);
    }
    let mut scanned = json!({
        "window": window,
        "count": landings.len(),
        "by_writer": by_writer(writers),
        "through_bay": through_bay,
        "off_first_parent": off_first_parent,
        "landings_off_first_parent": pushed_off,
        "landings": landings,
    });
    if !lanes.is_empty() {
        scanned["lane_leaks"] = json!(lane_leaks);
    }
    Ok(scanned)
}

/// G315: `by_writer`, the landings around bay per committer, the most first (ties by name).
fn by_writer(writers: BTreeMap<String, u64>) -> Vec<Value> {
    let mut counted: Vec<(String, u64)> = writers.into_iter().collect();
    counted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counted
        .into_iter()
        .map(|(writer, count)| json!({"writer": writer, "count": count}))
        .collect()
}

/// The commits merge `commit` reaches only through its other parents: at most [`MAX_MERGED`]
/// of them, newest first, and how many there are.
fn behind_merge(git: &Git, commit: &str) -> Result<(Vec<String>, u64), String> {
    let others = format!("{commit}^@");
    let first = format!("^{commit}^1");
    let max = MAX_MERGED.to_string();
    let listed = git
        .run(&["rev-list", "-n", &max, &others, &first])
        .map_err(|error| error.message())?;
    let counted = git
        .run(&["rev-list", "--count", &others, &first])
        .map_err(|error| error.message())?;
    let count = counted
        .trim()
        .parse()
        .map_err(|_| format!("git rev-list --count printed {counted:?}"))?;
    Ok((listed.lines().map(str::to_owned).collect(), count))
}
