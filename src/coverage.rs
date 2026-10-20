//! `bay evidence missing --kind <k> --producer <p> [--since <rev>]` (G74): the trunk commits
//! no record of kind `k` from producer `p` covers, so a producer's queue is a query bay
//! answers, not a walk each producer writes.
//!
//! The population is trunk's first-parent line after `rev` (all of it without one), oldest
//! first, and `off_first_parent` counts the commits in the same range reached only through a
//! merge's other parent: they are never listed, and the count says so. A note is keyed by
//! tree, but a record is about a landing's diff: it covers a landing only when its
//! `subject.commit` is that commit, or its `subject.base_tree` and `subject.tree` are the
//! tree of the landing's first parent and the landing's tree. A tree alone is never enough: a
//! landing that restores an earlier tree (an undo of an undo) is not covered by the record of
//! the landing that first made it. Records count as `bay status` counts them
//! ([`crate::evidence::records_for`], [`crate::evidence::counted`]): only from the producer's
//! own ref under its own namespaced id, and, with a registry, only when signed under the
//! producer's name by a key of the class the kind needs: the reviewer class for `review`, the
//! producer class for every other kind (rulings-open-questions.md Q12).
//!
//! Each row and the result carry `trust`: `unsigned` without a registry (every record
//! counts), else `signed`. A row whose only covering records do not verify says
//! `unverifiable`. The query reads; it writes nothing.
//!
//! G195: with `--since`, its Git work grows with the commits after the rev, not with history:
//! the walk stops at the rev (`^rev`), and each listed commit costs one note read.

use std::collections::HashMap;
use std::path::Path;

use serde_json::json;

use crate::clock::now_rfc3339;
use crate::evidence::{self, EvidenceRecord};
use crate::git::Git;
use crate::registry::Registry;
use crate::repo::Repo;
use crate::result::{BayResult, Freshness};

/// What `bay evidence missing` asks.
pub struct Missing {
    pub kind: String,
    pub producer: String,
    /// The rev the landings start after.
    pub since: Option<String>,
}

#[must_use]
pub fn missing(dir: &Path, request: &Missing) -> BayResult {
    run(dir, request).unwrap_or_else(BayResult::failed)
}

fn run(dir: &Path, request: &Missing) -> Result<BayResult, String> {
    let repo = match Repo::open_fetched(dir) {
        Ok(repo) => repo,
        Err(error) => return Ok(error.into_result()),
    };
    // Best effort, as `status`: offline, the local cache is what this read can see, and
    // `freshness` says so.
    let fetched_at = repo.fetch_trunk().ok().map(|_| now_rfc3339());
    repo.fetch_registry();
    repo.fetch_evidence(&crate::signing::Keys::load(&repo.git));
    let git = &repo.git;
    let trunk = git
        .run(&["rev-parse", "--verify", &repo.trunk_tracking_ref()])
        .map_err(|error| format!("the local cache holds no trunk: {}", error.message()))?;
    let since = request
        .since
        .as_deref()
        .map(|rev| {
            git.run(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{rev}^{{commit}}"),
            ])
            .map_err(|_| format!("--since {rev:?} names no commit in this repository"))
        })
        .transpose()?;
    let registry = Registry::current(git)?;
    let producers = registry
        .as_ref()
        .map(|registry| registry.class_file(git, evidence::signer_class(&request.kind)))
        .transpose()?;
    let (checked, rows) = uncovered(git, request, &trunk, since.as_deref(), producers.as_deref())?;

    let mut result = BayResult::ok(json!({
        "kind": request.kind,
        "producer": request.producer,
        "since": since,
        "trunk": trunk,
        "checked": checked,
        "missing": rows,
        "off_first_parent": crate::watch::off_first_parent(git, since.as_deref(), &trunk)?,
        "trust": if producers.is_some() { "signed" } else { "unsigned" },
    }));
    result.freshness = Some(Freshness {
        trunk,
        fetched_at: fetched_at.unwrap_or_else(|| "not fetched (remote unreachable)".to_owned()),
    });
    Ok(result)
}

