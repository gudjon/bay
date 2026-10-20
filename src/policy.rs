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
use crate::inputs::InputEntry;

pub const POLICY_PATH: &str = ".bay/policy.toml";
const DEFAULT_TIMEOUT_S: u64 = 1800;

/// Every table of the policy refuses a key it does not know (G167): a typo, or a key a newer bay
/// reads, would otherwise be a criterion nobody applies. `[[input]]` and `[profile.<name>]`
/// refuse theirs in their own types.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    revision: u64,
    /// The repository declares that a change lands with no check (the missing-policy ruling):
    /// valid only with no `[[check]]`, and judged at the base like every other field.
    #[serde(default)]
    unchecked: bool,
    #[serde(default)]
    check: Vec<CheckEntry>,
    #[serde(default)]
    quarantine: Vec<Quarantine>,
    /// The named inputs' providers (`DESIGN` § 8, ruling R3).
    #[serde(default)]
    input: Vec<InputEntry>,
    /// The completion profiles (`DESIGN` § 7, ruling R12): `[profile.<name>] require = [...]`.
    #[serde(default)]
    profile: BTreeMap<String, crate::completion::ProfileEntry>,
    /// What the policy asks of the acting agent (G152).
    #[serde(default)]
    agent: AgentPolicy,
}

/// The `[agent]` table: `require_actor = true` makes `bay start` refuse `ACTOR_UNNAMED` while
/// the actor's name comes from a default (G152).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPolicy {
    #[serde(default)]
    pub require_actor: bool,
}

/// One check set aside for a stated time (`DESIGN` § 7, ruling 8c): while the entry is in the
/// policy at the base, the check is not in the frozen manifest and is reported as a coverage
/// gap. An end never changes a judgement by itself: the fixing change removes the entry.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
    /// The content-addressed fixtures the check reads, each `<name>@sha256:<64 hex>`
    /// (ruling R18): their own key, so a changed fixture digest is a changed definition and
    /// evidence made with the old one is never reused (G7). Omitted from the definition digest
    /// when empty, like `inputs`, so a policy without it keeps its digest (the G26 vector).
    /// The digest is the policy's claim: bay passes it to the adapter as `requires.fixtures`
    /// and copies it into the record's `inputs`, but never reads the fixture, so the adapter
    /// must read only that content (`INTERFACES` § 3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixtures: Vec<String>,
    /// Absolute directories: the check's `PATH` (ruling 8b). Absent, it takes its stated
    /// default, [`DEFAULT_PATH`], so the search path is always one the policy names. Always
    /// present in the definition digest.
    #[serde(default = "default_path")]
    pub path: Vec<String>,
    /// The only variables passed from the executor's environment, by name. The names enter
    /// the definition digest; their values never do, so the digest never pins a secret.
    #[serde(default)]
    pub env: Vec<String>,
    /// The only paths of the candidate's tree the check sees, each a file or a directory
    /// ending in `/`, matched as an `adapter` entry is: the rest of the tree is absent from
    /// its materialization. Absent, the check gets the whole tree. A field of the entry, so it
    /// is judged at the base and enters the definition digest; left out of the canonical entry
    /// when absent, so a policy without it keeps its digest (the G26 vector). An empty list
    /// makes the policy invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    /// The bound on materializing the candidate and the adapter for this check, in seconds,
    /// apart from the bound on one Git call: exceeding it is `infra_failure` that names it,
    /// never a pass. Left out of the canonical entry at its default, so a policy without it
    /// keeps its digest; any other value enters the definition digest.
    #[serde(default, skip_serializing_if = "MaterializeTimeout::is_default")]
    pub materialize_timeout_s: MaterializeTimeout,
    /// Whether the check may use the network (G62). Denied by default: the check runs in a
    /// network namespace of its own. A field of the entry, so it is judged at the base and a
    /// candidate cannot grant itself the network; `true` enters the definition digest, and the
    /// default is left out of the canonical entry, so a policy without it keeps its digest.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub network: bool,
    /// When the check runs (G91): `pre`, the default, before publication and required for it;
    /// `post`, after publication, never blocking it and shown pending for the landed commit
    /// until a record for its tree arrives. Judged at the base; `post` enters the definition
    /// digest, and the default is left out of the canonical entry (the G26 vector).
    #[serde(default, skip_serializing_if = "Stage::is_pre")]
    pub stage: Stage,
    /// Globs over the candidate's changed paths (G146): the check is required only when the
    /// candidate's own diff against its base changes a path one of them matches, never by the
    /// paths the change declared. Judged at the base and in the definition digest, so a
    /// candidate cannot narrow it for itself; left out of the canonical entry when absent. An
    /// empty list makes the policy invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_paths: Option<Vec<String>>,
    /// The producers whose records answer this check (G163, owner ruling Q3): a record from any
    /// other producer is reported `unlisted_producer` and does not count for it. Absent, any
    /// producer answers. Judged at the base and in the definition digest, so a candidate that
    /// edits the list is a policy change checked twice and never widens it for itself (G26);
    /// left out of the canonical entry when absent. An empty list makes the policy invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producers: Option<Vec<String>>,
}

