//! `bay evidence record` (G75): the supported write path for a record from a producer outside
//! bay, such as a reviewer (`INTERFACES` § 6; the review dossier's corrections K4).
//!
//! One call writes one record per named landing, then pushes the writer's notes ref once. The
//! record is about the landing's diff: its subject is the commit, its tree, and the tree of its
//! first parent. Its policy fields name the policy at that parent, the landing's accepted base.
//!
//! - **The id is deterministic:** `<writer>/<kind>-<commit>-<digest>`, the digest over the
//!   record's inputs and the writer's version. A rerun with the same content is a no-op. A
//!   different record under one id is a conflict, refused before anything is written. A
//!   re-score under a new writer version gets a new id.
//! - **The verdict has no default:** the caller names it.
//! - **Unknown fields are signed:** the body's fields (for example a `findings` array) are
//!   kept beside the record's own, inside the canonical signed bytes. A body field that names a
//!   field of the record itself is refused.
//! - **Size:** a record whose JSON exceeds [`RECORD_MAX_BYTES`] is refused with the limit
//!   named, never cut.
//! - **Signer:** a `review` record is signed by the box's reviewer key (`bay.reviewerName`,
//!   `bay.reviewerKey`), an `approval` by its approver key (`bay.approverName`,
//!   `bay.approverKey`; a `waiver` too), every other kind by its producer key (`bay.producerName`,
//!   `bay.producerKey`), as the readers verify them (Q12, G245).
//! - **An approval** (G245) is about a candidate, not a landing: `--commit` names the candidate
//!   commit a land refused with `APPROVAL_PENDING` (its first parent is the trunk it was merged
//!   onto), and `--check` an approval check of the policy there, whose environment it takes when
//!   `--environment` is not named. A candidate that changes the policy is judged by its own new
//!   policy too (§ 7), so it gets one approval record under each policy that has the check.
//! - **A waiver** (N-9) is about a candidate too, signed by the approver key: `--check` names a
//!   check marked `waivable = true` in a policy that judges the candidate, its verdict is
//!   `unknown` (a waiver is never a pass), and `--why` is required. `bay approve <change>
//!   --waive <check> --why <text>` writes it.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::clock::now_rfc3339;
use crate::evidence::{self, CheckId, EVIDENCE_SCHEMA, EvidenceRecord, Producer, Subject, Verdict};
use crate::git::Git;
use crate::repo::Repo;
use crate::result::BayResult;

/// The largest record `bay evidence record` writes, as JSON bytes. A note holds every record
/// one writer has on one tree, and every reader parses the whole note.
pub const RECORD_MAX_BYTES: usize = 64 * 1024;

/// What `bay evidence record` asks.
pub struct Record {
    pub kind: String,
    /// `pass`, `product_failure`, `infra_failure` or `unknown`.
    pub verdict: String,
    /// The landings the record is about, each a commit on trunk.
    pub commits: Vec<String>,
    /// The check id the record answers; the kind when not named.
    pub check: Option<String>,
    /// The environment of that check, as the policy names it; `any` when not named.
    pub environment: Option<String>,
    /// `key=value` pairs, kept as the record's `inputs`.
    pub inputs: Vec<String>,
    /// A JSON object whose fields are added to each record.
    pub body: Option<PathBuf>,
    /// The writer's own version; bay's when not named.
    pub version: Option<String>,
    /// The record's `why`, in the writer's words (`bay approve --why`, G281); none when not
    /// named. Signed with the record, and outside its id.
    pub why: Option<String>,
}

#[must_use]
pub fn record(dir: &Path, request: &Record) -> BayResult {
    run(dir, request).unwrap_or_else(BayResult::failed)
}