/// The first-parent commits of `trunk` after `since` and, of those, the rows no counted
/// record covers. `producers` is the producer class file under a registry.
fn uncovered(
    git: &Git,
    request: &Missing,
    trunk: &str,
    since: Option<&str>,
    producers: Option<&Path>,
) -> Result<(usize, Vec<serde_json::Value>), String> {
    let mut args = vec![
        "log".to_owned(),
        "--first-parent".to_owned(),
        "--reverse".to_owned(),
        "--format=%H%x1f%T%x1f%P%x1e".to_owned(),
        trunk.to_owned(),
    ];
    if let Some(since) = since {
        args.push(format!("^{since}"));
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let log = git.run(&args).map_err(|error| error.message())?;

    let mut trees: HashMap<String, String> = HashMap::new();
    let mut notes: HashMap<String, Vec<EvidenceRecord>> = HashMap::new();
    let (mut checked, mut rows) = (0_usize, Vec::new());
    for entry in log.split('\u{1e}') {
        let fields: Vec<&str> = entry.trim().split('\u{1f}').map(str::trim).collect();
        let [commit, tree, parents] = fields.as_slice() else {
            continue;
        };
        if commit.is_empty() {
            continue;
        }
        checked += 1;
        trees.insert((*commit).to_owned(), (*tree).to_owned());
        let base = parents.split_whitespace().next();
        let base_tree = match base {
            Some(base) => Some(match trees.get(base) {
                Some(tree) => tree.clone(),
                None => git
                    .run(&["rev-parse", &format!("{base}^{{tree}}")])
                    .map_err(|error| error.message())?,
            }),
            None => None,
        };
        if !notes.contains_key(*tree) {
            let records = of_kind(git, &request.producer, &request.kind, tree)?;
            notes.insert((*tree).to_owned(), records);
        }
        let covering: Vec<&EvidenceRecord> = notes[*tree]
            .iter()
            .filter(|record| covers(record, commit, tree, base_tree.as_deref()))
            .collect();
        let trust = match producers {
            None if covering.is_empty() => "unsigned",
            None => continue,
            Some(file) if covering.iter().any(|record| record.verifies(file)) => continue,
            Some(_) if covering.is_empty() => "signed",
            Some(_) => "unverifiable",
        };
        rows.push(json!({
            "commit": commit,
            "tree": tree,
            "base": base,
            "base_tree": base_tree,
            "trust": trust,
        }));
    }
    Ok((checked, rows))
}

/// The records of `kind` that `producer` holds about `tree`: from its own ref, under its own
/// namespaced id, with no two different records under one id (as
/// [`crate::evidence::records_for`] reads them).
fn of_kind(
    git: &Git,
    producer: &str,
    kind: &str,
    tree: &str,
) -> Result<Vec<EvidenceRecord>, String> {
    Ok(
        evidence::without_conflicts(evidence::producer_records(git, producer, tree)?)
            .into_iter()
            .filter(|record| {
                record.kind == kind
                    && record.producer.name == producer
                    && record.id.starts_with(&format!("{producer}/"))
                    && record.subject.tree == tree
            })
            .collect(),
    )
}

/// Whether `record` is about the landing `commit` of `tree` on a first parent of `base_tree`:
/// by its commit, or by its diff (base tree and tree). Never by its tree alone.
fn covers(record: &EvidenceRecord, commit: &str, tree: &str, base_tree: Option<&str>) -> bool {
    record.subject.commit == commit
        || (record.subject.tree == tree
            && base_tree.is_some_and(|base_tree| record.subject.base_tree == base_tree))
}

/// A producer name as `bay evidence missing` takes it (`DESIGN` § 10a).
///
/// # Errors
///
/// The name is not a producer name.
pub fn producer_name(name: &str) -> Result<String, String> {
    crate::names::operator_name(name).map(|()| name.to_owned())
}
