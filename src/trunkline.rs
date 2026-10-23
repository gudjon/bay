//! N-20: the landing line of a trunk, so a landing an outside merge pushed off the first-parent
//! line is still found.
//!
//! On a forge, someone outside bay can push a merge whose first parent is an older trunk commit
//! and whose other parent is the trunk's tip: bay's landings then sit only behind the merge's
//! other parents. A reader that walks `--first-parent` alone would lose them. A landing is found
//! by its marker and checked by ancestry (`merge-base --is-ancestor`), never by first-parent
//! position; where a reader needs a linear order (red windows, suspect ranges, bisection,
//! promotion candidates), it reads the **landing line** this module builds:
//!
//! - trunk's first-parent commits, newest first;
//! - and, right after each merge that is not a bay publication (a **foreign merge**), the bay
//!   landings reached only through its other parents, newest first in topological order,
//!   placed at the merge that brought them in.
//!
//! A bay landing here is a commit [`crate::unattributed::is_publication`] judges one: one
//! `Change-Id` and, under a registry, a verified publication signature.
//!
//! The cost is bounded: one `git log` for the first-parent line, and one `git log` per merge on
//! it, which reads at most [`MAX_EXPANDED`] commits behind that merge; a merge with more is
//! `capped`, and the results say so.

use serde_json::{Value, json};

use crate::git::Git;

/// The most commits read behind one merge's other parents.
pub const MAX_EXPANDED: usize = 500;

/// The recovery hint of a trunk that is not linear.
pub const HINT: &str = "a merge pushed outside bay put bay's merges off main's first-parent line; require linear history on the host for main, so only fast-forward pushes reach it";

/// The warning and event code of a foreign merge.
pub const NOT_LINEAR: &str = "trunk_not_linear";

/// The fields every [`Entry`] reads, before the caller's own.
const FIXED: &str = "%H%x1f%P%x1f%cn%x1f%(trailers:key=Change-Id,valueonly,separator=%x2c)%x1f%(trailers:key=Registry-Id,valueonly,separator=%x2c)";

/// One commit of the landing line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub commit: String,
    /// Its parents, space-separated, first parent first.
    pub parents: String,
    pub committer: String,
    /// Its `Change-Id` trailer values, joined by commas.
    pub change: String,
    pub registry_id: String,
    /// The merge that brought it onto trunk, for a landing reached only through a merge's other
    /// parents; `None` on the first-parent line.
    pub via: Option<String>,
    /// The caller's fields, in the order its format names them.
    pub extra: Vec<String>,
}

impl Entry {
    /// Whether this commit has more than one parent.
    #[must_use]
    pub fn is_merge(&self) -> bool {
        self.parents.split_whitespace().count() > 1
    }
}

/// A merge outside bay that put bay landings off the first-parent line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignMerge {
    pub commit: String,
    pub parents: Vec<String>,
    /// The bay landings reached only through its other parents, newest first.
    pub landings: Vec<String>,
    /// More than [`MAX_EXPANDED`] commits stand behind its other parents: `landings` names
    /// those among the first read, and there may be more.
    pub capped: bool,
}

impl ForeignMerge {
    /// The `trunk_not_linear` row of this merge: its commit, its parents, how many bay
    /// landings it pushed off the first-parent line, and the recovery hint.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "code": NOT_LINEAR,
            "commit": self.commit,
            "parents": self.parents,
            "landings": self.landings.len(),
            "landing_commits": self.landings,
            "capped": self.capped,
            "note": HINT,
        })
    }
}

/// The landing line of a trunk, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    pub entries: Vec<Entry>,
    /// How many first-parent commits were read: the line's window is full when this reaches it.
    pub first_parent: usize,
    /// The foreign merges on the line, newest first.
    pub merges: Vec<ForeignMerge>,
}

/// What stands behind one merge's other parents: the bay landings there, newest first in
/// topological order, and whether the read stopped at [`MAX_EXPANDED`] commits.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Expansion {
    pub landings: Vec<Entry>,
    pub capped: bool,
}

/// Whether this repository verifies publications: it has a registry.
///
/// # Errors
///
/// A git fault.
pub fn signatures(git: &Git) -> Result<bool, String> {
    Ok(git
        .ref_target(crate::registry::REGISTRY_REF)
        .map_err(|error| error.message())?
        .is_some())
}

/// The `--format` of an [`Entry`] with the caller's `extra` fields (placeholders separated by
/// `%x1f`; empty for none).
fn log_format(extra: &str) -> String {
    if extra.is_empty() {
        format!("--format={FIXED}%x1e")
    } else {
        format!("--format={FIXED}%x1f{extra}%x1e")
    }
}

/// The entries of a `git log` printed with [`log_format`].
fn parse(log: &str) -> Vec<Entry> {
    log.split('\u{1e}')
        .filter_map(|record| {
            let fields: Vec<&str> = record.trim().split('\u{1f}').map(str::trim).collect();
            let [commit, parents, committer, change, registry_id, extra @ ..] = fields.as_slice()
            else {
                return None;
            };
            (!commit.is_empty()).then(|| Entry {
                commit: (*commit).to_owned(),
                parents: (*parents).to_owned(),
                committer: (*committer).to_owned(),
                change: (*change).to_owned(),
                registry_id: (*registry_id).to_owned(),
                via: None,
                extra: extra.iter().map(|field| (*field).to_owned()).collect(),
            })
        })
        .collect()
}