fn run(dir: &Path, request: &Record) -> Result<BayResult, String> {
    let repo = match Repo::open_fetched(dir) {
        Ok(repo) => repo,
        Err(error) => return Ok(error.into_result()),
    };
    let git = &repo.git;
    let (name, key) = writer(git, &request.kind)?;
    let verdict: Verdict = serde_json::from_value(json!(request.verdict)).map_err(|_| {
        format!(
            "verdict {} is not one of pass, product_failure, infra_failure, unknown",
            request.verdict
        )
    })?;
    // N-9: a waiver says nothing of the product, so it is never a pass, and it names its reason.
    if request.kind == evidence::KIND_WAIVER {
        if verdict != Verdict::Unknown {
            return Err(format!(
                "a waiver's verdict is unknown, never {}: a waiver lets a check through as a named gap, it never passes it",
                request.verdict
            ));
        }
        if request
            .why
            .as_deref()
            .is_none_or(|why| why.trim().is_empty())
        {
            return Err(
                "a waiver names its reason: --why <text>, kept in the signed record".to_owned(),
            );
        }
    }
    let common = Common {
        name: name.clone(),
        version: request
            .version
            .clone()
            .unwrap_or_else(|| crate::version::BAY.to_owned()),
        verdict,
        inputs: inputs(&request.inputs)?,
        body: body(request.body.as_deref())?,
    };
    repo.fetch_trunk().map_err(|error| error.message())?;
    repo.fetch_registry();
    // The writer's own notes ref is never overwritten from the remote: a record written here
    // whose push failed would be lost. It only moves forward to the remote's.
    refresh_own(&repo, &name)?;
    repo.fetch_evidence(&crate::signing::Keys::load(git));

    // Every record is built and judged before any is written: a conflict or an oversized
    // record anywhere writes nothing.
    let mut built = Vec::new();
    for commit in &request.commits {
        for mut record in build(git, request, &common, commit)? {
            record.sign(&key)?;
            let size = serde_json::to_vec(&record)
                .map_err(|error| error.to_string())?
                .len();
            if size > RECORD_MAX_BYTES {
                return Err(format!(
                    "the record about {commit} is {size} bytes; the limit is {RECORD_MAX_BYTES}, and a record is never cut"
                ));
            }
            let existing = evidence::producer_records(git, &name, &record.subject.tree)?
                .into_iter()
                .find(|held| held.id == record.id);
            match existing {
                Some(held) if same_content(&held, &record) => built.push((held, false)),
                Some(_) => {
                    return Err(format!(
                        "{} already holds a different record; one id is one record (rerun only with the same content, or under a new --version)",
                        record.id
                    ));
                }
                None => built.push((record, true)),
            }
        }
    }
    let mut written = 0;
    for (record, new) in &built {
        if *new {
            evidence::append(git, &name, record)?;
            written += 1;
        }
    }
    let notes = evidence::notes_ref(&name);
    // Pushed on every run, so a record an earlier run wrote but could not push goes too.
    git.run(&["push", "-q", repo.remote(), &format!("{notes}:{notes}")])
        .map_err(|error| {
            format!(
                "the records are written here but not pushed: {}",
                error.message()
            )
        })?;
    let ids: Vec<&str> = built.iter().map(|(record, _)| record.id.as_str()).collect();
    let mut result = BayResult::ok(json!({
        "records": ids,
        "written": written,
        "unchanged": built.len() - written,
        "notes": notes,
    }));
    result.freshness = repo.freshness();
    Ok(result)
}

