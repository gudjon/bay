//! `bay status --lineage [<range>]` (G76; `DESIGN` § 4, § 5; the review dossier's K5): the
//! lineage pairs of a first-parent trunk range, as JSON.
//!
//! Each first-parent commit of the range (oldest first) whose `Reverts:`, `Supersedes:` or
//! `Fixes:` trailer names a change gives one pair per named change: `{commit, change, relation,
//! target_change, target_commit, off_line, publication}`. `change` is the commit's own
//! `Change-Id` (null when it has none). `target_commit` is the named change's publication on the
//! first-parent line of the range's tip; a target that line does not hold is kept with
//! `target_commit` null and `off_line` true, never dropped, so a join over the pairs does not
//! under-count. `publication` says whether the naming commit is an executor-signed bay
//! publication (`crate::publish::verify_publication`): a trailer typed on a commit outside bay is
//! a claim, and is listed as one. Under no registry, bay runs unsigned and trailers decide, as
//! in `crate::publish::scan`.
//!
//! The range is a Git revision range (`A..B`) or one revision (its whole first-parent history),
//! and trunk's tracking ref by default. Lineage reads local refs only: it never fetches.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use serde_json::json;

use crate::git::Git;
use crate::registry::REGISTRY_REF;
use crate::repo::Repo;
use crate::result::BayResult;

/// Each relation word and the publication trailer that names it.
pub const RELATIONS: [(&str, &str); 3] = [
    ("reverts", "Reverts"),
    ("supersedes", "Supersedes"),
    ("fixes", "Fixes"),
];

#[must_use]
pub fn lineage(dir: &Path, range: Option<&str>) -> BayResult {
    run(dir, range).unwrap_or_else(BayResult::failed)
}

fn run(dir: &Path, range: Option<&str>) -> Result<BayResult, String> {
    let repo = Repo::open(dir)?;
    let git = &repo.git;
    let range = range
        .filter(|range| !range.is_empty())
        .map_or_else(|| repo.trunk_tracking_ref(), str::to_owned);
    // The tip of the range: its line is where targets are looked for.
    let tip_rev = range
        .rsplit_once("..")
        .map_or(range.as_str(), |(_, tip)| tip);
    let tip_rev = if tip_rev.is_empty() { "HEAD" } else { tip_rev };
    let tip = git
        .run(&[
            "rev-parse",
            "--verify",
            "-q",
            &format!("{tip_rev}^{{commit}}"),
        ])
        .map_err(|_| format!("{tip_rev} is not a commit in this repository"))?;
    let signed = git
        .ref_target(REGISTRY_REF)
        .map_err(|error| error.message())?
        .is_some();
    let named = walk(git, &range)?;
    let mut targets: BTreeMap<String, Option<String>> = BTreeMap::new();
    for commit in &named {
        for (_, target) in &commit.names {
            targets.entry(target.clone()).or_default();
        }
    }
    if !targets.is_empty() {
        find_targets(git, &tip, signed, &mut targets)?;
    }
    let mut pairs = Vec::new();
    for commit in &named {
        let publication = !signed || commit.publication(git);
        for (relation, target) in &commit.names {
            let target_commit = targets.get(target).cloned().flatten();
            pairs.push(json!({
                "commit": commit.id,
                "change": commit.change,
                "relation": relation,
                "target_change": target,
                "off_line": target_commit.is_none(),
                "target_commit": target_commit,
                "publication": publication,
            }));
        }
    }
    Ok(BayResult::ok(json!({
        "range": range,
        "trunk": tip,
        "count": pairs.len(),
        "pairs": pairs,
    })))
}

/// One first-parent commit that names at least one change.
struct Named {
    id: String,
    change: Option<String>,
    registry_id: String,
    committer: String,
    /// (relation, named change), in [`RELATIONS`] order.
    names: Vec<(&'static str, String)>,
}

impl Named {
    fn publication(&self, git: &Git) -> bool {
        crate::publish::verify_publication(git, &self.id, &self.registry_id, &self.committer)
    }
}

/// The first-parent commits of `range`, oldest first, that name a change.
fn walk(git: &Git, range: &str) -> Result<Vec<Named>, String> {
    let mut shape = "--format=%H%x1f%(trailers:key=Change-Id,valueonly,separator=%x2c)%x1f%(trailers:key=Registry-Id,valueonly,separator=%x2c)%x1f%cn".to_owned();
    for (_, trailer) in RELATIONS {
        let _ = write!(
            shape,
            "%x1f%(trailers:key={trailer},valueonly,separator=%x2c)"
        );
    }
    shape.push_str("%x1e");
    let log = git
        .run(&["log", "--first-parent", "--reverse", &shape, range, "--"])
        .map_err(|error| format!("{range} is not a range here: {}", error.message()))?;
    let mut found = Vec::new();
    for entry in log.split('\u{1e}') {
        let fields: Vec<&str> = entry.trim().split('\u{1f}').map(str::trim).collect();
        let [id, change, registry_id, committer, values @ ..] = fields.as_slice() else {
            continue;
        };
        let names: Vec<(&'static str, String)> = RELATIONS
            .iter()
            .zip(values)
            .flat_map(|((relation, _), values)| {
                split(values).map(move |target| (*relation, target.to_owned()))
            })
            .collect();
        if id.is_empty() || names.is_empty() {
            continue;
        }
        found.push(Named {
            id: (*id).to_owned(),
            change: split(change).next().map(str::to_owned),
            registry_id: (*registry_id).to_owned(),
            committer: (*committer).to_owned(),
            names,
        });
    }
    Ok(found)
}

fn split(values: &str) -> impl Iterator<Item = &str> {
    values
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Fills each target change's publication on the first-parent line of `tip`: the oldest commit
/// whose `Change-Id` names it and which is a bay publication (under a registry). A target the
/// line does not hold stays `None`.
fn find_targets(
    git: &Git,
    tip: &str,
    signed: bool,
    targets: &mut BTreeMap<String, Option<String>>,
) -> Result<(), String> {
    let log = git
        .run(&[
            "log",
            "--first-parent",
            "--reverse",
            "--format=%H%x1f%(trailers:key=Change-Id,valueonly,separator=%x2c)%x1f%(trailers:key=Registry-Id,valueonly,separator=%x2c)%x1f%cn%x1e",
            tip,
            "--",
        ])
        .map_err(|error| error.message())?;
    for entry in log.split('\u{1e}') {
        let fields: Vec<&str> = entry.trim().split('\u{1f}').map(str::trim).collect();
        let [id, change, registry_id, committer] = fields.as_slice() else {
            continue;
        };
        let Some(slot) = targets.get_mut(*change) else {
            continue;
        };
        if slot.is_none()
            && (!signed || crate::publish::verify_publication(git, id, registry_id, committer))
        {
            *slot = Some((*id).to_owned());
        }
    }
    Ok(())
}
