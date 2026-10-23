//! Runtime evidence: records of kind `running` and `effective` (`DESIGN` § 7 completion
//! dimensions; `INTERFACES` § 4 runtime adapter, § 6; ruling R12).
//!
//! bay links evidence; deployment stays outside. A runtime record is an evidence record on the
//! landed commit's tree, under its producer's notes ref, like a check's. It counts only when a
//! producer key signed it under its producer's name: a record an agent or an executor signed,
//! or one nobody signed, never confirms a dimension, and without a registry nothing can sign
//! one, so nothing is confirmed. `running` needs a `pass` that names the landed commit as its
//! subject and at least one artifact digest built from it; `effective` needs a `pass` on the
//! same commit for an artifact a counted `running` record names.
//!
//! The runtime adapter (`[plugin.runtime] command` in `.bay/config.toml`) is asked for records about
//! one landed commit; the records it prints that count are counted for that read and the rest
//! are refused, each with why. `bay status` is a read: it writes no evidence, because only a
//! producer writes its own notes ref. A producer that wants its observation kept writes it
//! there itself.

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::{Value, json};

use crate::evidence::{self, EvidenceRecord, Verdict};
use crate::git::Git;
use crate::registry::Registry;
use crate::repo::Repo;

pub const KIND_RUNNING: &str = "running";
pub const KIND_EFFECTIVE: &str = "effective";

/// How long one runtime adapter run may take; past it, its records are not read.
pub const RUNTIME_TIMEOUT: Duration = Duration::from_secs(10);
/// The most records one adapter run may print, and the most bytes of output: past either,
/// none is read, so one read verifies a bounded number of signatures.
pub const RUNTIME_MAX_RECORDS: usize = 64;
pub const RUNTIME_MAX_BYTES: usize = 1 << 20;

/// The counted runtime records for one landed commit, by id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Confirmed {
    pub running: Vec<String>,
    pub effective: Vec<String>,
}

/// The artifact digests `record` names when it is a `pass` about `commit` whose every
/// artifact is a `sha256:` digest; `None` otherwise. Its subject commit is the build
/// provenance it states.
fn observed<'a>(record: &'a EvidenceRecord, commit: &str) -> Option<Vec<&'a str>> {
    (record.verdict == Verdict::Pass && record.subject.commit == commit)
        .then(|| record.artifact_digests())
        .flatten()
}

/// Which of `records` confirm `running` and `effective` for `commit`: producer-signed only,
/// so without a registry none does, nor under `[policy] signing = "approvals"` (N-21), where no
/// producer key signs.
///
/// # Errors
///
/// The producer class file cannot be written.
pub fn confirmed(
    git: &Git,
    records: Vec<EvidenceRecord>,
    registry: Option<&Registry>,
    commit: &str,
) -> Result<Confirmed, String> {
    let Some(registry) = Registry::signing(registry) else {
        return Ok(Confirmed::default());
    };
    let runtime: Vec<EvidenceRecord> = records
        .into_iter()
        .filter(|record| record.kind == KIND_RUNNING || record.kind == KIND_EFFECTIVE)
        .collect();
    let counted = evidence::counted(git, runtime, Some(registry))?;
    let of = |kind: &'static str| {
        counted.iter().filter_map(move |record| {
            (record.kind == kind)
                .then(|| observed(record, commit))
                .flatten()
                .map(|artifacts| (record, artifacts))
        })
    };
    let running: Vec<_> = of(KIND_RUNNING).collect();
    let deployed: BTreeSet<&str> = running
        .iter()
        .flat_map(|(_, artifacts)| artifacts.iter().copied())
        .collect();
    let effective = of(KIND_EFFECTIVE)
        .filter(|(_, artifacts)| artifacts.iter().any(|artifact| deployed.contains(artifact)));
    Ok(Confirmed {
        running: running
            .iter()
            .map(|(record, _)| record.id.clone())
            .collect(),
        effective: effective.map(|(record, _)| record.id.clone()).collect(),
    })
}

