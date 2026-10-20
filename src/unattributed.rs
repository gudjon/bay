//! G85: an unchecked trunk commit is an unattributed landing (`DESIGN` § 10a: "On a forge
//! without such hooks, bay detects; it does not enforce").
//!
//! On a forge trunk, people may merge a pull request or push straight to trunk. Such a commit
//! did not land through bay: it is an `unattributed_landing` (`INTERFACES` § 5). It is derived
//! from trunk on each read, never stored:
//!
//! - Over the newest [`WINDOW`] first-parent commits of trunk after bay's adoption (the newest
//!   first-parent commit that added `.bay/config.toml`; it and what it stands on predate bay),
//!   each commit that is not a bay publication is named, `landed_as` `merge` when it has more
//!   than one parent and `outside` otherwise.
//! - A commit is a bay publication as [`crate::publish::scan`] judges one: it carries one
//!   `Change-Id`, and, with a registry, it verifies against the executor file of the registry
//!   version its `Registry-Id` names. A commit that only quotes the trailer is unattributed.
//! - A merge also names the commits reached only through its other parents (at most
//!   [`MAX_MERGED`] of them) and counts them all, so a commit behind a merge is never silent
//!   (review K12). `off_first_parent` is their sum.
//!
//! `bay status` lists them under `detail.unattributed`, `bay status --fleet` the same from the
//! local cache, and `bay watch` names each listed landing outside bay as an
//! `unattributed_landing` event ([`event`]).

use serde_json::{Value, json};

use crate::git::Git;

/// How many first-parent trunk commits the scan reads back, as a red judgement does.
pub const WINDOW: usize = crate::red::WINDOW;

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
/// `{"window", "count", "off_first_parent", "landings": [{"commit", "landed_as", "base",
/// "at", "subject", "merged", "merged_count"}]}`.
///
/// The cost is bounded: one log for the adoption, one log of [`WINDOW`] commits, one
/// signature check per commit that carries a `Change-Id` under a registry, and two
/// `rev-list` calls per merge.
///
/// # Errors
///
/// A git fault.
pub fn scan(git: &Git, trunk: &str) -> Result<Value, String> {
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
    let signatures = git
        .ref_target(crate::registry::REGISTRY_REF)
        .map_err(|error| error.message())?
        .is_some();
    let window = WINDOW.to_string();
    let mut args = vec![
        "log".to_owned(),
        "--first-parent".to_owned(),
        "-n".to_owned(),
        window,
        "--format=%H%x1f%P%x1f%ct%x1f%cn%x1f%(trailers:key=Change-Id,valueonly,separator=%x2c)%x1f%(trailers:key=Registry-Id,valueonly,separator=%x2c)%x1f%s%x1e".to_owned(),
        trunk.to_owned(),
    ];
    if !adoption.is_empty() {
        args.push(format!("^{adoption}"));
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let log = git.run(&args).map_err(|error| error.message())?;
    let mut landings = Vec::new();
    let mut off_first_parent: u64 = 0;
    for entry in log.split('\u{1e}') {
        let fields: Vec<&str> = entry.trim().split('\u{1f}').map(str::trim).collect();
        let [
            commit,
            parents,
            time,
            committer,
            change,
            registry_id,
            subject,
        ] = fields.as_slice()
        else {
            continue;
        };
        if commit.is_empty()
            || is_publication(git, signatures, commit, change, registry_id, committer)
        {
            continue;
        }
        let landed_as = landed_as(parents);
        let (merged, merged_count) = if landed_as == "merge" {
            behind_merge(git, commit)?
        } else {
            (Vec::new(), 0)
        };
        off_first_parent += merged_count;
        landings.push(json!({
            "commit": commit,
            "landed_as": landed_as,
            "base": parents.split_whitespace().next(),
            "at": time.parse().ok().map(crate::clock::rfc3339_of),
            "subject": subject,
            "merged": merged,
            "merged_count": merged_count,
        }));
    }
    Ok(json!({
        "window": WINDOW,
        "count": landings.len(),
        "off_first_parent": off_first_parent,
        "landings": landings,
    }))
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
