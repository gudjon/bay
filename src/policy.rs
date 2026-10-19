//! The acceptance policy, `.bay/policy.toml` at the accepted base (`INTERFACES` § 7).
//!
//! Publication is judged by the policy at the candidate's base, never the candidate's own.
//! The frozen manifest is the list of (`id`, `environment`, `definition_digest`); the
//! definition digest covers the canonical `[[check]]` entry and the Git object id, at the
//! base, of every path in its `adapter` list.

use std::collections::BTreeMap;
use std::path::Path;

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
    #[serde(default)]
    quarantine: Vec<Quarantine>,
}

/// One check set aside for a stated time (`DESIGN` § 7, ruling 8c): while the entry is in the
/// policy at the base, the check is not in the frozen manifest and is reported as a coverage
/// gap. An end never changes a judgement by itself: the fixing change removes the entry.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Quarantine {
    pub id: String,
    pub environment: String,
    pub reason: String,
    /// A date (`YYYY-MM-DD`), or the `Change-Id` whose landing ends it.
    pub end: String,
}

impl Quarantine {
    /// Whether the quarantine is past its end date on `today` (`YYYY-MM-DD`): it stays in
    /// force and is shown as overdue. An end that names a change is never overdue by date.
    #[must_use]
    pub fn overdue(&self, today: &str) -> bool {
        is_date(&self.end) && self.end.as_str() < today
    }

    fn sets_aside(&self, entry: &CheckEntry) -> bool {
        self.id == entry.id && self.environment == entry.environment
    }
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
    /// Absolute directories: the check's `PATH` (ruling 8b). Absent, it takes its stated
    /// default, [`DEFAULT_PATH`], so the search path is always one the policy names. Always
    /// present in the definition digest.
    #[serde(default = "default_path")]
    pub path: Vec<String>,
    /// The only variables passed from the executor's environment, by name. The names enter
    /// the definition digest; their values never do, so the digest never pins a secret.
    #[serde(default)]
    pub env: Vec<String>,
}

impl Policy {
    /// Whether check `entry` is set aside by a `[[quarantine]]` entry (ruling 8c).
    #[must_use]
    pub fn is_quarantined(&self, entry: &CheckEntry) -> bool {
        self.quarantined.iter().any(|q| q.sets_aside(entry))
    }

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

impl Policy {
    /// Every path this policy is made of: the policy file and each adapter path, sorted.
    #[must_use]
    pub fn paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = std::iter::once(POLICY_PATH.to_owned())
            .chain(
                self.checks
                    .iter()
                    .flat_map(|check| check.adapter.iter().cloned()),
            )
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }
}

/// How the policy at a base reached trunk (`INTERFACES` § 6): `bay` when through a bay
/// publication, `outside_bay` any other way. A `pass` under an `outside_bay` policy is never
/// read as a pass under a policy bay checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provenance {
    Bay,
    OutsideBay,
}

impl Provenance {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bay => "bay",
            Self::OutsideBay => "outside_bay",
        }
    }
}

/// A trunk commit that is no bay publication and changed policy or adapter paths
/// (`DESIGN` § 10a): the edit a `policy_changed_outside_bay` event records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutsideEdit {
    pub commit: String,
    /// The policy and adapter paths it changed, sorted.
    pub paths: Vec<String>,
    /// Whether the same commit also changed paths that are neither the policy nor an
    /// adapter (`DESIGN` § 10a): a forge landing that mixes the two is named as such.
    pub other_paths: bool,
}

