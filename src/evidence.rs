//! Evidence records and their notes (`DESIGN` § 5, § 7; `INTERFACES` § 6).
//!
//! Each producer writes only `refs/notes/bay/evidence/<producer>`. A note sits on the result
//! **tree** and holds a JSON list of records. A reader takes the union over every producer
//! ref and combines records per check id and environment; never latest-wins.

use serde::{Deserialize, Serialize};

use crate::change::RECORD_IDENTITY;
use crate::git::{Git, GitError};
use crate::registry::{KeyClass, Registry};

pub const EVIDENCE_PREFIX: &str = crate::names::EVIDENCE;
pub const EVIDENCE_SCHEMA: &str = "bay.evidence/1";
/// The kind of a check record: the only kind a check's verdict is combined from. A runtime
/// record (`running`, `effective`; [`crate::runtime`]) or a review never stands for a check.
pub const KIND_CHECK: &str = "check";
/// The kind of an outside review record. It verifies only against the reviewer class file, and
/// it never combines into a verdict (ruled 2026-10-19, rulings-open-questions.md Q12).
pub const KIND_REVIEW: &str = "review";

/// The key class a record of `kind` must be signed by (Q12): the reviewer class for a `review`
/// record, the producer class for every other kind. A reviewer key therefore never signs a
/// record that counts toward a check, and a producer key's `review` is unverifiable.
#[must_use]
pub fn signer_class(kind: &str) -> KeyClass {
    if kind == KIND_REVIEW {
        KeyClass::Reviewer
    } else {
        KeyClass::Producer
    }
}

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
    /// Fields this version does not know, kept as read: they are signed, so a record a later
    /// version wrote still verifies here.
    #[serde(flatten)]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

/// A (check id, environment) pair. Two are the same check when id and environment match;
/// fields a later version added do not make it another check.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckId {
    pub id: String,
    pub environment: String,
    /// Fields this version does not know, kept as read: they are signed, so a record a later
    /// version wrote still verifies here.
    #[serde(flatten)]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

impl CheckId {
    #[must_use]
    pub fn new(id: &str, environment: &str) -> Self {
        Self {
            id: id.to_owned(),
            environment: environment.to_owned(),
            unknown: serde_json::Map::new(),
        }
    }
}

impl PartialEq for CheckId {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.environment == other.environment
    }
}