/// Why `record`, printed by the runtime adapter for `commit` and `tree`, is not kept, or
/// `None` when it is a runtime record about them, signed by a producer key under its
/// producer's name.
fn refusal(
    record: &EvidenceRecord,
    commit: &str,
    tree: &str,
    producers: Option<&std::path::Path>,
) -> Option<String> {
    let producer = &record.producer.name;
    if record.schema != evidence::EVIDENCE_SCHEMA {
        return Some(format!(
            "schema {} is not {}",
            record.schema,
            evidence::EVIDENCE_SCHEMA
        ));
    }
    if record.kind != KIND_RUNNING && record.kind != KIND_EFFECTIVE {
        return Some(format!("kind {} is not running or effective", record.kind));
    }
    if record.subject.commit != commit || record.subject.tree != tree {
        return Some("its subject is not the merged commit and tree".to_owned());
    }
    if producer.is_empty()
        || producer.contains(['/', ' ', ':'])
        || !record.id.starts_with(&format!("{producer}/"))
    {
        return Some("its id is not under its producer's name".to_owned());
    }
    match producers {
        None => Some(
            "no registry signs producer records: only a producer-signed runtime record counts"
                .to_owned(),
        ),
        Some(file) if !record.verifies(file) => {
            Some("not signed by a producer key under its producer's name".to_owned())
        }
        Some(_) => None,
    }
}

/// What one runtime adapter run said: the records that count, for this read only, and the
/// records refused, each with why.
#[derive(Debug, Default)]
pub struct Observed {
    pub kept: Vec<EvidenceRecord>,
    pub refused: Vec<Value>,
}

/// Asks the runtime adapter about landed `commit` of `change`, with
/// `{"change", "commit", "tree"}` on stdin, and reads the JSON list of `bay.evidence/1`
/// records it prints. Nothing is written. `Ok(None)` when no adapter is configured.
///
/// # Errors
///
/// The adapter does not start, fails, runs past [`RUNTIME_TIMEOUT`], prints more than
/// [`RUNTIME_MAX_BYTES`] or [`RUNTIME_MAX_RECORDS`], or prints no list of records: nothing it
/// said is read, and the dimensions stay unconfirmed.
pub fn observe(
    repo: &Repo,
    change: &str,
    commit: &str,
    tree: &str,
    registry: Option<&Registry>,
) -> Result<Option<Observed>, String> {
    let Some(plugin) = repo.config.plugin.runtime.as_ref() else {
        return Ok(None);
    };
    let Some(command) = plugin.command.as_deref() else {
        return Ok(None);
    };
    let timeout = plugin.timeout(RUNTIME_TIMEOUT);
    // A program path relative to the main work tree, then its arguments, with only `PATH` and
    // `HOME` from bay's environment, as the notify adapter has (`crate::run`).
    let cmd = crate::run::command(&repo.work_root, command)
        .ok_or("[plugin.runtime] command is empty")?
        .map_err(|why| format!("the runtime plugin: {why}"))?;
    let request = json!({"change": change, "commit": commit, "tree": tree}).to_string();
    let ran = crate::run::captured(cmd, Some(request.as_bytes()), timeout)
        .map_err(|fault| format!("the runtime plugin: {fault}"))?;
    if !ran.status.success() {
        return Err(format!(
            "the runtime plugin exited with {}: {}",
            ran.status,
            String::from_utf8_lossy(&ran.stderr).trim()
        ));
    }
    let records = parse(&ran.stdout)?;
    let producers = Registry::signing(registry)
        .map(|registry| registry.class_file(&repo.git, crate::registry::KeyClass::Producer))
        .transpose()?;
    let mut observed = Observed::default();
    for record in evidence::without_conflicts(records) {
        match refusal(&record, commit, tree, producers.as_deref()) {
            Some(why) => observed
                .refused
                .push(json!({"id": record.id, "kind": record.kind, "why": why})),
            None => observed.kept.push(record),
        }
    }
    Ok(Some(observed))
}

/// The adapter's stdout as a list of records, within the bounds of one read.
fn parse(stdout: &[u8]) -> Result<Vec<EvidenceRecord>, String> {
    if stdout.len() > RUNTIME_MAX_BYTES {
        return Err(format!(
            "the runtime plugin printed more than {RUNTIME_MAX_BYTES} bytes"
        ));
    }
    let list: Vec<Value> = serde_json::from_slice(stdout)
        .map_err(|error| format!("the runtime plugin printed no list of records: {error}"))?;
    if list.len() > RUNTIME_MAX_RECORDS {
        return Err(format!(
            "the runtime plugin printed {} records; one read judges at most {RUNTIME_MAX_RECORDS}",
            list.len()
        ));
    }
    list.into_iter()
        .map(serde_json::from_value)
        .collect::<Result<_, _>>()
        .map_err(|error| format!("the runtime plugin printed a record bay cannot read: {error}"))
}