/// The edits outside bay that the policy at `base` still stands on: for each of its paths,
/// the last commit on trunk's first-parent line that changed it, when `is_publication` says
/// that commit is no bay publication. Empty means the policy reached trunk through bay.
///
/// # Errors
///
/// A git fault, or `is_publication` fails.
pub fn outside_edits(
    git: &Git,
    base: &str,
    policy: &Policy,
    is_publication: &dyn Fn(&str) -> Result<bool, String>,
) -> Result<Vec<OutsideEdit>, String> {
    let paths = policy.paths();
    // The bootstrap: the first commit on trunk's first-parent line that added the policy
    // file. Before it there was no policy for an edit to weaken and no bay judgement to go
    // around, so it and everything it stands on reached trunk the only way a first policy can,
    // on a forge or a bay-hosted trunk alike. A later re-add, after a removal, is no bootstrap.
    let adds = git
        .run(&[
            "log",
            "--first-parent",
            "--diff-filter=A",
            "--format=%H",
            base,
            "--",
            POLICY_PATH,
        ])
        .map_err(|error| error.message())?;
    let bootstrap = adds.lines().last().map(str::to_owned);
    let mut edits: Vec<OutsideEdit> = Vec::new();
    for path in &paths {
        let last = git
            .run(&[
                "log",
                "-1",
                "--first-parent",
                "--format=%H",
                base,
                "--",
                path,
            ])
            .map_err(|error| error.message())?;
        let last = last.trim();
        if last.is_empty() || edits.iter().any(|edit| edit.commit == last) {
            continue;
        }
        if let Some(bootstrap) = &bootstrap
            && is_ancestor(git, last, bootstrap)?
        {
            continue;
        }
        if is_publication(last)? {
            continue;
        }
        let changed = changed_paths(git, last)?;
        let mut touched: Vec<String> = paths
            .iter()
            .filter(|path| changed.iter().any(|file| covers(path, file)))
            .cloned()
            .collect();
        touched.sort();
        let other_paths = changed
            .iter()
            .any(|file| !paths.iter().any(|path| covers(path, file)));
        edits.push(OutsideEdit {
            commit: last.to_owned(),
            paths: touched,
            other_paths,
        });
    }
    Ok(edits)
}

/// Whether `ancestor` is an ancestor of (or equal to) `descendant`.
fn is_ancestor(git: &Git, ancestor: &str, descendant: &str) -> Result<bool, String> {
    match git.run(&["merge-base", "--is-ancestor", ancestor, descendant]) {
        Ok(_) => Ok(true),
        Err(GitError::Failed { .. }) => Ok(false),
        Err(error) => Err(error.message()),
    }
}

/// The files `commit` changed against its first parent (every file, for a root commit).
fn changed_paths(git: &Git, commit: &str) -> Result<Vec<String>, String> {
    let has_parent = git
        .run(&["rev-parse", "--verify", "-q", &format!("{commit}^1")])
        .is_ok();
    let out = if has_parent {
        git.run(&["diff", "--name-only", &format!("{commit}^1"), commit])
    } else {
        git.run(&["ls-tree", "-r", "--name-only", commit])
    }
    .map_err(|error| error.message())?;
    Ok(out.lines().map(str::to_owned).collect())
}

const fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_S
}

/// The `path` of a `[[check]]` entry that states none: the POSIX shell's own fallback search
/// path, now stated, so evidence names every directory a check could run a program from.
pub const DEFAULT_PATH: [&str; 2] = ["/usr/bin", "/bin"];

fn default_path() -> Vec<String> {
    DEFAULT_PATH.iter().map(|dir| (*dir).to_owned()).collect()
}

/// The variables of the fixed baseline (`INTERFACES` § 7). An `env` entry that names one
/// would override the baseline from the executor's environment, so it makes the policy
/// invalid.
pub const BASELINE_VARIABLES: [&str; 5] = ["PATH", "HOME", "TMPDIR", "LANG", "BAY_ADAPTER_DIR"];

/// Whether `end` is a quarantine end: a date (`YYYY-MM-DD`) or a `Change-Id`.
fn is_quarantine_end(end: &str) -> bool {
    is_date(end) || crate::ids::is_change_id(end)
}

fn is_date(text: &str) -> bool {
    text.len() == 10
        && text.char_indices().all(|(at, c)| {
            if at == 4 || at == 7 {
                c == '-'
            } else {
                c.is_ascii_digit()
            }
        })
}