/// A check's `stage` (G91).
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    #[default]
    Pre,
    Post,
}

impl Stage {
    #[allow(clippy::trivially_copy_pass_by_ref)]
    fn is_pre(&self) -> bool {
        *self == Self::Pre
    }
}

impl CheckEntry {
    /// Whether `changed`, the paths the candidate's diff against its base changes, wakes this
    /// check (G146): always without `when_paths`, else when a glob matches one of them.
    #[must_use]
    pub fn wakes(&self, changed: &[String]) -> bool {
        self.when_paths.as_ref().is_none_or(|globs| {
            changed
                .iter()
                .any(|path| globs.iter().any(|glob| glob_matches(glob, path)))
        })
    }
}

/// A required check that a candidate's diff did not wake (G146): recorded, never silently
/// skipped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NotRequired {
    pub check: String,
    pub environment: String,
    pub reason: String,
}

/// The `materialize_timeout_s` of a `[[check]]` entry that states none.
pub const DEFAULT_MATERIALIZE_TIMEOUT_S: u64 = 600;

/// A check's `materialize_timeout_s`: whole seconds, [`DEFAULT_MATERIALIZE_TIMEOUT_S`] when the
/// entry states none.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct MaterializeTimeout(pub u64);

impl Default for MaterializeTimeout {
    fn default() -> Self {
        Self(DEFAULT_MATERIALIZE_TIMEOUT_S)
    }
}

impl MaterializeTimeout {
    fn is_default(&self) -> bool {
        self.0 == DEFAULT_MATERIALIZE_TIMEOUT_S
    }
}

impl Policy {
    /// The adapter paths of every `[[check]]` and `[[input]]` entry: a provider is policy, so a
    /// change to it is a policy change (ruling R3).
    pub fn adapter_paths(&self) -> impl Iterator<Item = String> + '_ {
        self.checks
            .iter()
            .flat_map(|check| check.adapter.iter())
            .chain(self.inputs.iter().flat_map(|input| input.adapter.iter()))
            .cloned()
    }

    /// Whether check `entry` is set aside by a `[[quarantine]]` entry (ruling 8c).
    #[must_use]
    pub fn is_quarantined(&self, entry: &CheckEntry) -> bool {
        self.quarantined.iter().any(|q| q.sets_aside(entry))
    }

    /// Whether check `id` is keyed by subject commit.
    #[must_use]
    pub fn commit_keyed(&self, id: &crate::evidence::CheckId) -> bool {
        self.check(id)
            .is_some_and(|check| check.inputs.iter().any(|input| input == "commit"))
    }

    /// Whether check `entry` is required of a candidate whose diff against its base changes
    /// `changed`: it is in the manifest (pre-publication, not quarantined) and the diff wakes
    /// it ([`Policy::unwoken`]).
    #[must_use]
    pub fn requires(&self, entry: &CheckEntry, changed: &[String]) -> bool {
        self.manifest
            .iter()
            .any(|m| m.id == entry.id && m.environment == entry.environment)
            && self.unwoken(entry, changed).is_none()
    }

    /// The manifest entries required of a candidate whose diff against its base changes
    /// `changed` (G146), and the ones its diff did not wake, each with the reason.
    #[must_use]
    pub fn required(&self, changed: &[String]) -> (Vec<ManifestEntry>, Vec<NotRequired>) {
        let mut required = Vec::new();
        let mut not_required = Vec::new();
        for entry in &self.manifest {
            let id = crate::evidence::CheckId::new(&entry.id, &entry.environment);
            match self
                .check(&id)
                .and_then(|check| self.unwoken(check, changed))
            {
                Some(skipped) => not_required.push(skipped),
                None => required.push(entry.clone()),
            }
        }
        (required, not_required)
    }

    /// Why check `entry` is not required of a candidate whose diff against its base changes
    /// `changed` (G146), or `None` when the diff wakes it: it has `when_paths` and no changed
    /// path matches one. A change to the policy file or an adapter path wakes every check: a
    /// policy change is checked twice, and a check that never ran would check nothing.
    #[must_use]
    pub fn unwoken(&self, entry: &CheckEntry, changed: &[String]) -> Option<NotRequired> {
        let paths = self.paths();
        let policy_change = changed
            .iter()
            .any(|file| paths.iter().any(|path| covers(path, file)));
        (!policy_change && !entry.wakes(changed)).then(|| NotRequired {
            check: entry.id.clone(),
            environment: entry.environment.clone(),
            reason: format!(
                "no path the candidate changes against its base matches when_paths {}",
                serde_json::to_string(entry.when_paths.as_deref().unwrap_or_default())
                    .unwrap_or_default()
            ),
        })
    }

    /// The `[[check]]` entry for `id`.
    #[must_use]
    pub fn check(&self, id: &crate::evidence::CheckId) -> Option<&CheckEntry> {
        self.checks
            .iter()
            .find(|check| check.id == id.id && check.environment == id.environment)
    }

    /// Whether `record`'s producer may answer the check it names (G163): the check has no
    /// `producers` list, or the list names it. Applied to a check's records before they
    /// combine, under each policy that judges, so the base's list binds a candidate.
    #[must_use]
    pub fn listed(&self, record: &crate::evidence::EvidenceRecord) -> bool {
        self.check(&record.check)
            .and_then(|check| check.producers.as_ref())
            .is_none_or(|names| names.contains(&record.producer.name))
    }

    /// `records` without those whose producer the check's `producers` list leaves out.
    #[must_use]
    pub fn answering(
        &self,
        records: Vec<crate::evidence::EvidenceRecord>,
    ) -> Vec<crate::evidence::EvidenceRecord> {
        records
            .into_iter()
            .filter(|record| self.listed(record))
            .collect()
    }
}