/// Whether `entry` is a bay publication.
fn is_landing(git: &Git, signatures: bool, entry: &Entry) -> bool {
    crate::unattributed::is_publication(
        git,
        signatures,
        &entry.commit,
        &entry.change,
        &entry.registry_id,
        &entry.committer,
    )
}

/// The bay landings merge `merge` reaches only through its other parents and not from any of
/// `exclude`, newest first in topological order, each with `via` naming the merge. A commit
/// with one parent, or a merge that is itself a bay publication, has none.
///
/// # Errors
///
/// A git fault.
pub fn expand(
    git: &Git,
    merge: &Entry,
    exclude: &[&str],
    signatures: bool,
    extra: &str,
) -> Result<Expansion, String> {
    let Some(first) = merge.parents.split_whitespace().next() else {
        return Ok(Expansion::default());
    };
    if !merge.is_merge() || is_landing(git, signatures, merge) {
        return Ok(Expansion::default());
    }
    let max = format!("--max-count={}", MAX_EXPANDED + 1);
    let mut args = vec![
        "log".to_owned(),
        "--topo-order".to_owned(),
        max,
        log_format(extra),
    ];
    args.extend(merge.parents.split_whitespace().skip(1).map(str::to_owned));
    args.push(format!("^{first}"));
    args.extend(exclude.iter().map(|commit| format!("^{commit}")));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let log = git.run(&args).map_err(|error| error.message())?;
    let mut behind = parse(&log);
    let capped = behind.len() > MAX_EXPANDED;
    behind.truncate(MAX_EXPANDED);
    let landings = behind
        .into_iter()
        .filter(|entry| is_landing(git, signatures, entry))
        .map(|mut entry| {
            entry.via = Some(merge.commit.clone());
            entry
        })
        .collect();
    Ok(Expansion { landings, capped })
}

/// The landing line of `tip`: its newest `window` first-parent commits (not reachable from
/// `exclude`), each foreign merge followed by the bay landings it brought in (not reachable
/// from `exclude` either). Each entry carries the caller's `extra` fields.
///
/// # Errors
///
/// A git fault.
pub fn walk(
    git: &Git,
    tip: &str,
    window: usize,
    exclude: Option<&str>,
    extra: &str,
) -> Result<Line, String> {
    let signatures = signatures(git)?;
    let mut args = vec![
        "log".to_owned(),
        "--first-parent".to_owned(),
        format!("--max-count={window}"),
        log_format(extra),
        tip.to_owned(),
    ];
    if let Some(exclude) = exclude {
        args.push(format!("^{exclude}"));
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let first_parent = parse(&git.run(&args).map_err(|error| error.message())?);
    let excluded: Vec<&str> = exclude.into_iter().collect();
    let mut line = Line {
        first_parent: first_parent.len(),
        ..Line::default()
    };
    for entry in first_parent {
        let expansion = if entry.is_merge() {
            expand(git, &entry, &excluded, signatures, extra)?
        } else {
            Expansion::default()
        };
        let commit = entry.commit.clone();
        let parents = entry
            .parents
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        line.entries.push(entry);
        if !expansion.landings.is_empty() {
            line.merges.push(ForeignMerge {
                commit,
                parents,
                landings: expansion
                    .landings
                    .iter()
                    .map(|landing| landing.commit.clone())
                    .collect(),
                capped: expansion.capped,
            });
            line.entries.extend(expansion.landings);
        }
    }
    Ok(line)
}

/// Whether `commit` (a full object id) is on the landing line of `tip`: on its first-parent
/// line, or a bay landing that a foreign merge on that line brought in. The merge is the
/// oldest first-parent commit that reaches `commit`, found by bisection: a newer first-parent
/// commit reaches everything an older one does.
///
/// # Errors
///
/// A git fault.
pub fn on_line(git: &Git, tip: &str, commit: &str) -> Result<bool, String> {
    if crate::target::on_first_parent(git, tip, commit)? {
        return Ok(true);
    }
    if !git.is_ancestor(commit, tip)? {
        return Ok(false);
    }
    // The first-parent commits not reachable from `commit`, newest first: the merge that
    // brought it in is among them.
    let listed = git
        .run(&["rev-list", "--first-parent", tip, &format!("^{commit}")])
        .map_err(|error| error.message())?;
    let line: Vec<&str> = listed.lines().map(str::trim).collect();
    let (mut reaches, mut misses) = (0_usize, line.len());
    if line.is_empty() || !git.is_ancestor(commit, line[0])? {
        return Ok(false);
    }
    // Invariant: line[reaches] reaches `commit`; line[misses] (when in range) does not.
    while misses - reaches > 1 {
        let mid = reaches + (misses - reaches) / 2;
        if git.is_ancestor(commit, line[mid])? {
            reaches = mid;
        } else {
            misses = mid;
        }
    }
    let shape = log_format("");
    let log = git
        .run(&["log", "-1", &shape, line[reaches]])
        .map_err(|error| error.message())?;
    let Some(merge) = parse(&log).into_iter().next() else {
        return Ok(false);
    };
    let expansion = expand(git, &merge, &[], signatures(git)?, "")?;
    Ok(expansion
        .landings
        .iter()
        .any(|landing| landing.commit == commit))
}