/// Brings `name`'s notes ref here up to the remote's, never losing a record written here. The
/// remote's is fetched beside it, under [`crate::names::FETCH_EVIDENCE`]: none here takes the
/// remote's, one behind it fast-forwards, and one ahead of it is kept (records an earlier run
/// could not push). Diverged, neither is changed and nothing is written: another box records
/// under the same name. Best effort when the remote holds none or cannot be reached: the
/// push reports what is wrong.
fn refresh_own(repo: &Repo, name: &str) -> Result<(), String> {
    let git = &repo.git;
    let own = evidence::notes_ref(name);
    let fetched = format!("{}{name}", crate::names::FETCH_EVIDENCE);
    let got = git.run(&[
        "fetch",
        "-q",
        "--no-tags",
        repo.remote(),
        &format!("+{own}:{fetched}"),
    ]);
    let remote = git.ref_target(&fetched).ok().flatten();
    let _ = git.run(&["update-ref", "-d", &fetched]);
    let (Ok(_), Some(remote)) = (got, remote) else {
        return Ok(());
    };
    let update = |old: &str| {
        git.run(&["update-ref", &own, &remote, old])
            .map(|_| ())
            .map_err(|error| error.message())
    };
    let Some(local) = git.ref_target(&own).map_err(|error| error.message())? else {
        // An empty old value: the ref must still not exist.
        return update("");
    };
    if git.is_ancestor(&remote, &local)? {
        // The same, or ahead by records an earlier run wrote here but could not push.
        return Ok(());
    }
    if !git.is_ancestor(&local, &remote)? {
        return Err(format!(
            "{own} has diverged: here it is {local}, on the remote {remote}, and each holds records the other lacks (another box records as {name}); nothing is written and neither is changed"
        ));
    }
    update(&local)
}

/// The name and key that sign a record of `kind`: the reviewer's for `review`, the approver's
/// for `approval`, the producer's for every other kind.
fn writer(git: &Git, kind: &str) -> Result<(String, PathBuf), String> {
    let (name_key, key_key) = match kind {
        evidence::KIND_REVIEW => ("bay.reviewerName", "bay.reviewerKey"),
        evidence::KIND_APPROVAL | evidence::KIND_WAIVER => ("bay.approverName", "bay.approverKey"),
        _ => ("bay.producerName", "bay.producerKey"),
    };
    let get = |key: &str| {
        git.config_get(key)
            .ok()
            .flatten()
            .filter(|value| !value.is_empty())
    };
    let name = get(name_key).ok_or(format!("a {kind} record needs {name_key}"))?;
    crate::coverage::producer_name(&name).map_err(|why| format!("{name_key}: {why}"))?;
    let key = get(key_key).ok_or(format!(
        "a {kind} record is signed by {key_key}, and this box sets none"
    ))?;
    Ok((name, PathBuf::from(key)))
}

/// The `key=value` pairs as the record's `inputs`, or `None` when there are none.
fn inputs(pairs: &[String]) -> Result<Option<Value>, String> {
    if pairs.is_empty() {
        return Ok(None);
    }
    let mut map = Map::new();
    for pair in pairs {
        let (key, value) = pair
            .split_once('=')
            .filter(|(key, _)| !key.is_empty())
            .ok_or(format!("--input {pair} is not key=value"))?;
        if map.insert(key.to_owned(), json!(value)).is_some() {
            return Err(format!("--input names {key} twice"));
        }
    }
    Ok(Some(Value::Object(map)))
}

/// The body's fields, which may not name a field of the record itself.
fn body(path: Option<&Path>) -> Result<Map<String, Value>, String> {
    let Some(path) = path else {
        return Ok(Map::new());
    };
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let Value::Object(map) = serde_json::from_str(&text)
        .map_err(|error| format!("{}: not JSON: {error}", path.display()))?
    else {
        return Err(format!("{}: the body is a JSON object", path.display()));
    };
    let own = serde_json::to_value(placeholder()).map_err(|error| error.to_string())?;
    if let Some(clash) = map
        .keys()
        .find(|key| own.get(key.as_str()).is_some() || *key == "signature")
    {
        return Err(format!(
            "the body names {clash}, a field of the record itself; name it otherwise"
        ));
    }
    Ok(map)
}