/// The `revision` of the policy file at `commit`, read even when the policy is invalid, so a
/// repair of an invalid policy still raises it (`INTERFACES` § 7). `None` when there is no
/// policy file or no integer `revision` in it.
#[must_use]
pub fn revision_at(git: &Git, commit: &str) -> Option<u64> {
    let text = git
        .run(&["cat-file", "blob", &format!("{commit}:{POLICY_PATH}")])
        .ok()?;
    let table: toml::Table = toml::from_str(&text).ok()?;
    u64::try_from(table.get("revision")?.as_integer()?).ok()
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
    /// Every `[[check]]` entry, quarantined or not: each one's adapter paths stay policy paths.
    pub checks: Vec<CheckEntry>,
    /// The `[[quarantine]]` entries: their checks are not required.
    pub quarantined: Vec<Quarantine>,
    /// The required checks only.
    pub manifest: Vec<ManifestEntry>,
    pub manifest_digest: String,
}

/// Why a policy could not be loaded.
#[derive(Debug)]
pub enum LoadError {
    /// The policy breaks a validity rule (§ 7): at the accepted base, `POLICY_INVALID`.
    Invalid(String),
    /// A git fault, not a property of the policy.
    Git(String),
}

impl From<LoadError> for String {
    fn from(error: LoadError) -> Self {
        match error {
            LoadError::Invalid(why) | LoadError::Git(why) => why,
        }
    }
}

