//! The acceptance policy, `.bay/policy.toml` at the accepted base (`INTERFACES` § 7).
//!
//! Publication is judged by the policy at the candidate's base, never the candidate's own.
//! The frozen manifest is the list of (`id`, `environment`, `definition_digest`); the
//! definition digest covers the canonical `[[check]]` entry and the Git object id, at the
//! base, of every path in its `adapter` list.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::canonical::digest;
use crate::git::{Git, GitError};

pub const POLICY_PATH: &str = ".bay/policy.toml";
const DEFAULT_TIMEOUT_S: u64 = 1800;

#[derive(Debug, Deserialize)]
struct PolicyFile {
    revision: u64,
    #[serde(default)]
    check: Vec<CheckEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CheckEntry {
    pub id: String,
    pub environment: String,
    pub command: String,
    pub adapter: Vec<String>,
    /// Required, no default: the minimum number of tests the check must collect.
    pub expected_minimum: u64,
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
    /// Declared inputs beyond the tree. `commit` keys the check's evidence by subject commit
    /// (§ 7): it reads commit metadata, so a result for one commit never stands for another
    /// with the same tree. Omitted from the definition digest when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
}

impl Policy {
    /// Whether check `id` is keyed by subject commit.
    #[must_use]
    pub fn commit_keyed(&self, id: &crate::evidence::CheckId) -> bool {
        self.checks.iter().any(|check| {
            check.id == id.id
                && check.environment == id.environment
                && check.inputs.iter().any(|input| input == "commit")
        })
    }
}

const fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_S
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ManifestEntry {
    pub id: String,
    pub environment: String,
    pub definition_digest: String,
}

#[derive(Clone, Debug)]
pub struct Policy {
    pub revision: u64,
    pub checks: Vec<CheckEntry>,
    pub manifest: Vec<ManifestEntry>,
    pub manifest_digest: String,
}

/// The policy at `commit`, or `None` when that tree has no policy file (no required checks).
///
/// # Errors
///
/// The policy file is invalid: it does not parse, lacks `expected_minimum`, repeats an
/// (`id`, `environment`), names an executable outside its `adapter` list, or names an
/// adapter path absent at `commit`.
pub fn load_at(git: &Git, commit: &str) -> Result<Option<Policy>, String> {
    let text = match git.run(&["cat-file", "blob", &format!("{commit}:{POLICY_PATH}")]) {
        Ok(text) => text,
        Err(GitError::Failed { .. }) => return Ok(None),
        Err(error) => return Err(error.message()),
    };
    let file: PolicyFile =
        toml::from_str(&text).map_err(|error| format!("{POLICY_PATH} is invalid: {error}"))?;

    let mut seen = std::collections::HashSet::new();
    let mut manifest = Vec::new();
    for entry in &file.check {
        if !seen.insert((entry.id.clone(), entry.environment.clone())) {
            return Err(format!(
                "{POLICY_PATH} is invalid: two [[check]] entries for ({}, {})",
                entry.id, entry.environment
            ));
        }
        let executable = entry.command.split_whitespace().next().unwrap_or_default();
        if !entry.adapter.iter().any(|path| covers(path, executable)) {
            return Err(format!(
                "{POLICY_PATH} is invalid: check {} runs {executable}, which is not in its adapter list",
                entry.id
            ));
        }
        let mut adapter_ids = BTreeMap::new();
        for path in &entry.adapter {
            let spec = format!("{commit}:{}", path.trim_end_matches('/'));
            let oid = git
                .run(&["rev-parse", "--verify", "-q", &spec])
                .map_err(|_| format!("{POLICY_PATH} is invalid: adapter path {path} is absent"))?;
            adapter_ids.insert(path.clone(), Value::String(oid));
        }
        manifest.push(manifest_entry(entry, &adapter_ids)?);
    }
    let manifest_digest = manifest_digest(&mut manifest)?;
    Ok(Some(Policy {
        revision: file.revision,
        checks: file.check,
        manifest,
        manifest_digest,
    }))
}

/// One manifest entry: the definition digest is the sha256 of the RFC 8785 canonical JSON of
/// `{"check": <the [[check]] entry, every field present>, "adapter_ids": {<path>: <object id at
/// the base>}}`.
///
/// # Errors
///
/// The entry does not serialize.
pub fn manifest_entry(
    entry: &CheckEntry,
    adapter_ids: &BTreeMap<String, Value>,
) -> Result<ManifestEntry, String> {
    let entry_json = serde_json::to_value(entry).map_err(|error| error.to_string())?;
    Ok(ManifestEntry {
        id: entry.id.clone(),
        environment: entry.environment.clone(),
        definition_digest: digest(&json!({"check": entry_json, "adapter_ids": adapter_ids}))?,
    })
}

/// Sorts the manifest by (`id`, `environment`) and returns its digest: the sha256 of the
/// canonical JSON of the sorted list.
///
/// # Errors
///
/// The manifest does not serialize.
pub fn manifest_digest(manifest: &mut [ManifestEntry]) -> Result<String, String> {
    manifest.sort_by(|a, b| (&a.id, &a.environment).cmp(&(&b.id, &b.environment)));
    digest(&serde_json::to_value(&*manifest).map_err(|error| error.to_string())?)
}

/// Whether adapter entry `path` (a file, or a directory ending in `/`) covers `file`.
fn covers(path: &str, file: &str) -> bool {
    if path.ends_with('/') {
        file.starts_with(path)
    } else {
        path == file
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::Value;

    use super::{CheckEntry, manifest_digest, manifest_entry};

    /// The fixed test vector of G26: this policy and these adapter object ids give this
    /// manifest digest in every implementation.
    #[test]
    fn the_manifest_digest_test_vector() {
        let entry: CheckEntry = toml::from_str(
            "id = \"tests\"\nenvironment = \"linux-x86_64\"\ncommand = \"scripts/bay-check tests\"\nadapter = [\"scripts/bay-check\", \"pytest.ini\"]\nexpected_minimum = 400\n",
        )
        .unwrap();
        assert_eq!(
            entry.timeout_s, 1800,
            "an absent optional field takes its default before hashing"
        );
        let mut ids = BTreeMap::new();
        ids.insert(
            "pytest.ini".to_owned(),
            Value::String("5716ca5987cbf97d6bb54920bea6adde242d87e6".to_owned()),
        );
        ids.insert(
            "scripts/bay-check".to_owned(),
            Value::String("257cc5642cb1a054f08cc83f2d943e56fd3ebe99".to_owned()),
        );
        let mut manifest = vec![manifest_entry(&entry, &ids).unwrap()];
        let digest = manifest_digest(&mut manifest).unwrap();
        assert_eq!(
            manifest[0].definition_digest,
            "sha256:b327c311ab2e8cf882386720df7251f4775ed08564fa92bfb907e4f1574fd60e"
        );
        assert_eq!(
            digest,
            "sha256:6a071d20cb18584a5229a9fdc75c6b1d8b69ee229c25689a26251091d61dbd4b"
        );
    }
}