/// A record with every field of its own set, so a body field that names one is found.
fn placeholder() -> EvidenceRecord {
    EvidenceRecord {
        schema: String::new(),
        id: String::new(),
        kind: String::new(),
        subject: Subject {
            tree: String::new(),
            commit: String::new(),
            base_tree: String::new(),
            unknown: Map::new(),
        },
        inputs: Some(Value::Null),
        check: CheckId::new("", ""),
        policy_revision: 0,
        manifest_digest: String::new(),
        policy_provenance: Some(String::new()),
        producer: Producer {
            name: String::new(),
            version: String::new(),
            unknown: Map::new(),
        },
        verdict: Verdict::Unknown,
        at: String::new(),
        why: Some(String::new()),
        units: vec![evidence::Unit {
            id: String::new(),
            verdict: Verdict::Unknown,
            seconds: None,
            reason: None,
            unknown: Map::new(),
        }],
        selection: Some(String::new()),
        sandbox: Some(String::new()),
        seconds: Some(0.0),
        signature: None,
        unknown: Map::new(),
    }
}

/// What every record of one call shares.
struct Common {
    name: String,
    version: String,
    verdict: Verdict,
    inputs: Option<Value>,
    body: Map<String, Value>,
}

/// The records about `commit`: one, judged by the policy at its first parent. G245: an
/// approval is one record per policy that judges the candidate and has the approval check, so a
/// candidate that changes the policy, which its own new policy judges too (§ 7), is approved
/// under both.
fn build(
    git: &Git,
    request: &Record,
    common: &Common,
    commit: &str,
) -> Result<Vec<EvidenceRecord>, String> {
    let resolve = |rev: &str| {
        git.run(&["rev-parse", "--verify", "-q", rev])
            .map_err(|_| format!("{commit} is not a commit here"))
    };
    let commit = resolve(&format!("{commit}^{{commit}}"))?;
    let base = resolve(&format!("{commit}^1"))
        .map_err(|_| format!("{commit} has no parent, so no accepted base to judge it from"))?;
    let tree = resolve(&format!("{commit}^{{tree}}"))?;
    let base_tree = resolve(&format!("{base}^{{tree}}"))?;
    let check = request
        .check
        .clone()
        .unwrap_or_else(|| request.kind.clone());
    let judged = if evidence::approver_kind(&request.kind) {
        approval_policies(
            git,
            (base.as_str(), commit.as_str()),
            (tree.as_str(), base_tree.as_str()),
            (request.kind.as_str(), check.as_str()),
            request.environment.as_deref(),
        )?
    } else {
        let policy = crate::policy::load_at(git, &base).map_err(String::from)?;
        let (revision, digest) = policy
            .map(|policy| (policy.revision, policy.manifest_digest))
            .unwrap_or_default();
        let environment = request.environment.as_deref().unwrap_or("any");
        vec![(revision, digest, environment.to_owned(), None)]
    };
    let mut records = Vec::new();
    for (policy_revision, manifest_digest, environment, stamp) in judged {
        let mut unknown = common.body.clone();
        // G292: an approval holds the stamp of what it covers, so it answers a later candidate
        // of the same change whose gated files are byte for byte the same.
        if let Some(stamp) = stamp {
            unknown.insert(crate::throughput::FOOTPRINT.to_owned(), stamp);
        }
        let mut named = json!({"inputs": common.inputs, "version": common.version});
        // Two approvals (or waivers) of one candidate under two policies are two records,
        // never one id.
        if evidence::approver_kind(&request.kind) {
            named["manifest_digest"] = json!(manifest_digest);
        }
        let digest = crate::canonical::digest(&named)?;
        let short = digest
            .trim_start_matches("sha256:")
            .get(..12)
            .unwrap_or_default();
        records.push(EvidenceRecord {
            schema: EVIDENCE_SCHEMA.to_owned(),
            id: format!("{}/{}-{}-{short}", common.name, request.kind, &commit[..12]),
            kind: request.kind.clone(),
            subject: Subject {
                tree: tree.clone(),
                commit: commit.clone(),
                base_tree: base_tree.clone(),
                unknown: Map::new(),
            },
            inputs: common.inputs.clone(),
            check: CheckId::new(&check, &environment),
            policy_revision,
            manifest_digest,
            policy_provenance: None,
            producer: Producer {
                name: common.name.clone(),
                version: common.version.clone(),
                unknown: Map::new(),
            },
            verdict: common.verdict,
            at: now_rfc3339(),
            why: request.why.clone(),
            units: Vec::new(),
            selection: None,
            sandbox: None,
            seconds: None,
            signature: None,
            unknown,
        });
    }
    Ok(records)
}