/// The policy at `commit`, or `None` when that tree has no policy file (no required checks).
///
/// # Errors
///
/// [`LoadError::Invalid`] when the policy file is invalid: it does not parse, lacks
/// `expected_minimum`, repeats an (`id`, `environment`), names an executable outside its
/// `adapter` list, names an adapter path absent at `commit`, has a relative `path` entry,
/// names a baseline variable in `env`, has a `[[quarantine]]` that names no check or whose
/// `end` is neither a date nor a `Change-Id`, or has no required check whose
/// `expected_minimum` is above zero. [`LoadError::Git`] on a git fault.
pub fn load_at(git: &Git, commit: &str) -> Result<Option<Policy>, LoadError> {
    let text = match git.run(&["cat-file", "blob", &format!("{commit}:{POLICY_PATH}")]) {
        Ok(text) => text,
        Err(GitError::Failed { .. }) => return Ok(None),
        Err(error) => return Err(LoadError::Git(error.message())),
    };
    let invalid = |why: String| LoadError::Invalid(format!("{POLICY_PATH} is invalid: {why}"));
    let file: PolicyFile = toml::from_str(&text).map_err(|error| invalid(error.to_string()))?;
    // Ruling 8c: a quarantine that names no check, or whose end is neither a date nor a
    // Change-Id, would set nothing aside or never show as overdue: a typo is never silent.
    for quarantine in &file.quarantine {
        if !file.check.iter().any(|entry| quarantine.sets_aside(entry)) {
            return Err(invalid(format!(
                "[[quarantine]] ({}, {}) names no [[check]] entry",
                quarantine.id, quarantine.environment
            )));
        }
        if !is_quarantine_end(&quarantine.end) {
            return Err(invalid(format!(
                "[[quarantine]] ({}, {}) ends at {:?}, which is neither a date (YYYY-MM-DD) nor a Change-Id",
                quarantine.id, quarantine.environment, quarantine.end
            )));
        }
    }
    let quarantined = |entry: &CheckEntry| file.quarantine.iter().any(|q| q.sets_aside(entry));
    // § 7: a policy whose every required check may collect nothing could pass any candidate
    // untested.
    if !file
        .check
        .iter()
        .any(|entry| entry.expected_minimum > 0 && !quarantined(entry))
    {
        return Err(invalid(
            "no required check has an expected_minimum above zero".to_owned(),
        ));
    }

    let mut seen = std::collections::HashSet::new();
    let mut manifest = Vec::new();
    for entry in &file.check {
        if !seen.insert((entry.id.clone(), entry.environment.clone())) {
            return Err(invalid(format!(
                "two [[check]] entries for ({}, {})",
                entry.id, entry.environment
            )));
        }
        let executable = entry.command.split_whitespace().next().unwrap_or_default();
        // An empty `PATH`, or an empty element, means the current directory to a POSIX
        // shell: the candidate's tree. A `:` would split one entry into two.
        if entry.path.is_empty() {
            return Err(invalid(format!(
                "check {} has an empty path; leave path out for its default ({})",
                entry.id,
                DEFAULT_PATH.join(", ")
            )));
        }
        if let Some(relative) = entry
            .path
            .iter()
            .find(|dir| !Path::new(dir.as_str()).is_absolute() || dir.contains(':'))
        {
            return Err(invalid(format!(
                "check {} has the path entry {relative}; path takes absolute directories, each without ':'",
                entry.id
            )));
        }
        if let Some(name) = entry.env.iter().find(|name| {
            name.is_empty()
                || name.contains(['=', '\0'])
                || BASELINE_VARIABLES.contains(&name.as_str())
        }) {
            return Err(invalid(format!(
                "check {} names {name:?} in env; env names other variables than the fixed baseline ({})",
                entry.id,
                BASELINE_VARIABLES.join(", ")
            )));
        }
        if !entry.adapter.iter().any(|path| covers(path, executable)) {
            return Err(invalid(format!(
                "check {} runs {executable}, which is not in its adapter list",
                entry.id
            )));
        }
        let mut adapter_ids = BTreeMap::new();
        for path in &entry.adapter {
            let spec = format!("{commit}:{}", path.trim_end_matches('/'));
            let oid = git
                .run(&["rev-parse", "--verify", "-q", &spec])
                .map_err(|_| invalid(format!("adapter path {path} is absent")))?;
            adapter_ids.insert(path.clone(), Value::String(oid));
        }
        // Ruling 8c: an open quarantine takes its check out of the frozen manifest.
        if !quarantined(entry) {
            manifest.push(manifest_entry(entry, &adapter_ids).map_err(LoadError::Git)?);
        }
    }
    let manifest_digest = manifest_digest(&mut manifest).map_err(LoadError::Git)?;
    Ok(Some(Policy {
        revision: file.revision,
        checks: file.check,
        quarantined: file.quarantine,
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

    /// Ruling 8b: the check's `path` and the names in its `env` are fields of the entry, so
    /// changing either changes its definition digest (`INTERFACES` § 7).
    #[test]
    fn path_and_env_enter_the_definition_digest() {
        let text = "id = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n";
        let ids = BTreeMap::new();
        let digest_of = |extra: &str| {
            let entry: CheckEntry = toml::from_str(&format!("{text}{extra}")).unwrap();
            manifest_entry(&entry, &ids).unwrap().definition_digest
        };
        let plain = digest_of("");
        let with_path = digest_of("path = [\"/usr/bin\"]\n");
        let with_env = digest_of("env = [\"DATABASE_URL\"]\n");
        assert_ne!(plain, with_path);
        assert_ne!(plain, with_env);
        assert_ne!(with_path, digest_of("path = [\"/bin\"]\n"));
        assert_ne!(with_env, digest_of("env = [\"OTHER\"]\n"));
    }

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
        // Ruling 8b: an absent `path` is hashed as its stated default, an absent `env` as an
        // empty list.
        assert_eq!(entry.path, super::DEFAULT_PATH);
        assert_eq!(entry.env, Vec::<String>::new());
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
            "sha256:70842f39709ecf6dfb51ab97c81cc020ca764df865f221a57f744a5a507b5e83"
        );
        assert_eq!(
            digest,
            "sha256:648dc0a34161fcd3d98d99ba9a6344ff8bb74db55fed4177ccd0debf0eb2387e"
        );
    }
}