impl Policy {
    /// Every path this policy is made of: the policy file and each adapter path of its checks
    /// and its input providers, sorted.
    #[must_use]
    pub fn paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = std::iter::once(POLICY_PATH.to_owned())
            .chain(self.adapter_paths())
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
    /// `unchecked = true`: the policy declares that a change lands with no check. It has no
    /// `[[check]]` and an empty manifest, and its landing says "unchecked by policy".
    pub unchecked: bool,
    /// Every `[[check]]` entry, quarantined or not: each one's adapter paths stay policy paths.
    pub checks: Vec<CheckEntry>,
    /// The `[[quarantine]]` entries: their checks are not required.
    pub quarantined: Vec<Quarantine>,
    /// The `[[input]]` entries: each named input's provider.
    pub inputs: Vec<InputEntry>,
    /// The required checks only: pre-publication and not quarantined.
    pub manifest: Vec<ManifestEntry>,
    /// The post-publication checks that are not quarantined (G91): never required for a
    /// landing, shown pending for the landed commit until a record for its tree arrives.
    pub post: Vec<ManifestEntry>,
    /// The `[profile.<name>]` tables: the completion dimensions each profile requires, and
    /// whether it counts only signed evidence.
    pub profiles: BTreeMap<String, crate::completion::Profile>,
    /// The digest of the sorted manifest (`INTERFACES` § 7), its post-publication entries with
    /// it: a record of a post check is judged under the same digest. A profile is not in it: a profile
    /// says what a change needs, not what a check computes, so editing one leaves every check's
    /// evidence current. It is a policy change all the same (`revision`, ruling R12).
    pub manifest_digest: String,
    /// The `[agent]` table (G152).
    pub agent: AgentPolicy,
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

/// The policy at `commit`, or `None` when that tree has no policy file. No policy is unknown
/// criteria, never "no required checks": land refuses it with `POLICY_MISSING`. A repository
/// that wants no checks says `unchecked = true`.
///
/// # Errors
///
/// [`LoadError::Invalid`] when the policy file is invalid: it does not parse, lacks
/// `expected_minimum`, repeats an (`id`, `environment`) or an `[[input]]` name, names an
/// executable outside its `adapter` list, names an adapter path absent at `commit`, has a
/// relative `path` entry, names a baseline variable in `env`, declares an input other than
/// `commit` or a fixture that is not `<name>@sha256:<hex>`, has a `[[quarantine]]` that names
/// no check or whose `end` is neither a date nor a `Change-Id`, has a `[profile.<name>]` that
/// requires nothing, an unknown dimension or one twice, declares `unchecked = true` beside a
/// `[[check]]`, puts one check id in both stages, has a bad `when_paths` or `producers` list,
/// or, not unchecked, has no required pre-publication check whose `expected_minimum` is above
/// zero.
/// [`LoadError::Git`] on a git fault.
pub fn load_at(git: &Git, commit: &str) -> Result<Option<Policy>, LoadError> {
    let text = match git.run(&["cat-file", "blob", &format!("{commit}:{POLICY_PATH}")]) {
        Ok(text) => text,
        Err(GitError::Failed { .. }) => return Ok(None),
        Err(error) => return Err(LoadError::Git(error.message())),
    };
    let invalid = |why: String| LoadError::Invalid(format!("{POLICY_PATH} is invalid: {why}"));
    let file: PolicyFile =
        toml::from_str(&text).map_err(|error| invalid(parse_error(&text, &error)))?;
    validate_shape(&file).map_err(invalid)?;
    let quarantined = |entry: &CheckEntry| file.quarantine.iter().any(|q| q.sets_aside(entry));

    let mut seen = std::collections::HashSet::new();
    let mut manifest = Vec::new();
    let mut post = Vec::new();
    for entry in &file.check {
        if !seen.insert((entry.id.clone(), entry.environment.clone())) {
            return Err(invalid(format!(
                "two [[check]] entries for ({}, {})",
                entry.id, entry.environment
            )));
        }
        let executable = entry.command.split_whitespace().next().unwrap_or_default();
        validate_environment(entry)
            .and_then(|()| validate_declared_inputs(entry))
            .and_then(|()| validate_materialization(entry))
            .and_then(|()| validate_when_paths(entry))
            .and_then(|()| validate_producers(entry))
            .map_err(invalid)?;
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
        // Ruling 8c: an open quarantine takes its check out of the frozen manifest. G91: a
        // post-publication check is never in it, so it never blocks a landing.
        if !quarantined(entry) {
            let made = manifest_entry(entry, &adapter_ids).map_err(LoadError::Git)?;
            if entry.stage == Stage::Pre {
                manifest.push(made);
            } else {
                post.push(made);
            }
        }
    }
    validate_inputs(git, commit, &file.input).map_err(invalid)?;
    let profiles = crate::completion::requirements(file.profile).map_err(invalid)?;
    let mut frozen: Vec<ManifestEntry> = manifest.iter().chain(&post).cloned().collect();
    let manifest_digest = manifest_digest(&mut frozen).map_err(LoadError::Git)?;
    sort_manifest(&mut manifest);
    sort_manifest(&mut post);
    Ok(Some(Policy {
        revision: file.revision,
        unchecked: file.unchecked,
        checks: file.check,
        quarantined: file.quarantine,
        inputs: file.input,
        manifest,
        post,
        profiles,
        manifest_digest,
        agent: file.agent,
    }))
}

/// Whether the policy at `commit` requires a named actor (`[agent] require_actor`, G152),
/// judged at the base like every other policy key. No policy requires none. An invalid policy
/// still requires one when its text says so: a typo elsewhere never lifts the requirement.
///
/// # Errors
///
/// A git fault.
pub fn requires_actor_at(git: &Git, commit: &str) -> Result<bool, String> {
    match load_at(git, commit) {
        Ok(policy) => Ok(policy.is_some_and(|policy| policy.agent.require_actor)),
        Err(LoadError::Git(why)) => Err(why),
        Err(LoadError::Invalid(_)) => Ok(git
            .run(&["cat-file", "blob", &format!("{commit}:{POLICY_PATH}")])
            .ok()
            .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
            .and_then(|table| table.get("agent")?.get("require_actor")?.as_bool())
            .unwrap_or(false)),
    }
}

/// Why the policy text does not parse. An unknown key (G167) is named with its table and line,
/// in one line: a typo, or a key a newer bay reads, is never read as nothing.
fn parse_error(text: &str, error: &toml::de::Error) -> String {
    let key = error
        .message()
        .strip_prefix("unknown field `")
        .and_then(|rest| rest.split_once('`'));
    let (Some((key, rest)), Some(span)) = (key, error.span()) else {
        return error.to_string();
    };
    let line = text.get(..span.start).unwrap_or(text).matches('\n').count();
    let lines: Vec<&str> = text.lines().collect();
    let header = |at: usize| {
        let line = lines.get(at)?.split('#').next()?.trim();
        line.starts_with('[').then(|| line.to_owned())
    };
    // On a header line the unknown key is the header's last name, in the table above it;
    // elsewhere it is in the nearest header above, or at the top level.
    let table = match header(line) {
        Some(own) => {
            let path = own.trim_matches(|c| c == '[' || c == ']').trim();
            path.rsplit_once('.').map_or_else(
                || "the top level".to_owned(),
                |(parent, _)| match parent.trim() {
                    table @ ("check" | "quarantine" | "input") => format!("[[{table}]]"),
                    table => format!("[{table}]"),
                },
            )
        }
        None => (0..line)
            .rev()
            .find_map(header)
            .unwrap_or_else(|| "the top level".to_owned()),
    };
    format!(
        "{table} has the unknown key `{key}` (line {}); this bay does not read it, so it is a typo or a key of a newer bay, and unknown is never pass; {}",
        line + 1,
        rest.trim_start_matches(", ")
    )
}

/// The rules on the file's shape: its quarantines, `unchecked` and the minimum of its checks.
fn validate_shape(file: &PolicyFile) -> Result<(), String> {
    // Ruling 8c: a quarantine that names no check, or whose end is neither a date nor a
    // Change-Id, would set nothing aside or never show as overdue: a typo is never silent.
    for quarantine in &file.quarantine {
        if !file.check.iter().any(|entry| quarantine.sets_aside(entry)) {
            return Err(format!(
                "[[quarantine]] ({}, {}) names no [[check]] entry",
                quarantine.id, quarantine.environment
            ));
        }
        if !is_quarantine_end(&quarantine.end) {
            return Err(format!(
                "[[quarantine]] ({}, {}) ends at {:?}, which is neither a date (YYYY-MM-DD) nor a Change-Id",
                quarantine.id, quarantine.environment, quarantine.end
            ));
        }
    }
    let quarantined = |entry: &CheckEntry| file.quarantine.iter().any(|q| q.sets_aside(entry));
    // The missing-policy ruling: `unchecked = true` says no check runs, so a check beside it
    // would make the policy say two things.
    if file.unchecked && !file.check.is_empty() {
        return Err(
            "unchecked = true declares that no check runs, and this policy has [[check]] entries; remove one or the other".to_owned(),
        );
    }
    // § 7: a policy whose every required check may collect nothing could pass any candidate
    // untested. An unchecked policy says so instead of passing.
    // G91: one check id runs before publication or after it, never both, so a record of it
    // never answers for the other stage.
    if let Some(entry) = file.check.iter().find(|entry| {
        file.check
            .iter()
            .any(|other| other.id == entry.id && other.stage != entry.stage)
    }) {
        return Err(format!(
            "check {} is in both stages, pre and post; give the post-publication check its own id",
            entry.id
        ));
    }
    // A post-publication check never judges a landing, so it never counts here.
    if !file.unchecked
        && !file.check.iter().any(|entry| {
            entry.expected_minimum > 0 && !quarantined(entry) && entry.stage == Stage::Pre
        })
    {
        return Err("no required check has an expected_minimum above zero".to_owned());
    }
    Ok(())
}

/// The one `inputs` value is `commit` (BACKLOG 28, `INTERFACES` § 7): any other is an input bay
/// cannot identify, and an unknown input is never a pass, so it makes the policy invalid rather
/// than keying the check's evidence by tree. Each `fixtures` entry is `<name>@sha256:<64 hex>`
/// (ruling R18), each name once: a fixture without a digest is an input bay cannot identify.
fn validate_declared_inputs(entry: &CheckEntry) -> Result<(), String> {
    if let Some(other) = entry.inputs.iter().find(|input| *input != "commit") {
        return Err(format!(
            "check {} declares the input {other:?}; the one input bay can identify is \"commit\"",
            entry.id
        ));
    }
    let mut names = std::collections::HashSet::new();
    for fixture in &entry.fixtures {
        let Some(name) = fixture_name(fixture) else {
            return Err(format!(
                "check {} names the fixture {fixture:?}; a fixture is <name>@sha256:<64 lowercase hex>",
                entry.id
            ));
        };
        if !names.insert(name) {
            return Err(format!("check {} names the fixture {name} twice", entry.id));
        }
    }
    Ok(())
}

/// The environment rules of one `[[check]]` (ruling 8b): `path` lists absolute directories,
/// and `env` names variables outside the fixed baseline.
fn validate_environment(entry: &CheckEntry) -> Result<(), String> {
    // An empty `PATH`, or an empty element, means the current directory to a POSIX
    // shell: the candidate's tree. A `:` would split one entry into two.
    if entry.path.is_empty() {
        return Err(format!(
            "check {} has an empty path; leave path out for its default ({})",
            entry.id,
            DEFAULT_PATH.join(", ")
        ));
    }
    if let Some(relative) = entry
        .path
        .iter()
        .find(|dir| !Path::new(dir.as_str()).is_absolute() || dir.contains(':'))
    {
        return Err(format!(
            "check {} has the path entry {relative}; path takes absolute directories, each without ':'",
            entry.id
        ));
    }
    if let Some(name) = entry.env.iter().find(|name| {
        name.is_empty() || name.contains(['=', '\0']) || BASELINE_VARIABLES.contains(&name.as_str())
    }) {
        return Err(format!(
            "check {} names {name:?} in env; env names other variables than the fixed baseline ({})",
            entry.id,
            BASELINE_VARIABLES.join(", ")
        ));
    }
    Ok(())
}

/// The materialization rules of one `[[check]]`: `paths`, when present, lists at least one
/// path, each a plain relative file or directory (ending in `/`) that the tree could hold, once;
/// `materialize_timeout_s` is at least one second. A check that could see nothing, or whose
/// bound is always exceeded, is a typo, never silent.
fn validate_materialization(entry: &CheckEntry) -> Result<(), String> {
    if entry.materialize_timeout_s.0 == 0 {
        return Err(format!(
            "check {} has materialize_timeout_s = 0; leave it out for its default ({DEFAULT_MATERIALIZE_TIMEOUT_S})",
            entry.id
        ));
    }
    let Some(paths) = &entry.paths else {
        return Ok(());
    };
    if paths.is_empty() {
        return Err(format!(
            "check {} has an empty paths list; leave paths out for the whole tree",
            entry.id
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for path in paths {
        let name = path.strip_suffix('/').unwrap_or(path);
        if !covers(name, name) || path.contains('\\') {
            return Err(format!(
                "check {} names the path {path:?}; paths takes relative files and directories ending in '/', without '.', '..' or empty parts",
                entry.id
            ));
        }
        if !seen.insert(path.as_str()) {
            return Err(format!("check {} names the path {path} twice", entry.id));
        }
    }
    Ok(())
}

/// The `when_paths` rules of one `[[check]]` (G146): when present, at least one glob, each a
/// relative path pattern without `.`, `..` or empty parts (a trailing `/` names a directory),
/// once. A list that could match nothing is a typo, never silent.
fn validate_when_paths(entry: &CheckEntry) -> Result<(), String> {
    let Some(globs) = &entry.when_paths else {
        return Ok(());
    };
    if globs.is_empty() {
        return Err(format!(
            "check {} has an empty when_paths list; leave when_paths out to require it for every change",
            entry.id
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for glob in globs {
        let name = glob.strip_suffix('/').unwrap_or(glob);
        if !covers(name, name) || glob.contains('\\') {
            return Err(format!(
                "check {} names the glob {glob:?} in when_paths; when_paths takes relative path globs, without '.', '..' or empty parts",
                entry.id
            ));
        }
        if !seen.insert(glob.as_str()) {
            return Err(format!(
                "check {} names {glob} twice in when_paths",
                entry.id
            ));
        }
    }
    Ok(())
}

/// The `producers` list of one `[[check]]` (G163): when present, at least one producer name,
/// each a valid operator name, once. An empty list would let no record answer the check, which
/// is a typo, never silent.
fn validate_producers(entry: &CheckEntry) -> Result<(), String> {
    let Some(producers) = &entry.producers else {
        return Ok(());
    };
    if producers.is_empty() {
        return Err(format!(
            "check {} has an empty producers list; leave producers out to let any producer answer it",
            entry.id
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for name in producers {
        crate::names::operator_name(name)
            .map_err(|why| format!("check {} names a bad producer: {why}", entry.id))?;
        if !seen.insert(name.as_str()) {
            return Err(format!(
                "check {} names producer {name} twice in producers",
                entry.id
            ));
        }
    }
    Ok(())
}

/// Whether `when_paths` glob `glob` matches changed path `path`: `*` matches within one path
/// part, `?` one character other than `/`, a part `**` any number of parts, and a glob ending
/// in `/` every path under that directory.
#[must_use]
pub fn glob_matches(glob: &str, path: &str) -> bool {
    let glob: Vec<&str> = match glob.strip_suffix('/') {
        // A directory holds at least one part below it: `studio/` never matches a file `studio`.
        Some(dir) => dir.split('/').chain(["*", "**"]).collect(),
        None => glob.split('/').collect(),
    };
    let path: Vec<&str> = path.split('/').collect();
    parts_match(&glob, &path)
}

fn parts_match(glob: &[&str], path: &[&str]) -> bool {
    match glob.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| parts_match(rest, &path[skip..])),
        Some((part, rest)) => path.split_first().is_some_and(|(name, tail)| {
            part_matches(part.as_bytes(), name.as_bytes()) && parts_match(rest, tail)
        }),
    }
}

fn part_matches(glob: &[u8], name: &[u8]) -> bool {
    match glob.split_first() {
        None => name.is_empty(),
        Some((b'*', rest)) => (0..=name.len()).any(|skip| part_matches(rest, &name[skip..])),
        Some((b'?', rest)) => !name.is_empty() && part_matches(rest, &name[1..]),
        Some((byte, rest)) => name.first() == Some(byte) && part_matches(rest, &name[1..]),
    }
}

/// The name of fixture `text` when it is `<name>@sha256:<64 lowercase hex>` with a non-empty
/// name, `None` otherwise.
fn fixture_name(text: &str) -> Option<&str> {
    let (name, digest) = text.split_once('@')?;
    let hex = digest.strip_prefix("sha256:")?;
    let plain = !name.is_empty() && !name.contains(char::is_whitespace);
    let hex_ok = hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    (plain && hex_ok).then_some(name)
}

/// The validity rules of the `[[input]]` entries at `commit`: one entry per name, each valid.
fn validate_inputs(git: &Git, commit: &str, inputs: &[InputEntry]) -> Result<(), String> {
    let mut names = std::collections::HashSet::new();
    for entry in inputs {
        if !names.insert(entry.name.clone()) {
            return Err(format!("two [[input]] entries for {}", entry.name));
        }
        entry.validate(git, commit)?;
    }
    Ok(())
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
    sort_manifest(manifest);
    digest(&serde_json::to_value(&*manifest).map_err(|error| error.to_string())?)
}

fn sort_manifest(manifest: &mut [ManifestEntry]) {
    manifest.sort_by(|a, b| (&a.id, &a.environment).cmp(&(&b.id, &b.environment)));
}

/// Whether adapter entry `path` (a file, or a directory ending in `/`) covers `file`. A file
/// named by an absolute path or with a `.`, `..` or empty component is covered by no entry: a
/// command `scripts/../../bin/sh` lies outside `scripts/` whatever its prefix says.
pub(crate) fn covers(path: &str, file: &str) -> bool {
    let plain = !file.is_empty()
        && file
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..");
    if !plain {
        return false;
    }
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

    use super::{CheckEntry, covers, manifest_digest, manifest_entry};

    /// An executable that climbs out of its adapter directory is not in it.
    #[test]
    fn a_path_that_climbs_out_is_covered_by_no_adapter_entry() {
        assert!(covers("scripts/", "scripts/bay-check"));
        assert!(covers("scripts/bay-check", "scripts/bay-check"));
        for file in [
            "scripts/../../bin/sh",
            "scripts/./bay-check",
            "scripts//bay-check",
            "/scripts/bay-check",
            "",
        ] {
            assert!(!covers("scripts/", file), "{file}");
        }
    }

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

    /// Ruling R18: `fixtures` is its own key inside the definition digest, so another fixture
    /// digest is another definition (G7); an absent or empty list leaves the digest as it was.
    #[test]
    fn fixtures_enter_the_definition_digest() {
        let text = "id = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n";
        let ids = BTreeMap::new();
        let digest_of = |extra: &str| {
            let entry: CheckEntry = toml::from_str(&format!("{text}{extra}")).unwrap();
            manifest_entry(&entry, &ids).unwrap().definition_digest
        };
        let one = format!("fixtures = [\"data@sha256:{}\"]\n", "a".repeat(64));
        let two = format!("fixtures = [\"data@sha256:{}\"]\n", "b".repeat(64));
        assert_eq!(digest_of(""), digest_of("fixtures = []\n"));
        assert_ne!(digest_of(""), digest_of(&one));
        assert_ne!(digest_of(&one), digest_of(&two));
    }

    /// A check's `paths` and `materialize_timeout_s` are fields of the entry, so changing
    /// either changes its definition digest (judged at the base like the rest of policy); an
    /// absent `paths` and the default bound leave the digest as it was (the G26 vector).
    #[test]
    fn paths_and_the_materialize_bound_enter_the_definition_digest() {
        let text = "id = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n";
        let ids = BTreeMap::new();
        let digest_of = |extra: &str| {
            let entry: CheckEntry = toml::from_str(&format!("{text}{extra}")).unwrap();
            manifest_entry(&entry, &ids).unwrap().definition_digest
        };
        let plain = digest_of("");
        let board = digest_of("paths = [\"board/\", \".bay/\"]\n");
        assert_ne!(plain, board);
        assert_ne!(board, digest_of("paths = [\"board/\"]\n"));
        assert_eq!(plain, digest_of("materialize_timeout_s = 600\n"));
        assert_ne!(plain, digest_of("materialize_timeout_s = 60\n"));
    }

    /// G163: `producers` is a field of the entry, so listing or changing a check's producers
    /// changes its definition digest; an absent list leaves the digest as it was. An empty list,
    /// a bad name and a name twice are invalid.
    #[test]
    fn producers_enter_the_definition_digest() {
        let text = "id = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n";
        let ids = BTreeMap::new();
        let entry =
            |extra: &str| -> CheckEntry { toml::from_str(&format!("{text}{extra}")).unwrap() };
        let digest_of = |extra: &str| {
            manifest_entry(&entry(extra), &ids)
                .unwrap()
                .definition_digest
        };
        let plain = digest_of("");
        let one = digest_of("producers = [\"gate-1\"]\n");
        assert_ne!(plain, one);
        assert_ne!(one, digest_of("producers = [\"gate-1\", \"gate-2\"]\n"));
        assert!(super::validate_producers(&entry("")).is_ok());
        assert!(super::validate_producers(&entry("producers = [\"gate-1\"]\n")).is_ok());
        for bad in ["[]", "[\"Gate/1\"]", "[\"gate-1\", \"gate-1\"]"] {
            assert!(
                super::validate_producers(&entry(&format!("producers = {bad}\n"))).is_err(),
                "{bad}"
            );
        }
    }

    /// G62: `network = true` is a field of the entry, so granting a check the network changes
    /// its definition digest; `network = false` is the default and leaves the digest as it was.
    #[test]
    fn network_enters_the_definition_digest() {
        let text = "id = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n";
        let ids = BTreeMap::new();
        let digest_of = |extra: &str| {
            let entry: CheckEntry = toml::from_str(&format!("{text}{extra}")).unwrap();
            manifest_entry(&entry, &ids).unwrap().definition_digest
        };
        let plain = digest_of("");
        assert_eq!(plain, digest_of("network = false\n"));
        assert_ne!(plain, digest_of("network = true\n"));
    }

    /// G91, G146: `stage = "post"` and `when_paths` are fields of the entry, so either changes
    /// its definition digest; the default stage and an absent `when_paths` leave the digest as
    /// it was (the G26 vector).
    #[test]
    fn stage_and_when_paths_enter_the_definition_digest() {
        let text = "id = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n";
        let ids = BTreeMap::new();
        let digest_of = |extra: &str| {
            let entry: CheckEntry = toml::from_str(&format!("{text}{extra}")).unwrap();
            manifest_entry(&entry, &ids).unwrap().definition_digest
        };
        let plain = digest_of("");
        assert_eq!(plain, digest_of("stage = \"pre\"\n"));
        assert_ne!(plain, digest_of("stage = \"post\"\n"));
        let studio = digest_of("when_paths = [\"studio/\"]\n");
        assert_ne!(plain, studio);
        assert_ne!(
            studio,
            digest_of("when_paths = [\"studio/\", \"shared/\"]\n")
        );
    }

    /// G146: the `when_paths` globs.
    #[test]
    fn when_paths_globs_match_whole_paths() {
        use super::glob_matches;
        assert!(glob_matches("studio/", "studio/a.txt"));
        assert!(glob_matches("studio/", "studio/deep/a.txt"));
        assert!(!glob_matches("studio/", "studio"));
        assert!(!glob_matches("studio/", "studios/a.txt"));
        assert!(glob_matches("studio/**", "studio/deep/a.txt"));
        assert!(glob_matches("**/*.rs", "a.rs"));
        assert!(glob_matches("**/*.rs", "src/deep/a.rs"));
        assert!(!glob_matches("**/*.rs", "src/a.rsx"));
        assert!(glob_matches("shared/*.txt", "shared/lib.txt"));
        assert!(!glob_matches("shared/*.txt", "shared/deep/lib.txt"));
        assert!(glob_matches("Cargo.toml", "Cargo.toml"));
        assert!(!glob_matches("Cargo.toml", "sub/Cargo.toml"));
        assert!(glob_matches("src/?.rs", "src/a.rs"));
        assert!(!glob_matches("src/?.rs", "src/ab.rs"));
    }

    /// A check that could see nothing, a path bay could never find in a tree, and a bound
    /// that is always exceeded make the policy invalid.
    #[test]
    fn a_bad_paths_list_or_bound_is_invalid() {
        let text = "id = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n";
        let check = |extra: &str| {
            let entry: CheckEntry = toml::from_str(&format!("{text}{extra}")).unwrap();
            super::validate_materialization(&entry)
        };
        assert!(check("").is_ok());
        assert!(check("paths = [\"board/\", \".bay/\", \"README.md\"]\n").is_ok());
        assert!(check("materialize_timeout_s = 1\n").is_ok());
        for bad in [
            "paths = []\n",
            "paths = [\"/etc/\"]\n",
            "paths = [\"../up/\"]\n",
            "paths = [\"a//b\"]\n",
            "paths = [\"./a\"]\n",
            "paths = [\"\"]\n",
            "paths = [\"/\"]\n",
            "paths = [\"a\\\\b\"]\n",
            "paths = [\"board/\", \"board/\"]\n",
            "materialize_timeout_s = 0\n",
        ] {
            assert!(check(bad).is_err(), "{bad}");
        }
    }

    /// BACKLOG 28: `commit` is the one input bay identifies; R18: a fixture names its digest.
    #[test]
    fn an_input_or_fixture_bay_cannot_identify_is_invalid() {
        let text = "id = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n";
        let check = |extra: &str| {
            let entry: CheckEntry = toml::from_str(&format!("{text}{extra}")).unwrap();
            super::validate_declared_inputs(&entry)
        };
        let hex = "0123456789abcdef".repeat(4);
        assert!(check("inputs = [\"commit\"]\n").is_ok());
        assert!(
            check(&format!(
                "fixtures = [\"data@sha256:{hex}\", \"more@sha256:{hex}\"]\n"
            ))
            .is_ok()
        );
        for bad in [
            "inputs = [\"commits\"]\n".to_owned(),
            "inputs = [\"commit\", \"network\"]\n".to_owned(),
            "fixtures = [\"data\"]\n".to_owned(),
            format!("fixtures = [\"@sha256:{hex}\"]\n"),
            format!("fixtures = [\"data@sha1:{hex}\"]\n"),
            format!("fixtures = [\"data@sha256:{}\"]\n", hex.to_uppercase()),
            format!("fixtures = [\"data@sha256:{}\"]\n", &hex[1..]),
            format!("fixtures = [\"data@sha256:{hex}\", \"data@sha256:{hex}\"]\n"),
        ] {
            assert!(check(&bad).is_err(), "{bad}");
        }
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
