//! Evidence records and their notes (`DESIGN` § 5, § 7; `INTERFACES` § 6).
//!
//! Each producer writes only `refs/notes/bay/evidence/<producer>`. A note sits on the result
//! **tree** and holds a JSON list of records. A reader takes the union over every producer
//! ref and combines records per check id and environment; never latest-wins.

use serde::{Deserialize, Serialize};

use crate::change::RECORD_IDENTITY;
use crate::git::{Git, GitError};

pub const EVIDENCE_PREFIX: &str = "refs/notes/bay/evidence/";
pub const EVIDENCE_SCHEMA: &str = "bay.evidence/1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    ProductFailure,
    InfraFailure,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    pub tree: String,
    pub commit: String,
    pub base_tree: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckId {
    pub id: String,
    pub environment: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Producer {
    pub name: String,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unit {
    pub id: String,
    pub verdict: Verdict,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRecord {
    pub schema: String,
    /// `<producer>/<id>`.
    pub id: String,
    pub kind: String,
    pub subject: Subject,
    /// The policy check this record answers.
    pub check: CheckId,
    pub policy_revision: u64,
    pub manifest_digest: String,
    /// How the policy at the accepted base reached trunk: `bay` or `outside_bay`
    /// (`INTERFACES` § 6). Absent only on records written before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_provenance: Option<String>,
    pub producer: Producer,
    pub verdict: Verdict,
    pub at: String,
    /// Why the verdict is not `pass`, when it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub units: Vec<Unit>,
    /// `declared_empty` when the check passed on an empty selection under an explicit
    /// `expected_minimum = 0` (`INTERFACES` § 7): a coverage gap (G30), never tested code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<String>,
    /// The sandbox the check ran in: `sandbox-exec`, `bwrap`, or `none`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    /// The producer key's SSH signature over the canonical JSON of the record without this
    /// field (`ssh-keygen -Y sign -n bay-producer`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Top-level fields this version does not know, kept as read: they are part of the signed
    /// payload, so a record a later version wrote still verifies here.
    #[serde(flatten)]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

impl EvidenceRecord {
    /// The bytes a producer signs: the canonical JSON of the record without its signature.
    ///
    /// # Errors
    ///
    /// The record does not serialize.
    pub fn signing_payload(&self) -> Result<Vec<u8>, String> {
        let mut value = serde_json::to_value(self).map_err(|error| error.to_string())?;
        if let Some(map) = value.as_object_mut() {
            map.remove("signature");
        }
        Ok(crate::canonical::canonical(&value)?.into_bytes())
    }

    /// Signs the record with the producer's key.
    ///
    /// # Errors
    ///
    /// The key cannot sign.
    pub fn sign(&mut self, key: &std::path::Path) -> Result<(), String> {
        self.signature = None;
        let payload = self.signing_payload()?;
        self.signature = Some(crate::signing::sign_data(
            key,
            crate::registry::KeyClass::Producer.namespace(),
            &payload,
        )?);
        Ok(())
    }

    /// Whether the record is signed by a key in the producer file under its producer's name.
    #[must_use]
    pub fn verifies(&self, producer_file: &std::path::Path) -> bool {
        let (Some(signature), Ok(payload)) = (&self.signature, self.signing_payload()) else {
            return false;
        };
        crate::signing::verify_data(
            producer_file,
            &self.producer.name,
            crate::registry::KeyClass::Producer.namespace(),
            &payload,
            signature,
        )
    }
}

#[must_use]
pub fn notes_ref(producer: &str) -> String {
    format!("{EVIDENCE_PREFIX}{producer}")
}

/// The records `producer` holds on `tree`.
///
/// # Errors
///
/// A git fault, or a note that is not a list of records.
pub fn producer_records(
    git: &Git,
    producer: &str,
    tree: &str,
) -> Result<Vec<EvidenceRecord>, String> {
    let notes = notes_ref(producer);
    match git.run(&["notes", &format!("--ref={notes}"), "show", tree]) {
        Ok(text) => {
            serde_json::from_str(&text).map_err(|error| format!("{notes} on {tree}: {error}"))
        }
        Err(GitError::Failed { .. }) => Ok(Vec::new()),
        Err(error) => Err(error.message()),
    }
}

/// Appends `record` to `producer`'s note on its subject tree.
///
/// # Errors
///
/// A git fault, or an existing record with the same id and different content.
pub fn append(git: &Git, producer: &str, record: &EvidenceRecord) -> Result<(), String> {
    let tree = &record.subject.tree;
    let mut records = producer_records(git, producer, tree)?;
    if let Some(existing) = records.iter().find(|existing| existing.id == record.id) {
        if existing == record {
            return Ok(());
        }
        return Err(format!(
            "two different records under one evidence id {}",
            record.id
        ));
    }
    records.push(record.clone());
    let mut text = serde_json::to_string_pretty(&records).map_err(|error| error.to_string())?;
    text.push('\n');
    git.run_with(
        &[
            "notes",
            &format!("--ref={}", notes_ref(producer)),
            "add",
            "-f",
            "-F",
            "-",
            tree,
        ],
        RECORD_IDENTITY,
        Some(text.as_bytes()),
    )
    .map(|_| ())
    .map_err(|error| error.message())
}

/// Every record on `tree`, from every producer ref.
///
/// # Errors
///
/// A git fault or an unreadable note.
pub fn records_for(git: &Git, tree: &str) -> Result<Vec<EvidenceRecord>, String> {
    let refs = git
        .run(&["for-each-ref", "--format=%(refname)", EVIDENCE_PREFIX])
        .map_err(|error| error.message())?;
    let mut all = Vec::new();
    for name in refs.lines() {
        let producer = name.trim_start_matches(EVIDENCE_PREFIX);
        // A record counts only from its own producer's ref, under its own namespaced id.
        all.extend(
            without_conflicts(producer_records(git, producer, tree)?)
                .into_iter()
                .filter(|record| {
                    record.producer.name == producer
                        && record.id.starts_with(&format!("{producer}/"))
                }),
        );
    }
    Ok(all)
}

/// The records that count: with a registry, only those signed by a producer key under their
/// producer's name (§ 7); without one, bay runs unsigned and every record counts.
///
/// # Errors
///
/// The producer class file cannot be written.
pub fn counted(
    git: &Git,
    records: Vec<EvidenceRecord>,
    registry: Option<&crate::registry::Registry>,
) -> Result<Vec<EvidenceRecord>, String> {
    let Some(registry) = registry else {
        return Ok(records);
    };
    let producers = registry.class_file(git, crate::registry::KeyClass::Producer)?;
    Ok(records
        .into_iter()
        .filter(|record| record.verifies(&producers))
        .collect())
}

/// The combined verdict for one check on one tree under one manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Combined {
    Pass,
    ProductFailure,
    Flaky,
    Unknown,
}

/// Combines records per check id and environment (`DESIGN` § 7): `pass` needs a
/// `pass` and no `product_failure`; a `product_failure` beside a `pass` is flaky; `unknown`
/// and `infra_failure` never cancel a `pass`. Records made under another manifest do not count.
/// A check keyed by commit (`subject_commit`) counts only records for that commit, so a
/// failure on one commit is never charged to another that shares its tree.
#[must_use]
pub fn combine(
    records: &[EvidenceRecord],
    check: &CheckId,
    manifest_digest: &str,
    subject_commit: Option<&str>,
) -> Combined {
    let relevant = records.iter().filter(|record| {
        &record.check == check
            && record.manifest_digest == manifest_digest
            && subject_commit.is_none_or(|commit| record.subject.commit == commit)
    });
    let (mut pass, mut failure) = (false, false);
    for record in relevant {
        match record.verdict {
            Verdict::Pass => pass = true,
            Verdict::ProductFailure => failure = true,
            Verdict::InfraFailure | Verdict::Unknown => {}
        }
    }
    match (pass, failure) {
        (true, false) => Combined::Pass,
        (true, true) => Combined::Flaky,
        (false, true) => Combined::ProductFailure,
        (false, false) => Combined::Unknown,
    }
}

/// The aggregate over a frozen manifest (G30): complete only when every required (check id,
/// environment) combines to `pass`. Counts are never the measure: twelve green checks may be
/// the wrong twelve.
#[must_use]
pub fn aggregate(
    records: &[EvidenceRecord],
    manifest: &[crate::policy::ManifestEntry],
    manifest_digest: &str,
    commit_keyed: &dyn Fn(&CheckId) -> bool,
    candidate_commit: &str,
) -> Vec<(CheckId, Combined)> {
    manifest
        .iter()
        .map(|entry| CheckId {
            id: entry.id.clone(),
            environment: entry.environment.clone(),
        })
        .map(|id| {
            let subject = commit_keyed(&id).then_some(candidate_commit);
            let combined = combine(records, &id, manifest_digest, subject);
            (id, combined)
        })
        .filter(|(_, combined)| *combined != Combined::Pass)
        .collect()
}

/// Drops every record whose id appears with two different contents: two records under one
/// evidence id are refused, both of them (G29).
#[must_use]
pub fn without_conflicts(records: Vec<EvidenceRecord>) -> Vec<EvidenceRecord> {
    let conflicting: Vec<String> = records
        .iter()
        .filter(|record| {
            records
                .iter()
                .any(|other| other.id == record.id && other != *record)
        })
        .map(|record| record.id.clone())
        .collect();
    records
        .into_iter()
        .filter(|record| !conflicting.contains(&record.id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        CheckId, Combined, EvidenceRecord, Producer, Subject, Verdict, combine, without_conflicts,
    };

    fn record(id: &str, environment: &str, commit: &str, verdict: Verdict) -> EvidenceRecord {
        EvidenceRecord {
            schema: super::EVIDENCE_SCHEMA.to_owned(),
            id: id.to_owned(),
            kind: "check".to_owned(),
            subject: Subject {
                tree: "t".to_owned(),
                commit: commit.to_owned(),
                base_tree: "b".to_owned(),
            },
            check: CheckId {
                id: "tests".to_owned(),
                environment: environment.to_owned(),
            },
            policy_revision: 1,
            manifest_digest: "m".to_owned(),
            policy_provenance: None,
            producer: Producer {
                name: "ci".to_owned(),
                version: "1".to_owned(),
            },
            verdict,
            at: "now".to_owned(),
            why: None,
            units: Vec::new(),
            selection: None,
            sandbox: None,
            signature: None,
            unknown: serde_json::Map::new(),
        }
    }

    fn tests_on(environment: &str) -> CheckId {
        CheckId {
            id: "tests".to_owned(),
            environment: environment.to_owned(),
        }
    }

    #[test]
    fn an_older_pass_and_a_newer_unknown_or_infra_failure_is_pass() {
        for later in [Verdict::Unknown, Verdict::InfraFailure] {
            let records = [
                record("ci/1", "x", "c", Verdict::Pass),
                record("ci/2", "x", "c", later),
            ];
            assert_eq!(combine(&records, &tests_on("x"), "m", None), Combined::Pass);
        }
    }

    #[test]
    fn a_pass_and_a_product_failure_in_either_order_is_flaky() {
        let one = [
            record("ci/1", "x", "c", Verdict::Pass),
            record("ci/2", "x", "c", Verdict::ProductFailure),
        ];
        let two = [
            record("ci/1", "x", "c", Verdict::ProductFailure),
            record("ci/2", "x", "c", Verdict::Pass),
        ];
        assert_eq!(combine(&one, &tests_on("x"), "m", None), Combined::Flaky);
        assert_eq!(combine(&two, &tests_on("x"), "m", None), Combined::Flaky);
    }

    #[test]
    fn a_pass_on_one_environment_does_not_satisfy_another() {
        let records = [record("ci/1", "x86", "c", Verdict::Pass)];
        assert_eq!(
            combine(&records, &tests_on("aarch64"), "m", None),
            Combined::Unknown
        );
    }

    #[test]
    fn a_commit_keyed_failure_is_not_charged_to_another_commit_with_the_same_tree() {
        let records = [record("ci/1", "x", "commit-a", Verdict::ProductFailure)];
        assert_eq!(
            combine(&records, &tests_on("x"), "m", Some("commit-b")),
            Combined::Unknown
        );
        assert_eq!(
            combine(&records, &tests_on("x"), "m", Some("commit-a")),
            Combined::ProductFailure
        );
    }

    #[test]
    fn all_green_with_a_required_check_missing_is_incomplete() {
        use crate::policy::ManifestEntry;
        let manifest = [
            ManifestEntry {
                id: "tests".to_owned(),
                environment: "x".to_owned(),
                definition_digest: "d1".to_owned(),
            },
            ManifestEntry {
                id: "lint".to_owned(),
                environment: "x".to_owned(),
                definition_digest: "d2".to_owned(),
            },
        ];
        let records = [
            record("ci/1", "x", "c", Verdict::Pass),
            record("ci/2", "x", "c", Verdict::Pass),
        ];
        let gaps = super::aggregate(&records, &manifest, "m", &|_| false, "c");
        assert_eq!(
            gaps.len(),
            1,
            "two green records of one check do not cover two checks"
        );
        assert_eq!(gaps[0].0.id, "lint");
        assert_eq!(gaps[0].1, Combined::Unknown);
    }

    /// A record a later version wrote, with a top-level field this version does not know,
    /// signs over the same bytes once read here: the field is kept, so its signature still
    /// verifies on a box running an older bay.
    #[test]
    fn a_field_this_version_does_not_know_stays_in_the_signed_payload() {
        let mut written = serde_json::to_value(record("ci/1", "x", "c", Verdict::Pass)).unwrap();
        written["a_later_field"] = serde_json::json!({"since": "0.2"});
        let expected = crate::canonical::canonical(&written).unwrap().into_bytes();
        let read: EvidenceRecord = serde_json::from_value(written).unwrap();
        assert_eq!(read.signing_payload().unwrap(), expected);
    }

    #[test]
    fn two_records_under_one_id_are_both_refused() {
        let records = vec![
            record("ci/1", "x", "c", Verdict::Pass),
            record("ci/1", "x", "c", Verdict::ProductFailure),
            record("ci/2", "x", "c", Verdict::Unknown),
        ];
        let kept = without_conflicts(records);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "ci/2");
    }
}