impl Eq for CheckId {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Producer {
    pub name: String,
    pub version: String,
    /// Fields this version does not know, kept as read: they are signed, so a record a later
    /// version wrote still verifies here.
    #[serde(flatten)]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Unit {
    pub id: String,
    pub verdict: Verdict,
    /// The unit's own time as the adapter reported it (`INTERFACES` § 3). Absent when the
    /// adapter sent none, and on records written before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f64>,
    /// Why the unit did not pass, as the adapter chose to show it (G240, at most
    /// [`REASON_MAX_BYTES`]). Absent on a pass and on records written before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Fields this version does not know, kept as read: they are signed, so a record a later
    /// version wrote still verifies here.
    #[serde(flatten)]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

/// The longest unit `reason` an adapter may send, in bytes of UTF-8 (G240). A longer one makes
/// the run `infra_failure`; bay never cuts it.
pub const REASON_MAX_BYTES: usize = 2048;

/// The units of `record` that did not pass and say why, as `detail.checks[].reasons` and
/// `bay status` show them (G240).
#[must_use]
pub fn reasons(units: &[Unit]) -> Vec<serde_json::Value> {
    units
        .iter()
        .filter(|unit| unit.verdict != Verdict::Pass)
        .filter_map(|unit| {
            unit.reason.as_ref().map(|reason| {
                serde_json::json!({"unit": unit.id, "verdict": unit.verdict, "reason": reason})
            })
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvidenceRecord {
    pub schema: String,
    /// `<producer>/<id>`.
    pub id: String,
    pub kind: String,
    pub subject: Subject,
    /// What the check read besides the subject (`INTERFACES` § 6): `{"fixtures": [...]}`.
    /// Absent when it named none, and on records written before the field existed. Kept as
    /// read, whatever its shape: it is signed, and a shape this version cannot read makes
    /// only this record unreusable ([`EvidenceRecord::fixtures`]), never the note unreadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<serde_json::Value>,
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
    /// Worker wall time of this check in seconds, measured by bay around the adapter run
    /// (`ROADMAP` § 3). Absent on records written before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f64>,
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
    /// The `inputs` of a check that read `fixtures`: none when it read none.
    #[must_use]
    pub fn inputs_for(fixtures: &[String]) -> Option<serde_json::Value> {
        (!fixtures.is_empty()).then(|| serde_json::json!({ "fixtures": fixtures }))
    }

    /// The fixtures the record says its check read: empty on a record without `inputs`.
    /// `None` when bay cannot identify them: `inputs` is not an object, holds a key other than
    /// `fixtures` (an input this version cannot compare), or `fixtures` is not a list of
    /// strings. Such a record is never reused (G7): an unknown input never matches.
    #[must_use]
    pub fn fixtures(&self) -> Option<Vec<String>> {
        let Some(inputs) = &self.inputs else {
            return Some(Vec::new());
        };
        let inputs = inputs.as_object()?;
        if inputs.keys().any(|key| key != "fixtures") {
            return None;
        }
        let Some(fixtures) = inputs.get("fixtures") else {
            return Some(Vec::new());
        };
        fixtures
            .as_array()?
            .iter()
            .map(|fixture| fixture.as_str().map(str::to_owned))
            .collect()
    }

    /// The artifact digests the record names (`INTERFACES` § 6 `artifacts`), each a string
    /// `sha256:<64 hex>` or an object whose `digest` is one (the § 3 shape). `artifacts` stays
    /// in `unknown` as written, so it signs over the bytes its producer wrote. `None` when the
    /// field is absent or empty, or when any entry is not a digest: a list bay cannot read whole
    /// names nothing.
    #[must_use]
    pub fn artifact_digests(&self) -> Option<Vec<&str>> {
        let digests: Vec<&str> = self
            .unknown
            .get("artifacts")?
            .as_array()?
            .iter()
            .map(|entry| {
                entry
                    .as_str()
                    .or_else(|| entry.get("digest").and_then(serde_json::Value::as_str))
                    .filter(|digest| is_sha256_digest(digest))
            })
            .collect::<Option<_>>()?;
        (!digests.is_empty()).then_some(digests)
    }

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

    /// Signs the record with `key`, under the namespace of the class its kind needs
    /// ([`signer_class`]): `bay-reviewer` for a `review`, `bay-producer` otherwise.
    ///
    /// # Errors
    ///
    /// The key cannot sign.
    pub fn sign(&mut self, key: &std::path::Path) -> Result<(), String> {
        self.signature = None;
        let payload = self.signing_payload()?;
        self.signature = Some(crate::signing::sign_data(
            key,
            signer_class(&self.kind).namespace(),
            &payload,
        )?);
        Ok(())
    }

    /// Whether the record is signed by a key in `class_file` under its producer's name, in the
    /// namespace of the class its kind needs. The caller names the file of that class
    /// ([`EvidenceRecord::verifies_in`]).
    #[must_use]
    pub fn verifies(&self, class_file: &std::path::Path) -> bool {
        let (Some(signature), Ok(payload)) = (&self.signature, self.signing_payload()) else {
            return false;
        };
        crate::signing::verify_data(
            class_file,
            &self.producer.name,
            signer_class(&self.kind).namespace(),
            &payload,
            signature,
        )
    }

    /// Whether the record verifies under `registry` against the one class file its kind names
    /// (Q12): a `review` against the reviewer file, every other kind against the producer file.
    ///
    /// # Errors
    ///
    /// The class file cannot be written.
    pub fn verifies_in(&self, git: &Git, registry: &Registry) -> Result<bool, String> {
        let file = registry.class_file(git, signer_class(&self.kind))?;
        Ok(self.verifies(&file))
    }
}

/// Whether `text` is `sha256:` and 64 lowercase hex digits.
fn is_sha256_digest(text: &str) -> bool {
    text.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
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
        // G104b: a note whose blob the store lacks (and no fetch by id could bring) is
        // unreadable, never "no records".
        Err(error @ GitError::Failed { .. }) if failed_on_absent_note(&error) => {
            Err(format!("{notes} on {tree}: {}", error.message()))
        }
        Err(GitError::Failed { .. }) => Ok(Vec::new()),
        Err(error) => Err(error.message()),
    }
}

/// Whether a failed `notes show` found a note whose blob the store cannot read: Git says
/// `fatal: bad object <blob>`, where a tree with no note says `no note found`.
fn failed_on_absent_note(error: &GitError) -> bool {
    matches!(error, GitError::Failed { stderr, .. } if stderr.contains("fatal: bad object "))
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

/// Every record on `tree`, from every producer ref, of the schema this bay reads.
///
/// G196: one `for-each-ref` lists the producer refs and one `git cat-file --batch` reads every
/// producer's note ([`notes_on`]), so the read spawns two `git` processes however many
/// producers there are.
///
/// # Errors
///
/// A git fault or an unreadable note.
pub fn records_for(git: &Git, tree: &str) -> Result<Vec<EvidenceRecord>, String> {
    let refs = git
        .run(&["for-each-ref", "--format=%(refname)", EVIDENCE_PREFIX])
        .map_err(|error| error.message())?;
    let producers: Vec<&str> = refs
        .lines()
        .map(|name| name.trim_start_matches(EVIDENCE_PREFIX))
        .collect();
    let mut all = Vec::new();
    for (producer, records) in producers.iter().zip(notes_on(git, &producers, tree)?) {
        // A record counts only from its own producer's ref, under its own namespaced id, and
        // only on the note of the tree it is about: a record copied onto another tree's note
        // says nothing about that tree.
        all.extend(without_conflicts(records).into_iter().filter(|record| {
            // G86: a record of a schema major this bay does not know may mean something
            // it cannot judge, so it never counts here; it stays on the note.
            record.schema == EVIDENCE_SCHEMA
                && record.producer.name == *producer
                && record.id.starts_with(&format!("{producer}/"))
                && record.subject.tree == tree
        }));
    }
    Ok(all)
}

/// G196: the records each of `producers` holds on `tree`, in their order, as
/// [`producer_records`] reads them one by one, read through one `git cat-file --batch`.
///
/// Git finds a note at any fanout of its notes tree (`<tree>`, `ab/<rest>`, `ab/cd/<rest>`
/// and so on; `git notes add` picks one by the number of notes), so each producer's note is
/// asked at every fanout and found at the one its tree holds. A tree that is not a full object
/// id, or a store `cat-file` cannot read, is read producer by producer as before.
///
/// # Errors
///
/// A git fault, or a note that is not a list of records.
fn notes_on(git: &Git, producers: &[&str], tree: &str) -> Result<Vec<Vec<EvidenceRecord>>, String> {
    let one_by_one = || -> Result<Vec<Vec<EvidenceRecord>>, String> {
        producers
            .iter()
            .map(|producer| producer_records(git, producer, tree))
            .collect()
    };
    if producers.is_empty() {
        return Ok(Vec::new());
    }
    if !crate::compact::is_object_id(tree) {
        return one_by_one();
    }
    let paths = note_paths(tree);
    let mut input = String::new();
    for producer in producers {
        for path in &paths {
            input.push_str(&notes_ref(producer));
            input.push(':');
            input.push_str(path);
            input.push('\n');
        }
    }
    let out = match git.run_bytes(&["cat-file", "--batch"], &[], Some(input.as_bytes())) {
        Ok(out) => out,
        Err(GitError::Failed { .. }) => return one_by_one(),
        Err(error) => return Err(error.message()),
    };
    let mut replies = out.as_slice();
    producers
        .iter()
        .map(|producer| {
            let notes = notes_ref(producer);
            let mut found = None;
            for _ in &paths {
                if let Some(blob) = next_reply(&mut replies, &notes, tree)? {
                    if found.is_some() {
                        return Err(format!("{notes} on {tree}: more than one note"));
                    }
                    found = Some(blob);
                }
            }
            let Some(blob) = found else {
                return Ok(Vec::new());
            };
            let text = std::str::from_utf8(blob)
                .map_err(|_| format!("{notes} on {tree}: the note is not utf-8"))?;
            serde_json::from_str(text).map_err(|error| format!("{notes} on {tree}: {error}"))
        })
        .collect()
}

/// Every path a note on `tree` may sit at in a notes tree: flat, then each fanout of two hex
/// digits per level, as Git's notes reader accepts them.
fn note_paths(tree: &str) -> Vec<String> {
    (0..tree.len() / 2)
        .map(|levels| {
            let mut path = String::with_capacity(tree.len() + levels);
            for level in 0..levels {
                path.push_str(&tree[level * 2..level * 2 + 2]);
                path.push('/');
            }
            path.push_str(&tree[levels * 2..]);
            path
        })
        .collect()
}

/// The next reply of a `git cat-file --batch` in `replies`: the blob's contents, or `None`
/// when the name it was given is `missing`. `replies` moves past it.
fn next_reply<'a>(
    replies: &mut &'a [u8],
    notes: &str,
    tree: &str,
) -> Result<Option<&'a [u8]>, String> {
    let unreadable = || format!("{notes} on {tree}: git cat-file gave no full reply");
    let end = replies
        .iter()
        .position(|&byte| byte == b'\n')
        .ok_or_else(unreadable)?;
    let header = String::from_utf8_lossy(&replies[..end]).into_owned();
    *replies = &replies[end + 1..];
    let words: Vec<&str> = header.split(' ').collect();
    match words.as_slice() {
        [_, "missing"] => Ok(None),
        [_, kind, size] => {
            let size: usize = size.parse().map_err(|_| unreadable())?;
            if replies.len() <= size || replies[size] != b'\n' {
                return Err(unreadable());
            }
            let body = &replies[..size];
            *replies = &replies[size + 1..];
            if *kind == "blob" {
                Ok(Some(body))
            } else {
                Err(format!(
                    "{notes} on {tree}: the note is a {kind}, not a blob"
                ))
            }
        }
        _ => Err(format!("{notes} on {tree}: git cat-file gave {header:?}")),
    }
}

/// The records that count: with a registry, only those signed under their producer's name by a
/// key of the class their kind needs (§ 7, Q12); without one, bay runs unsigned and every
/// record counts.
///
/// # Errors
///
/// A class file cannot be written.
pub fn counted(
    git: &Git,
    records: Vec<EvidenceRecord>,
    registry: Option<&Registry>,
) -> Result<Vec<EvidenceRecord>, String> {
    Ok(tagged(git, records, registry)?
        .into_iter()
        .filter(|(_, verified)| *verified != Verified::Unverifiable)
        .map(|(record, _)| record)
        .collect())
}

/// How one record stands against the registry, as every reader names it (G160).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verified {
    /// There is no registry: nothing can verify the record, and it counts unsigned.
    Unsigned,
    /// Signed under its producer's name by a key of the class its kind needs.
    Verified,
    /// A registry exists and the record does not verify under it: it never counts.
    Unverifiable,
}

/// The trust a result names for the evidence it reports (G160): `unsigned` without a registry,
/// where every record counts unattested, and `signed` with one, where only verified records
/// count.
#[must_use]
pub const fn trust(registry: Option<&Registry>) -> &'static str {
    if registry.is_some() {
        "signed"
    } else {
        "unsigned"
    }
}

/// Each record with how it stands: with a registry, verified or unverifiable against the class
/// file its kind names (§ 7, Q12); without one, every record is unsigned.
///
/// # Errors
///
/// A class file cannot be written.
pub fn tagged(
    git: &Git,
    records: Vec<EvidenceRecord>,
    registry: Option<&Registry>,
) -> Result<Vec<(EvidenceRecord, Verified)>, String> {
    let Some(registry) = registry else {
        return Ok(records
            .into_iter()
            .map(|record| (record, Verified::Unsigned))
            .collect());
    };
    records
        .into_iter()
        .map(|record| {
            let verified = if record.verifies_in(git, registry)? {
                Verified::Verified
            } else {
                Verified::Unverifiable
            };
            Ok((record, verified))
        })
        .collect()
}

/// What a record made by another operation must match to stand for a new run of a check
/// (`DESIGN` § 7 Reuse, G7). The manifest digest, the check and, for a commit-keyed check, the
/// commit are matched by [`combine`].
pub struct ReuseKey<'a> {
    /// The candidate's tree.
    pub tree: &'a str,
    /// The tree of the trunk commit the candidate was merged onto. The adapter is given the
    /// paths the candidate changes against it (`changed_paths`, `INTERFACES` § 3) and may judge
    /// by them, so a verdict made on another base is never reused.
    pub base_tree: &'a str,
    pub policy_revision: u64,
    /// `bay` or `outside_bay` (`INTERFACES` § 6): a pass under an `outside_bay` policy is never
    /// read as one under a policy bay checked.
    pub policy_provenance: &'a str,
    /// The fixtures the check's policy entry names.
    pub fixtures: &'a [String],
}