/// G245: each policy that judges candidate `commit` (the one at `base`, and its own when it
/// differs) with approval check `check` (on `environment`, when named), as its revision,
/// manifest digest and the check's environment. An approval that would answer no approval
/// check never counts, so none is refused before anything is written. G292: with each, the
/// stamp of what the check covers in the candidate's `tree` and its `base_tree`
/// (`crate::throughput::stamp`), when the check carries. N-9: for a `waiver` (`kind`), each such
/// policy whose `check` is a run check marked `waivable = true`, with no stamp: a waiver names
/// one candidate and never carries.
#[allow(clippy::type_complexity)]
fn approval_policies(
    git: &Git,
    (base, commit): (&str, &str),
    (tree, base_tree): (&str, &str),
    (kind, check): (&str, &str),
    environment: Option<&str>,
) -> Result<Vec<(u64, String, String, Option<Value>)>, String> {
    let waiver = kind == evidence::KIND_WAIVER;
    let mut judged: Vec<(u64, String, String, Option<Value>)> = Vec::new();
    for at in [base, commit] {
        let loaded = crate::policy::load_at(git, at);
        // A candidate's own invalid policy is its land's refusal; the base still judges.
        if at == commit && matches!(loaded, Err(crate::policy::LoadError::Invalid(_))) {
            continue;
        }
        let Some(policy) = loaded.map_err(String::from)? else {
            continue;
        };
        if judged
            .iter()
            .any(|(_, digest, _, _)| *digest == policy.manifest_digest)
        {
            continue;
        }
        let found: Vec<&crate::policy::CheckEntry> = policy
            .checks
            .iter()
            .filter(|entry| {
                let answers = if waiver {
                    entry.waivable && !entry.is_approval()
                } else {
                    entry.is_approval()
                };
                answers
                    && entry.id == check
                    && environment.is_none_or(|named| named == entry.environment)
            })
            .collect();
        match found.as_slice() {
            [entry] => judged.push((
                policy.revision,
                policy.manifest_digest.clone(),
                entry.environment.clone(),
                if waiver {
                    None
                } else {
                    crate::throughput::stamp(git, entry, tree, base_tree)
                },
            )),
            [] => {}
            _ => {
                return Err(format!(
                    "{kind} check {check} has more than one environment at {at}; name one with --environment"
                ));
            }
        }
    }
    if judged.is_empty() && waiver {
        return Err(format!(
            "no policy that judges {commit} has a waivable check {check}{}; a waiver answers only a [[check]] with waivable = true",
            environment
                .map(|named| format!(" on {named}"))
                .unwrap_or_default()
        ));
    }
    if judged.is_empty() {
        return Err(format!(
            "no policy that judges {commit} has an approval check {check}{}; an approval answers a [[check]] with kind = \"approval\"",
            environment
                .map(|named| format!(" on {named}"))
                .unwrap_or_default()
        ));
    }
    Ok(judged)
}

/// Whether two records under one id say the same thing: every field but the time and the
/// signature, which differ on every run.
fn same_content(held: &EvidenceRecord, new: &EvidenceRecord) -> bool {
    let strip = |record: &EvidenceRecord| {
        let mut record = record.clone();
        record.at = String::new();
        record.signature = None;
        record
    };
    strip(held) == strip(new)
}