/// The `records` that may stand for a new run under `key`: about the same tree on the same
/// base, under the same policy revision and provenance, with the same fixtures. A record whose
/// inputs bay cannot identify matches nothing.
#[must_use]
pub fn reusable(records: &[EvidenceRecord], key: &ReuseKey) -> Vec<EvidenceRecord> {
    records
        .iter()
        .filter(|record| {
            record.subject.tree == key.tree
                && record.subject.base_tree == key.base_tree
                && record.policy_revision == key.policy_revision
                && record.policy_provenance.as_deref() == Some(key.policy_provenance)
                && record.fixtures().as_deref() == Some(key.fixtures)
        })
        .cloned()
        .collect()
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
/// failure on one commit is never charged to another that shares its tree. Only records of
/// kind `check` combine: a runtime or review record that names the check is never its pass.
#[must_use]
pub fn combine(
    records: &[EvidenceRecord],
    check: &CheckId,
    manifest_digest: &str,
    subject_commit: Option<&str>,
) -> Combined {
    let relevant = records.iter().filter(|record| {
        record.kind == KIND_CHECK
            && &record.check == check
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
        .map(|entry| CheckId::new(&entry.id, &entry.environment))
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
                unknown: serde_json::Map::new(),
            },
            inputs: None,
            check: CheckId::new("tests", environment),
            policy_revision: 1,
            manifest_digest: "m".to_owned(),
            policy_provenance: None,
            producer: Producer {
                name: "ci".to_owned(),
                version: "1".to_owned(),
                unknown: serde_json::Map::new(),
            },
            verdict,
            at: "now".to_owned(),
            why: None,
            units: Vec::new(),
            selection: None,
            sandbox: None,
            seconds: None,
            signature: None,
            unknown: serde_json::Map::new(),
        }
    }

    fn tests_on(environment: &str) -> CheckId {
        CheckId::new("tests", environment)
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

    /// `artifacts` in any shape another producer writes, an empty list or a list of objects,
    /// is read and signs over the bytes it was written with.
    #[test]
    fn artifacts_in_any_shape_stay_in_the_signed_payload() {
        for artifacts in [
            serde_json::json!([]),
            serde_json::json!([{"name": "app", "digest": "sha256:00"}]),
        ] {
            let mut written =
                serde_json::to_value(record("ci/1", "x", "c", Verdict::Pass)).unwrap();
            written["artifacts"] = artifacts.clone();
            let expected = crate::canonical::canonical(&written).unwrap().into_bytes();
            let read: EvidenceRecord = serde_json::from_value(written).unwrap();
            assert_eq!(read.signing_payload().unwrap(), expected, "{artifacts}");
        }
    }

    /// A runtime record that names a check's id and environment, under its manifest, with a
    /// `pass`, is an observation of a deployment, never that check's pass.
    #[test]
    fn a_runtime_record_never_combines_as_a_check() {
        for kind in ["running", "effective", "review"] {
            let mut other = record("ci/1", "x", "c", Verdict::Pass);
            other.kind = kind.to_owned();
            assert_eq!(
                combine(&[other], &tests_on("x"), "m", None),
                Combined::Unknown,
                "{kind}"
            );
        }
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

    #[test]
    fn fields_this_version_does_not_know_are_kept_in_the_signed_payload() {
        let mut timed = record("ci/1", "x", "c", Verdict::Pass);
        timed.seconds = Some(0.041);
        timed.sandbox = Some("none".to_owned());
        let mut value = serde_json::to_value(timed).unwrap();
        value["future"] = serde_json::json!(1);
        value["subject"]["future"] = serde_json::json!(true);
        value["check"]["future"] = serde_json::json!("c");
        value["producer"]["future"] = serde_json::json!(null);
        value["inputs"] = serde_json::json!({"fixtures": ["data@sha256:ab"], "plan": "default@3"});
        value["units"] =
            serde_json::json!([{"id": "t1", "verdict": "pass", "seconds": 1.0, "future": [1.5]}]);
        let signed = crate::canonical::canonical(&value).unwrap();
        value["signature"] = serde_json::json!("sig");
        let read: EvidenceRecord = serde_json::from_value(value).unwrap();
        let payload = String::from_utf8(read.signing_payload().unwrap()).unwrap();
        assert_eq!(payload, signed);
    }

    /// G7: a record stands for a new run only when every recorded input matches the key; each
    /// field alone, changed, stops the reuse.
    #[test]
    fn a_record_is_reusable_only_when_every_recorded_input_matches() {
        type Change = fn(&mut EvidenceRecord);
        let fixtures = vec!["data@sha256:aa".to_owned()];
        let mut base = record("ci/1", "x", "c", Verdict::Pass);
        base.policy_provenance = Some("bay".to_owned());
        base.inputs = EvidenceRecord::inputs_for(&fixtures);
        let key = super::ReuseKey {
            tree: "t",
            base_tree: "b",
            policy_revision: 1,
            policy_provenance: "bay",
            fixtures: &fixtures,
        };
        assert_eq!(super::reusable(std::slice::from_ref(&base), &key).len(), 1);
        let changes: [(&str, Change); 8] = [
            ("another tree", |r| r.subject.tree = "u".to_owned()),
            ("another base", |r| r.subject.base_tree = "a".to_owned()),
            ("another revision", |r| r.policy_revision = 2),
            ("outside_bay", |r| {
                r.policy_provenance = Some("outside_bay".to_owned());
            }),
            ("no provenance", |r| r.policy_provenance = None),
            ("no fixtures", |r| r.inputs = None),
            ("another fixture", |r| {
                r.inputs = Some(serde_json::json!({"fixtures": ["data@sha256:bb"]}));
            }),
            ("an input bay cannot compare", |r| {
                r.inputs = Some(serde_json::json!({"fixtures": ["data@sha256:aa"], "net": 1}));
            }),
        ];
        for (what, change) in changes {
            let mut other = base.clone();
            change(&mut other);
            assert!(super::reusable(&[other], &key).is_empty(), "{what}");
        }
    }

    /// A record whose `inputs` has a shape this version cannot read still parses (the note
    /// stays readable) and keeps its signed bytes, but its fixtures are unknown.
    #[test]
    fn inputs_of_another_shape_leave_the_note_readable_and_the_fixtures_unknown() {
        let mut value = serde_json::to_value(record("ci/1", "x", "c", Verdict::Pass)).unwrap();
        for inputs in [
            serde_json::json!({"fixtures": [{"name": "data", "digest": "sha256:aa"}]}),
            serde_json::json!({"fixtures": "data@sha256:aa"}),
            serde_json::json!(["data@sha256:aa"]),
        ] {
            value["inputs"] = inputs.clone();
            let read: Vec<EvidenceRecord> =
                serde_json::from_value(serde_json::json!([value])).unwrap();
            assert_eq!(read[0].inputs.as_ref(), Some(&inputs));
            assert_eq!(read[0].fixtures(), None, "{inputs}");
        }
        let plain = record("ci/1", "x", "c", Verdict::Pass);
        assert_eq!(plain.fixtures(), Some(Vec::new()));
    }

    /// A record is about the tree its subject names: copied onto another tree's note, it does
    /// not count there, so a pass for one tree never answers for another (G7, G29).
    #[test]
    fn a_record_copied_onto_another_trees_note_does_not_count_there() {
        use crate::change::RECORD_IDENTITY;
        let dir = tempfile::TempDir::new().unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(dir.path())
            .status()
            .unwrap();
        assert!(init.success());
        let git = crate::git::Git::new(dir.path());
        let blob = git
            .run_with(&["hash-object", "-w", "--stdin"], &[], Some(b"x\n"))
            .unwrap();
        let entry = format!("100644 blob {}\tx\n", blob.trim());
        let one = git.run_with(&["mktree"], &[], Some(b"")).unwrap();
        let two = git
            .run_with(&["mktree"], &[], Some(entry.as_bytes()))
            .unwrap();
        let (one, two) = (one.trim(), two.trim());
        let mut passed = record("ci/1", "x", "c", Verdict::Pass);
        passed.subject.tree = one.to_owned();
        super::append(&git, "ci", &passed).unwrap();
        git.run_with(
            &[
                "notes",
                "--ref=refs/notes/bay/evidence/ci",
                "copy",
                one,
                two,
            ],
            RECORD_IDENTITY,
            None,
        )
        .unwrap();
        assert_eq!(super::producer_records(&git, "ci", two).unwrap().len(), 1);
        assert_eq!(super::records_for(&git, one).unwrap(), vec![passed]);
        assert_eq!(super::records_for(&git, two).unwrap(), Vec::new());
    }
}
