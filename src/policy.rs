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
use crate::git::Git;
use crate::inputs::InputEntry;

pub const POLICY_PATH: &str = ".bay/policy.toml";

/// The file at the root that says how each path collides (BACKLOG 65): `lockable`, `bay-offload`.
pub const GITATTRIBUTES: &str = ".gitattributes";
const DEFAULT_TIMEOUT_S: u64 = 1800;

/// The `inputs` value of a check that reads commit metadata: its evidence is keyed by subject
/// commit (§ 7).
pub const INPUT_COMMIT: &str = "commit";
/// The `inputs` value of a check that reads history: keyed by subject commit like
/// [`INPUT_COMMIT`], and its request carries `git`, read-only access to the repository's objects
/// (`INTERFACES` § 3).
pub const INPUT_HISTORY: &str = "history";

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
    /// The site's own completion dimensions (G150): `[[dimension]] name, kind, producers`.
    #[serde(default)]
    dimension: Vec<crate::completion::SiteDimension>,
    /// What the policy asks of the acting agent (G152).
    #[serde(default)]
    agent: AgentPolicy,
    /// The `[policy]` table (N-21): what a key registry signs.
    #[serde(default)]
    policy: PolicyTable,
    /// The `[limits]` table (owner ruling 2026-10-21): how big one change may be.
    #[serde(default)]
    limits: crate::limits::Limits,
    /// The `[locks]` table 0.5.0 removed (BACKLOG 65): read only to name its new home.
    #[serde(default)]
    locks: Option<toml::Value>,
    /// The `[assets]` table (BACKLOG 49): how long the store keeps generated files.
    #[serde(default)]
    assets: crate::assets::AssetsPolicy,
    /// The `[talk]` table (owner ruling 2026-10-25, the talk uplift).
    #[serde(default)]
    talk: TalkPolicy,
}

/// The `[talk]` table (owner ruling 2026-10-25): talk is off unless the policy on main says
/// `enabled = true`. Off, `bay talk` refuses, an accept writes nothing and a bay host refuses
/// talk refs. The other keys narrow it, each only when it names something: `kinds` (talk kinds;
/// any other value is `POLICY_INVALID`), `senders` and `recipients` (actor names), `max_round`
/// (the most recipients of one fan-out) and `wake = false` (talks arrive, nothing is woken).
/// `follow_up_s` is how long a talk sent with no `--follow-up` is owed a reply,
/// [`crate::talk::BAY_TALK_FOLLOW_UP_S`] when unnamed; 0, a reply owed at once, is
/// `POLICY_INVALID`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TalkPolicy {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub kinds: Vec<String>,
    #[serde(default)]
    pub senders: Vec<String>,
    #[serde(default)]
    pub recipients: Vec<String>,
    #[serde(default)]
    pub max_round: Option<u32>,
    #[serde(default)]
    pub wake: Option<bool>,
    #[serde(default)]
    pub follow_up_s: Option<u64>,
    /// G564: why main's policy is `POLICY_INVALID`, when no earlier valid policy judges talk.
    #[serde(skip)]
    pub invalid: Option<String>,
}

impl TalkPolicy {
    /// How long a talk sent with no `--follow-up` is owed a reply, in seconds.
    #[must_use]
    pub fn follow_up_s(&self) -> u64 {
        self.follow_up_s
            .unwrap_or(crate::talk::BAY_TALK_FOLLOW_UP_S)
    }

    /// How `bay status --all` names it: whether talk is on, the kinds it allows (every kind when
    /// the policy names none), and whether the policy states `enabled` at all.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let kinds: Vec<&str> = if self.kinds.is_empty() {
            crate::talk::KIND_NAMES.to_vec()
        } else {
            self.kinds.iter().map(String::as_str).collect()
        };
        serde_json::json!({
            "enabled": self.on(),
            "stated": self.enabled.is_some(),
            "kinds": kinds,
            "policy_invalid": self.invalid,
        })
    }

    /// Whether talk is on: only `enabled = true` turns it on.
    #[must_use]
    pub fn on(&self) -> bool {
        self.enabled == Some(true)
    }

    /// Why this table refuses a talk with `body`, if it does: talk is off, or the table names
    /// kinds, senders or recipients and not the body's.
    #[must_use]
    pub fn refusal(&self, body: &Value) -> Option<String> {
        if !self.on() {
            return Some(self.invalid.as_ref().map_or_else(
                || format!(
                    "talk is off in this repository: the policy on main ({POLICY_PATH}) does not say `[talk] enabled = true`"
                ),
                |why| format!(
                    "talk is off in this repository: the policy on main ({POLICY_PATH}) is POLICY_INVALID, and no earlier policy on main is valid: {why}"
                ),
            ));
        }
        [
            ("kinds", &self.kinds, &body["kind"]),
            ("senders", &self.senders, &body["from"]["name"]),
            ("recipients", &self.recipients, &body["who"]),
        ]
        .into_iter()
        .find(|(_, names, value)| {
            !names.is_empty() && !names.iter().any(|name| value.as_str() == Some(name))
        })
        .map(|(key, names, value)| {
            format!("the policy on main allows only `[talk] {key} = {names:?}`, not {value}")
        })
    }

    /// Whether a talk that arrives may wake its recipient: not under `wake = false`.
    #[must_use]
    pub fn wakes(&self) -> bool {
        self.wake != Some(false)
    }
}

/// The `[talk]` table of the policy at `commit`. No policy, or one that does not parse, is
/// talk off: unknown never turns talk on.
///
/// # Errors
///
/// A git fault.
pub fn talk_at(git: &Git, commit: &str) -> Result<TalkPolicy, String> {
    match load_at(git, commit) {
        Ok(policy) => Ok(policy.map(|policy| policy.talk).unwrap_or_default()),
        Err(LoadError::Git(why)) => Err(why),
        // G564 (BACKLOG #132; a fleet finding, 2026-10-29: a policy that broke a rule turned talk
        // off, and the refusal said only that it did not say `enabled = true`): a broken policy
        // judges talk as it judges a merge, by the last valid policy on main (B2); with none,
        // talk is off and its refusal names the fault.
        Err(LoadError::Invalid(why)) => Ok(last_valid_at(git, commit).map_or_else(
            || TalkPolicy {
                invalid: Some(why),
                ..TalkPolicy::default()
            },
            |(_, policy)| policy.talk,
        )),
    }
}

/// B2: the newest commit on `commit`'s first-parent line that changes `.bay/` and whose policy
/// loads, and that policy; `None` when none of the last 50 such commits has a valid one.
#[must_use]
pub fn last_valid_at(git: &Git, commit: &str) -> Option<(String, Policy)> {
    let listed = git
        .run(&[
            "rev-list",
            "--first-parent",
            "-n",
            "50",
            commit,
            "--",
            ".bay/",
        ])
        .ok()?;
    listed
        .lines()
        .find_map(|commit| match load_at(git, commit) {
            Ok(Some(policy)) => Some((commit.to_owned(), policy)),
            _ => None,
        })
}

/// The `[policy]` table: `signing = "all"` (the default) or `"approvals"` (N-21). Any other value
/// does not parse, so the policy is `POLICY_INVALID`. `reuse_unsigned = true` (G318) lets
/// evidence reuse and batching run on evidence that counts unsigned; off by default.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyTable {
    #[serde(default)]
    signing: Signing,
    #[serde(default)]
    reuse_unsigned: bool,
}

/// What a key registry signs (N-21, `[policy] signing`), judged at the accepted base like every
/// policy key, so a candidate never switches the mode of its own landing. It applies only where
/// a registry exists: without one nothing is signed and no approval counts, whatever it says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Signing {
    /// Every landing, request, chain and evidence record is signed by an enrolled key (§ 10a).
    #[default]
    All,
    /// Only `approval` records are signed, by a key of the registry's approver file (G245).
    /// Landings, requests, chains and every other record run as with no registry: unsigned, and
    /// they count unsigned. For a small team with no executor host.
    Approvals,
}

impl Signing {
    /// The value as the policy and results name it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Approvals => "approvals",
        }
    }
}

/// The `[policy] signing` mode of the policy at `commit` (N-21), judged at the base like every
/// other policy key: [`load_at`]'s, read once per process with the rest of the policy. No
/// policy is [`Signing::All`]. An invalid policy keeps the mode its text states
/// ([`signing_stated_at`]): it lands nothing, and its refusal is `POLICY_INVALID`, never a
/// missing key.
///
/// # Errors
///
/// A git fault, or a policy file the tree names whose blob cannot be read (G104b).
pub fn signing_at(git: &Git, commit: &str) -> Result<Signing, String> {
    match load_at(git, commit) {
        Ok(policy) => Ok(policy.map_or(Signing::All, |policy| policy.signing)),
        Err(LoadError::Git(why)) => Err(why),
        Err(LoadError::Invalid(_)) => signing_stated_at(git, commit),
    }
}

/// The `[policy] signing` mode the policy text at `commit` states, with no other part of the
/// policy judged: for a reader of many commits, such as each landing's parent (N-21). Only
/// `signing = "approvals"` is [`Signing::Approvals`]: no policy, no key, another value or a text
/// that does not parse is [`Signing::All`], so unknown never weakens signing.
///
/// # Errors
///
/// A git fault, or a policy file the tree names whose blob cannot be read (G104b).
pub fn signing_stated_at(git: &Git, commit: &str) -> Result<Signing, String> {
    // Read once per process for a fixed commit, kept only when the file was there: no policy
    // may be a commit not fetched yet.
    crate::memo::at(
        "signing",
        git,
        commit,
        |(read, present): &(Result<Signing, String>, bool)| read.is_ok() && *present,
        || match git.file_at(commit, POLICY_PATH) {
            Ok(Some(text)) => (Ok(signing_in(&text)), true),
            Ok(None) => (Ok(Signing::All), false),
            Err(error) => (Err(error.message()), false),
        },
    )
    .0
}

/// The mode policy text `text` states: [`Signing::Approvals`] only for `[policy] signing =
/// "approvals"`.
fn signing_in(text: &str) -> Signing {
    let approvals = toml::from_str::<toml::Table>(text).is_ok_and(|table| {
        table
            .get("policy")
            .and_then(|policy| policy.get("signing"))
            .and_then(toml::Value::as_str)
            == Some(Signing::Approvals.name())
    });
    if approvals {
        Signing::Approvals
    } else {
        Signing::All
    }
}

/// The `[agent]` table: `require_actor = true` makes `bay start` refuse `ACTOR_UNNAMED` while
/// the actor's name comes from a default (G152). `require_actor_proof = true` makes `bay merge`
/// refuse `ACTOR_UNPROVEN` while the agent key cannot prove which actor signs: several actors
/// run sessions under this OS account, which can read a file-based agent key (G243b).
/// `max_live_changes = N` makes `bay start` refuse `WIP_LIMIT` while the actor already holds N
/// live changes, a lane change among them (G264). `question_wait_s = N` (dossier 63) has a
/// question tool's hook wait up to N seconds, at most [`MAX_QUESTION_WAIT_S`], for a person's
/// signed answer on the question's talk. None refuses or waits by default.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPolicy {
    #[serde(default)]
    pub require_actor: bool,
    #[serde(default)]
    pub require_actor_proof: bool,
    #[serde(default)]
    pub max_live_changes: Option<u64>,
    #[serde(default)]
    pub question_wait_s: Option<u64>,
}

/// The longest a question tool's hook waits for an answer (owner ruling 2026-10-25, ~90 s):
/// while it waits, the person at the terminal sees only that a hook runs.
pub const MAX_QUESTION_WAIT_S: u64 = 90;

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
    /// Who answers for the quarantine until it ends: shown by `bay status` and `bay doctor`, so
    /// a set-aside check has a name beside it. Absent on entries written before the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// The date (`YYYY-MM-DD`) the check was set aside, from which `bay status` counts its age.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
}

impl Quarantine {
    /// Whether the quarantine is past its end date on `today` (`YYYY-MM-DD`): it stays in
    /// force and is shown as overdue. An end that names a change is never overdue by date.
    #[must_use]
    pub fn overdue(&self, today: &str) -> bool {
        is_date(&self.end) && self.end.as_str() < today
    }

    /// Whole days from `since` to `today` (both `YYYY-MM-DD`); `None` without a `since`.
    #[must_use]
    pub fn age_days(&self, today: &str) -> Option<i64> {
        days_between(self.since.as_deref()?, today)
    }

    fn sets_aside(&self, entry: &CheckEntry) -> bool {
        self.id == entry.id && self.environment == entry.environment
    }
}

/// Whole days from date `from` to date `to` (`YYYY-MM-DD`), negative when `to` is earlier.
fn days_between(from: &str, to: &str) -> Option<i64> {
    let day = |date: &str| {
        let secs = crate::clock::unix_of(&format!("{date}T00:00:00Z"))?;
        i64::try_from(secs / 86_400).ok()
    };
    Some(day(to)? - day(from)?)
}

// Each bool is its own policy key, read from TOML; an enum would not read as one.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckEntry {
    pub id: String,
    pub environment: String,
    /// What answers the check (G245): `check`, the default, a run of `command` whose record bay
    /// writes; `approval`, a person's signed `approval` record for the exact candidate tree,
    /// never run. Judged at the base; `approval` enters the definition digest, and the default
    /// is left out of the canonical entry (the G26 vector).
    #[serde(default, skip_serializing_if = "CheckKind::is_check")]
    pub kind: CheckKind,
    /// Required of a `check`, refused on an `approval`. Left out of the canonical entry when
    /// empty, which only an approval's is, so a run check keeps its digest.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,
    /// Required of a `check` (its command is in it), refused on an `approval`; left out of the
    /// canonical entry when empty, like `command`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adapter: Vec<String>,
    /// Required of a `check`, no default: the minimum number of tests the check must collect.
    /// Refused on an `approval`, which collects none. Read through [`CheckEntry::minimum`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_minimum: Option<u64>,
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
    /// Declared inputs beyond the tree. `commit` keys the check's evidence by subject commit
    /// (§ 7): it reads commit metadata, so a result for one commit never stands for another
    /// with the same tree. `history` keys it the same way and also gives the check read-only
    /// access to the repository's objects through the request's `git` (`INTERFACES` § 3): a
    /// verdict that judges history is no function of the tree alone. A check whose verdict
    /// reads the request's `base`, `change` or `messages` declares one of the two. Omitted from
    /// the definition digest when empty.
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
    /// its materialization. Absent, the check gets the whole tree. With `materialize =
    /// "changed"`, the check sees these paths and the candidate's changed paths. A field of the
    /// entry, so it is judged at the base and enters the definition digest; left out of the
    /// canonical entry when absent, so a policy without it keeps its digest (the G26 vector).
    /// An empty list makes the policy invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    /// N-18: what of the candidate's tree is materialized for the check. Absent, the whole
    /// tree (or its `paths`). `"changed"`: only the paths the candidate's diff against its base
    /// changes (the request's `changed_paths`; a deleted path is simply absent), together with
    /// the entry's `paths` when it lists them, and the request says `"materialized":
    /// "changed"`. What such a check sees depends on the base as well as the tree, so its
    /// evidence is keyed by subject commit, as `inputs = ["commit"]` keys it
    /// ([`CheckEntry::commit_keyed`]): the candidate commit names its base, so a record never
    /// answers another changed set. Refused with `footprint = true`, which carries a record
    /// onto another changed set, and on an approval. Judged at the base; `"changed"` enters the
    /// definition digest, and absent is left out of the canonical entry (the G26 vector).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub materialize: Option<Materialize>,
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
    /// paths the change declared. G500: a glob that starts with `!` excludes the paths it
    /// matches, read in order as `.gitignore` reads it: the last glob that matches a path
    /// decides ([`globs_match`]), so `["**/AGENTS.md", "!notes/**"]` is every `AGENTS.md`
    /// outside `notes/`, and a later include takes a path back. Judged at the base and in the
    /// definition digest, so a candidate cannot narrow it for itself; left out of the canonical
    /// entry when absent. An empty list, or one that starts with an exclusion, makes the policy
    /// invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_paths: Option<Vec<String>>,
    /// The producers whose records answer this check (G163, owner ruling Q3): a record from any
    /// other producer is reported `unlisted_producer` and does not count for it. Absent, any
    /// producer answers. Judged at the base and in the definition digest, so a candidate that
    /// edits the list is a policy change checked twice and never widens it for itself (G26);
    /// left out of the canonical entry when absent. An empty list makes the policy invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producers: Option<Vec<String>>,
    /// NEW-10 (`crate::throughput`): the check's verdict depends on nothing but the content of
    /// its read set ([`CheckEntry::read_set`]) in the candidate's tree and in its base's, so a
    /// record of it holds for a later candidate whose read set is the same, byte for byte, and
    /// is carried onto it. An opt-in: a check that reads outside its `when_paths` would be
    /// carried onto a tree it never judged. Judged at the base; `true` enters the definition
    /// digest, and the default is left out of the canonical entry (the G26 vector). It needs
    /// `paths` or `when_paths`.
    ///
    /// `footprint = "observed"` (items 1 and 2, owner ruling 2026-10-25): bay records what the
    /// check read and listed in its tree ([`crate::check::observe`]), and that observed read
    /// set is the record's footprint. No `paths` is needed, and a read set that goes stale is
    /// impossible.
    #[serde(default, skip_serializing_if = "FootprintMode::is_off")]
    pub footprint: FootprintMode,
    /// G292: whether an approval check's record also answers a later candidate of the same
    /// change whose `when_paths` content, in its tree and in its base's, is byte for byte the
    /// approved candidate's ([`CheckEntry::carries`], `crate::throughput::carried_approvals`).
    /// A person approves the change to the gated files, so an unrelated landing does not ask
    /// them again. Absent, an approval check carries; `carry = false` asks an approval of every
    /// candidate. Only an approval check names it. Judged at the base; it enters the definition
    /// digest only when set, and absent is left out of the canonical entry (the G26 vector).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carry: Option<bool>,
    /// How many times the executor runs the check again on the identical tree after a
    /// `product_failure`, at most [`MAX_RETRY_ON_FAILURE`]; 0, the default, runs it once. Only a
    /// `product_failure` is run again: an `unknown` or `infra_failure` is no answer and never
    /// cancels a pass (`DESIGN` § 7), so a rerun could not tell a flake from it. Every run is a
    /// record on the tree, so a failure and a later pass combine to `flaky`, never `pass`.
    /// Judged at the base and in the definition digest; left out of the canonical entry at 0.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub retry_on_failure: u32,
    /// Fail fast (a fleet finding, 2026-10-25: half the night's refusals failed a seconds-long
    /// check but paid for every slow one): a check with `first = true` runs before every other
    /// check of the run, and a failure there ends the run, so no slow check starts. It orders
    /// the run and changes nothing a check computes, so it is in no digest.
    #[serde(default, skip_serializing)]
    pub first: bool,
    /// Whether a `flaky` verdict of this check (a pass and a `product_failure` on one tree) lets
    /// the landing proceed. False, the default, refuses it `CHECK_FLAKY`. A flaky landing is
    /// named in the land result's `coverage_gaps` with every run, never as a plain pass. Judged
    /// at the base and in the definition digest; left out of the canonical entry when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_flaky: bool,
    /// N-3: what a failing unit is judged against. Absent, the check passes only when every unit
    /// of its result passes (whole-green). `"base"`: a unit that fails on the candidate fails the
    /// check only when the same unit (by `id`) passed on the base, or the base has no such unit;
    /// a unit that fails on both is pre-existing, and the landing names it under `coverage_gaps`
    /// as a `baseline_failures` entry, never as a pass. The base's result comes from its own
    /// record of this check under the same manifest, or from one run of the check on the base,
    /// recorded as the base's. A base run that is `unknown` or `infra_failure` makes the
    /// candidate's run `unknown`. bay cannot know before a run whether a result reports units;
    /// a result without units has nothing to compare, so every failure in it is new. Judged at
    /// the base; `"base"` enters the definition digest, and absent is left out of the canonical
    /// entry (the G26 vector). Its one value is `"base"`; any other is `POLICY_INVALID`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<Baseline>,
    /// N-9: whether a person may waive this check on one candidate. With `waivable = true`, an
    /// enrolled approver's signed `waiver` record (`bay approve <change> --waive <check> --why
    /// <text>`) about the exact candidate commit, with a reason, lets the check through when
    /// its own records do not pass ([`Policy::waiver`]). A waiver is never a pass: the landing
    /// names it under `coverage_gaps` with the check's real verdict. Refused on an approval
    /// check, where a person's approval is the answer. Judged at the base; `true` enters the
    /// definition digest, and the default is left out of the canonical entry (the G26 vector).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub waivable: bool,
}

/// A check's `baseline` (N-3): what a failing unit is compared with.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Baseline {
    /// The same check's result on the trunk commit the candidate was merged onto.
    Base,
}

/// A check's `materialize` (N-18): what of the candidate's tree its run gets.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Materialize {
    /// Only the paths the candidate changes against its base, and the entry's `paths`.
    Changed,
}

/// The most reruns a `[[check]]` may ask for with `retry_on_failure`: a check that fails every
/// run is run this many times more, so the bound keeps a typo from holding the executor.
pub const MAX_RETRY_ON_FAILURE: u32 = 5;

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(count: &u32) -> bool {
    *count == 0
}

/// A check's `kind` (G245).
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CheckKind {
    /// Bay runs the check's command and writes its record.
    #[default]
    Check,
    /// A person answers: an enrolled approver's signed `approval` record for the candidate tree.
    Approval,
}

impl CheckKind {
    #[allow(clippy::trivially_copy_pass_by_ref)]
    fn is_check(&self) -> bool {
        *self == Self::Check
    }

    /// The evidence record kind that answers a check of this kind.
    #[must_use]
    pub const fn record_kind(self) -> &'static str {
        match self {
            Self::Check => crate::evidence::KIND_CHECK,
            Self::Approval => crate::evidence::KIND_APPROVAL,
        }
    }
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
    /// The minimum number of tests the check must collect: its `expected_minimum`, which a valid
    /// run check always states; zero for an approval, which runs nothing.
    #[must_use]
    pub fn minimum(&self) -> u64 {
        self.expected_minimum.unwrap_or(0)
    }

    /// Whether the check's evidence is keyed by subject commit: it declares the input `commit`
    /// or `history`, or sees only its changed paths (`materialize = "changed"`, N-18), so its
    /// verdict reads more than the tree, and a result for one commit never stands for another
    /// with the same tree. A changed-only check sees what the candidate changes against its
    /// base; the candidate commit names that base, so its record never answers a candidate on
    /// another base, whose changed set differs.
    #[must_use]
    pub fn commit_keyed(&self) -> bool {
        self.changed_only()
            || self
                .inputs
                .iter()
                .any(|input| input == INPUT_COMMIT || input == INPUT_HISTORY)
    }

    /// Whether the check sees only the candidate's changed paths and its `paths`
    /// (`materialize = "changed"`, N-18).
    #[must_use]
    pub fn changed_only(&self) -> bool {
        self.materialize == Some(Materialize::Changed)
    }

    /// What of the candidate's tree a run of the check gets, for a candidate whose diff
    /// against its base changes `changed`: `None` for the whole tree, else the paths its
    /// materialization keeps. A changed-only check (N-18) keeps `changed` and its `paths`,
    /// sorted and once each; any other keeps its `paths`, or the whole tree without them.
    #[must_use]
    pub fn materialized(&self, changed: &[String]) -> Option<Vec<String>> {
        if !self.changed_only() {
            return self.paths.clone();
        }
        let mut kept: Vec<String> = changed
            .iter()
            .chain(self.paths.iter().flatten())
            .cloned()
            .collect();
        kept.sort();
        kept.dedup();
        Some(kept)
    }

    /// Whether the check reads history: it declares the input `history`, so its request
    /// carries `git`, read-only access to the repository's objects (`INTERFACES` § 3).
    #[must_use]
    pub fn reads_history(&self) -> bool {
        self.inputs.iter().any(|input| input == INPUT_HISTORY)
    }

    /// Whether a person answers the check rather than a run (G245).
    #[must_use]
    pub fn is_approval(&self) -> bool {
        self.kind == CheckKind::Approval
    }

    /// Whether `changed`, the paths the candidate's diff against its base changes, wakes this
    /// check (G146): always without `when_paths`, else when the list matches one of them
    /// ([`globs_match`], G500: a path it excludes wakes nothing).
    #[must_use]
    pub fn wakes(&self, changed: &[String]) -> bool {
        self.when_paths
            .as_ref()
            .is_none_or(|globs| changed.iter().any(|path| globs_match(globs, path)))
    }

    /// The paths of `changed` this check's `when_paths` match, in their order; every one of
    /// them without `when_paths`. What a person approves (G280).
    #[must_use]
    pub fn woken_paths(&self, changed: &[String]) -> Vec<String> {
        changed
            .iter()
            .filter(|path| {
                self.when_paths
                    .as_ref()
                    .is_none_or(|globs| globs_match(globs, path))
            })
            .cloned()
            .collect()
    }

    /// G292: whether this is an approval check whose record carries onto a later candidate of
    /// the same change with the same `when_paths` content: every approval check but one that
    /// says `carry = false`.
    #[must_use]
    pub fn carries(&self) -> bool {
        self.is_approval() && self.carry != Some(false)
    }

    /// NEW-10: what a check with `footprint = true` reads, its verdict's whole input besides
    /// its definition: its `paths` (the only part of the tree its materialization holds) when it
    /// has them, else its `when_paths`. `None` for a check without `footprint = true`. G292: an
    /// approval check that carries reads its `when_paths`; one with `carry = false` reads none.
    #[must_use]
    pub fn read_set(&self) -> Option<&[String]> {
        // G292: an approval check that carries reads its `when_paths`: what a person approves.
        if self.is_approval() {
            return if self.carries() {
                self.when_paths.as_deref()
            } else {
                None
            };
        }
        if self.footprint != FootprintMode::Declared {
            return None;
        }
        self.paths.as_deref().or(self.when_paths.as_deref())
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

    /// Whether a `flaky` verdict of check `id` lets a landing proceed (`allow_flaky`).
    #[must_use]
    pub fn allows_flaky(&self, id: &crate::evidence::CheckId) -> bool {
        self.check(id).is_some_and(|check| check.allow_flaky)
    }

    /// The checks of `required` that `records` do not pass for `commit` (G30), with their
    /// combined verdicts, less each `flaky` one whose entry says `allow_flaky`, and less each
    /// waivable one that a valid waiver answers on `commit` (N-9, [`Policy::waiver`]). The
    /// executor and the host hook judge a landing by this one rule.
    #[must_use]
    pub fn gaps(
        &self,
        records: &[crate::evidence::EvidenceRecord],
        required: &[ManifestEntry],
        commit: &str,
    ) -> Vec<(crate::evidence::CheckId, crate::evidence::Combined)> {
        crate::evidence::aggregate(
            records,
            required,
            &self.manifest_digest,
            &|id| self.commit_keyed(id),
            commit,
        )
        .into_iter()
        .filter(|(id, combined)| {
            *combined != crate::evidence::Combined::Flaky || !self.allows_flaky(id)
        })
        // N-9: a waived check is no gap here; the landing names it under `coverage_gaps`.
        .filter(|(id, _)| self.waiver(records, id, commit).is_none())
        .collect()
    }

    /// N-9: the waiver among `records` that lets check `id` through on candidate `commit`, if
    /// any: the check's entry says `waivable = true` (and is no approval check); the record is
    /// a `waiver` of that check and environment, under this policy's manifest, about exactly
    /// `commit` (never another candidate, even one with the same tree: a waiver never carries),
    /// with a non-empty `why`. `records` are counted records ([`crate::evidence::counted`]), so
    /// only an enrolled approver's signed waiver is among them, and none without a registry.
    #[must_use]
    pub fn waiver<'r>(
        &self,
        records: &'r [crate::evidence::EvidenceRecord],
        id: &crate::evidence::CheckId,
        commit: &str,
    ) -> Option<&'r crate::evidence::EvidenceRecord> {
        if !self
            .check(id)
            .is_some_and(|entry| entry.waivable && !entry.is_approval())
        {
            return None;
        }
        records.iter().find(|record| {
            record.kind == crate::evidence::KIND_WAIVER
                && record.check == *id
                && record.manifest_digest == self.manifest_digest
                && record.subject.commit == commit
                && record
                    .why
                    .as_deref()
                    .is_some_and(|why| !why.trim().is_empty())
        })
    }

    /// Whether check `id` is keyed by subject commit ([`CheckEntry::commit_keyed`]).
    #[must_use]
    pub fn commit_keyed(&self, id: &crate::evidence::CheckId) -> bool {
        self.check(id).is_some_and(CheckEntry::commit_keyed)
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

    /// Whether bay runs check `entry` for a candidate whose diff against its base changes
    /// `changed`: it is required ([`Policy::requires`]) and no approval, which a person's
    /// record answers and nothing runs (G245).
    #[must_use]
    pub fn runs(&self, entry: &CheckEntry, changed: &[String]) -> bool {
        !entry.is_approval() && self.requires(entry, changed)
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
    /// `changed` (G146), or `None` when the diff wakes it: it has `when_paths`, and the list
    /// matches no changed path. A change to the policy file or an adapter path wakes every check: a
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

    /// G560: the first pre-merge check keyed by its commit that `changed` wakes. Its record
    /// answers one commit only, so a member whose diff wakes one never lands in a batch, whose
    /// candidate commit that check never saw; a member that wakes none batches.
    #[must_use]
    pub fn commit_keyed_woken(&self, changed: &[String]) -> Option<&CheckEntry> {
        self.checks.iter().find(|entry| {
            entry.stage != Stage::Post
                && entry.commit_keyed()
                && self.unwoken(entry, changed).is_none()
        })
    }

    /// Whether `changed` holds this policy's file or one of its adapter paths: a change that
    /// is judged twice, and that every check after it is judged by another policy than this
    /// one (§ 7; `crate::throughput` predicts nothing past it).
    #[must_use]
    pub fn touches_policy(&self, changed: &[String]) -> bool {
        let paths = self.paths();
        changed
            .iter()
            .any(|file| paths.iter().any(|path| covers(path, file)))
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
    /// combine, under each policy that judges, so the base's list binds a candidate. G245: a
    /// record answers only a check of its own kind, so a producer's `check` pass never answers an
    /// approval check, and an approval never answers a check bay runs.
    #[must_use]
    pub fn listed(&self, record: &crate::evidence::EvidenceRecord) -> bool {
        // N-9: a waiver stands beside a waivable check's records, never in place of them; an
        // enrolled approver signs it, so the check's `producers` list does not name its writer.
        if record.kind == crate::evidence::KIND_WAIVER {
            return self
                .check(&record.check)
                .is_some_and(|check| check.waivable && !check.is_approval());
        }
        self.check(&record.check).is_none_or(|check| {
            check.kind.record_kind() == record.kind
                && check
                    .producers
                    .as_ref()
                    .is_none_or(|names| names.contains(&record.producer.name))
        })
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
    /// Every path this policy is made of: the policy file, `.gitattributes` (BACKLOG 65: how each
    /// path collides, lockable or offloaded, is said there), and each adapter path of its checks
    /// and its input providers, sorted.
    #[must_use]
    pub fn paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = [POLICY_PATH.to_owned(), GITATTRIBUTES.to_owned()]
            .into_iter()
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
            && git.is_ancestor(last, bootstrap)?
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

/// The files `commit` changed against its first parent (every file, for a root commit), read
/// as [`crate::check::changed_paths`] reads them: NUL-separated, so Git never quotes a name.
fn changed_paths(git: &Git, commit: &str) -> Result<Vec<String>, String> {
    let parent = format!("{commit}^1");
    let has_parent = git.run(&["rev-parse", "--verify", "-q", &parent]).is_ok();
    if has_parent {
        return crate::check::changed_paths(git, &parent, commit);
    }
    let out = git
        .run(&["ls-tree", "-r", "-z", "--name-only", commit])
        .map_err(|error| error.message())?;
    Ok(out
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect())
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
///
/// # Errors
///
/// The tree names the policy file and its blob cannot be read (G104b), or a git fault.
pub fn revision_at(git: &Git, commit: &str) -> Result<Option<u64>, String> {
    let Some(text) = git
        .file_at(commit, POLICY_PATH)
        .map_err(|error| error.message())?
    else {
        return Ok(None);
    };
    Ok(toml::from_str::<toml::Table>(&text)
        .ok()
        .and_then(|table| table.get("revision")?.as_integer())
        .and_then(|revision| u64::try_from(revision).ok()))
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
    /// The `[[dimension]]` tables (G150): completion dimensions the site defines, reported by
    /// `bay status` and required only through a profile.
    pub dimensions: Vec<crate::completion::SiteDimension>,
    /// The digest of the sorted manifest (`INTERFACES` § 7), its post-publication entries with
    /// it: a record of a post check is judged under the same digest. A profile is not in it: a profile
    /// says what a change needs, not what a check computes, so editing one leaves every check's
    /// evidence current. It is a policy change all the same (`revision`, ruling R12).
    pub manifest_digest: String,
    /// The `[agent]` table (G152).
    pub agent: AgentPolicy,
    /// `[policy] signing` (N-21): what a key registry signs.
    pub signing: Signing,
    /// `[policy] reuse_unsigned = true` (G318, owner ruling 2026-10-22): evidence reuse,
    /// pre-checks, footprint carry and batching run where evidence counts unsigned (no
    /// registry, or `signing = "approvals"`). There the team already takes an unsigned record
    /// as the check's own pass, so taking it for an equal tree assumes no new trust; results
    /// still name `trust: unsigned` or `approvals`. With a registry that signs everything it
    /// changes nothing. Judged at the accepted base like every policy key, so a candidate never
    /// turns it on for its own landing. Like `signing` and a profile, it is in no digest: it
    /// says which records may stand for a run, not what a check computes, and a policy that
    /// sets it is a policy change all the same (`revision`).
    pub reuse_unsigned: bool,
    /// `[limits]` (owner ruling 2026-10-21): how big one change may be.
    pub limits: crate::limits::Limits,
    /// `[talk]` (owner ruling 2026-10-25): read and named, with no effect before 0.6.0.
    pub talk: TalkPolicy,
}

/// Why a policy could not be loaded.
#[derive(Clone, Debug)]
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
/// has a run check without its `command` or `expected_minimum`, or an approval check that names
/// what only a run uses, is `post`, or has no `when_paths` (G245), or, not unchecked, has no
/// required pre-publication check whose `expected_minimum` is above zero.
/// [`LoadError::Git`] on a git fault, or a policy file the tree names whose blob cannot be read
/// (G104b): unreadable is never "no policy".
pub fn load_at(git: &Git, commit: &str) -> Result<Option<Policy>, LoadError> {
    // The policy at a commit never changes: loaded once per process (plan B2). Kept only when
    // it loads, or is invalid by its text alone: a git fault may pass on a retry, no policy may
    // be a commit not fetched yet, and an invalid judgement that ran git (an adapter path, an
    // input) may rest on a failed or timed-out call.
    crate::memo::at(
        "policy",
        git,
        commit,
        |(loaded, ran_git): &(Result<Option<Policy>, LoadError>, bool)| match loaded {
            Ok(policy) => policy.is_some(),
            Err(LoadError::Invalid(_)) => !ran_git,
            Err(LoadError::Git(_)) => false,
        },
        || {
            let mut ran_git = false;
            let loaded = read_at(git, commit, &mut ran_git);
            (loaded, ran_git)
        },
    )
    .0
}

/// The blob or tree id of every adapter path of `entry` at `commit`, in one `cat-file
/// --batch-check`, never one process per path (a fleet finding, 2026-10-26: 25 paths across 50
/// landings were 1,250 git calls in one status). An absent path makes the policy invalid.
fn adapter_ids(
    git: &Git,
    commit: &str,
    entry: &CheckEntry,
    invalid: &dyn Fn(String) -> LoadError,
) -> Result<BTreeMap<String, Value>, LoadError> {
    let mut input = String::new();
    for path in &entry.adapter {
        input.push_str(commit);
        input.push(':');
        input.push_str(path.trim_end_matches('/'));
        input.push('\n');
    }
    let out = git
        .run_with(
            &["cat-file", "--batch-check=%(objectname)"],
            &[],
            Some(input.as_bytes()),
        )
        .map_err(|error| LoadError::Git(error.message()))?;
    let mut answers = out.lines();
    let mut ids = BTreeMap::new();
    for path in &entry.adapter {
        let oid = answers
            .next()
            .filter(|line| !line.ends_with(" missing") && !line.is_empty())
            .ok_or_else(|| invalid(format!("adapter path {path} is absent")))?;
        ids.insert(path.clone(), Value::String(oid.to_owned()));
    }
    Ok(ids)
}

/// [`load_at`], read from Git every time. `ran_git` is set once the judgement runs any git
/// call past the policy text itself.
fn read_at(git: &Git, commit: &str, ran_git: &mut bool) -> Result<Option<Policy>, LoadError> {
    // G104b: the tree says whether the file is there; a file it names that cannot be read is a
    // fault naming it, never "no policy".
    let text = match git.file_at(commit, POLICY_PATH) {
        Ok(Some(text)) => text,
        Ok(None) => return Ok(None),
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
            .and_then(|()| validate_kind(entry))
            .and_then(|()| validate_footprint(entry))
            .and_then(|()| validate_retry(entry))
            .map_err(invalid)?;
        // G245: an approval runs nothing, so it names no executable and no adapter.
        if !entry.is_approval() && !entry.adapter.iter().any(|path| covers(path, executable)) {
            return Err(invalid(format!(
                "check {} runs {executable}, which is not in its adapter list",
                entry.id
            )));
        }
        *ran_git = true;
        let adapter_ids = adapter_ids(git, commit, entry, &invalid)?;
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
    *ran_git = true;
    if let Some(kind) = file
        .talk
        .kinds
        .iter()
        .find(|kind| !crate::talk::KIND_NAMES.contains(&kind.as_str()))
    {
        return Err(invalid(format!(
            "[talk] kinds names {kind:?}; a talk kind is one of {}",
            crate::talk::KIND_NAMES.join(", ")
        )));
    }
    if file.talk.follow_up_s == Some(0) {
        return Err(invalid(
            "[talk] follow_up_s is 0: a reply owed at once is owed before any talk arrives; name the seconds after sending it is owed".to_owned(),
        ));
    }
    validate_inputs(git, commit, &file.input).map_err(invalid)?;
    let dimensions = crate::completion::site_dimensions(file.dimension).map_err(invalid)?;
    let profiles = crate::completion::requirements(file.profile, &dimensions).map_err(invalid)?;
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
        dimensions,
        manifest_digest,
        agent: file.agent,
        signing: file.policy.signing,
        reuse_unsigned: file.policy.reuse_unsigned,
        limits: file.limits,
        talk: file.talk,
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
    agent_flag_at(git, commit, "require_actor", |agent| agent.require_actor)
}

/// Whether the policy at `commit` requires actor proof (`[agent] require_actor_proof`, G243b),
/// judged at the base as [`requires_actor_at`] is: a change never lifts it for its own landing.
///
/// # Errors
///
/// A git fault.
pub fn requires_actor_proof_at(git: &Git, commit: &str) -> Result<bool, String> {
    agent_flag_at(git, commit, "require_actor_proof", |agent| {
        agent.require_actor_proof
    })
}

/// The most live changes one actor may hold under the policy at `commit`
/// (`[agent] max_live_changes`, G264), judged at the base as [`requires_actor_at`] is. No policy, or no
/// key, sets no limit. An invalid policy still sets it when its text says so: a typo elsewhere
/// never lifts the limit.
///
/// # Errors
///
/// A git fault.
pub fn max_live_changes_at(git: &Git, commit: &str) -> Result<Option<u64>, String> {
    match load_at(git, commit) {
        Ok(policy) => Ok(policy.and_then(|policy| policy.agent.max_live_changes)),
        Err(LoadError::Git(why)) => Err(why),
        Err(LoadError::Invalid(_)) => Ok(git
            .file_at(commit, POLICY_PATH)
            .map_err(|error| error.message())?
            .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
            .and_then(|table| table.get("agent")?.get("max_live_changes")?.as_integer())
            .and_then(|limit| u64::try_from(limit).ok())),
    }
}

/// How long a question tool's hook waits for an answer under the policy at `commit`
/// (`[agent] question_wait_s`, dossier 63): 0, record only, with no policy, no key, or a
/// policy bay cannot read, so a broken policy never holds an agent.
///
/// # Errors
///
/// A git fault.
pub fn question_wait_at(git: &Git, commit: &str) -> Result<u64, String> {
    match load_at(git, commit) {
        Ok(policy) => Ok(policy
            .and_then(|policy| policy.agent.question_wait_s)
            .unwrap_or(0)
            .min(MAX_QUESTION_WAIT_S)),
        Err(LoadError::Git(why)) => Err(why),
        Err(LoadError::Invalid(_)) => Ok(0),
    }
}

/// The size limits of the policy at `commit` (`[limits]`, owner ruling 2026-10-21), judged at
/// the base like every other policy key. No policy, or no key, sets no limit. An invalid policy
/// sets none here: it lands nothing, and its refusal is `POLICY_INVALID`.
///
/// # Errors
///
/// A git fault.
pub fn limits_at(git: &Git, commit: &str) -> Result<crate::limits::Limits, String> {
    match load_at(git, commit) {
        Ok(Some(policy)) => Ok(policy.limits),
        Ok(None) | Err(LoadError::Invalid(_)) => Ok(crate::limits::Limits::default()),
        Err(LoadError::Git(why)) => Err(why),
    }
}

/// The `[assets]` table of the policy at `commit` (BACKLOG 49, 62). No policy, or no table,
/// keeps and holds nothing. An invalid policy still holds what its text names: a typo elsewhere
/// never lifts a hold.
///
/// # Errors
///
/// A git fault.
pub fn assets_at(git: &Git, commit: &str) -> Result<crate::assets::AssetsPolicy, String> {
    // Read from the text, valid or not: the table is validated with the policy (G104b) and kept
    // nowhere else.
    let Some(table) = git
        .file_at(commit, POLICY_PATH)
        .map_err(|error| error.message())?
        .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
        .and_then(|table| table.get("assets").cloned())
    else {
        return Ok(crate::assets::AssetsPolicy::default());
    };
    if let Ok(assets) = table.clone().try_into() {
        return Ok(assets);
    }
    // A bad line beside it never lifts a hold: its globs are read on their own.
    let hold = table
        .get("hold")
        .and_then(toml::Value::as_array)
        .map(|globs| {
            globs
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Ok(crate::assets::AssetsPolicy {
        hold,
        ..crate::assets::AssetsPolicy::default()
    })
}

/// The `[agent]` flag `key` of the policy at `commit`, read by `flag`. No policy sets none. An
/// invalid policy still sets it when its text says so: a typo elsewhere never lifts it.
fn agent_flag_at(
    git: &Git,
    commit: &str,
    key: &str,
    flag: fn(&AgentPolicy) -> bool,
) -> Result<bool, String> {
    match load_at(git, commit) {
        Ok(policy) => Ok(policy.is_some_and(|policy| flag(&policy.agent))),
        Err(LoadError::Git(why)) => Err(why),
        Err(LoadError::Invalid(_)) => Ok(git
            .file_at(commit, POLICY_PATH)
            .map_err(|error| error.message())?
            .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
            .and_then(|table| table.get("agent")?.get(key)?.as_bool())
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
    // Owner ruling 2026-10-21: each `[limits]` key is a positive integer.
    file.limits.validate()?;
    // BACKLOG 65 (owner ruling 2026-10-25): the class of a path has one home, `.gitattributes`
    // on main. No alias, so a policy that still says `[locks]` names where it went.
    if file.locks.is_some() {
        return Err(
            "[locks] was removed in 0.5.0: mark the paths `lockable` in .gitattributes on main (`art/** lockable`, Git LFS's attribute)"
                .to_owned(),
        );
    }
    file.assets.validate()?;
    // Owner ruling 2026-10-25 (BACKLOG 62): only a person lifts a hold. A change to the policy
    // is judged by the policy at its base, so with an approval check over the policy file, a
    // change that removes or narrows a hold merges only with an approver's signed yes.
    if !file.assets.hold.is_empty()
        && !file.check.iter().any(|entry| {
            entry.is_approval()
                && entry
                    .when_paths
                    .as_ref()
                    .is_some_and(|globs| globs_match(globs, POLICY_PATH))
        })
    {
        return Err(format!(
            "[assets] hold needs an approval [[check]] whose when_paths covers {POLICY_PATH}: only a person's signed yes lifts a hold"
        ));
    }
    // Dossier 63: a wait past the cap would hide the question from the person at the terminal.
    if let Some(wait) = file.agent.question_wait_s
        && wait > MAX_QUESTION_WAIT_S
    {
        return Err(format!(
            "[agent] question_wait_s = {wait}: a question tool waits at most {MAX_QUESTION_WAIT_S} seconds for an answer"
        ));
    }
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
        if let Some(since) = quarantine.since.as_deref().filter(|since| !is_date(since)) {
            return Err(format!(
                "[[quarantine]] ({}, {}) has since = {since:?}, which is not a date (YYYY-MM-DD)",
                quarantine.id, quarantine.environment
            ));
        }
        if quarantine
            .owner
            .as_deref()
            .is_some_and(|owner| owner.trim().is_empty())
        {
            return Err(format!(
                "[[quarantine]] ({}, {}) has an empty owner; name who answers for it, or leave owner out",
                quarantine.id, quarantine.environment
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
            "check {} is in both stages, pre and post; give the post-merge check its own id",
            entry.id
        ));
    }
    // A post-publication check never judges a landing, so it never counts here.
    // G245: an approval collects no test, so it never counts here either.
    if !file.unchecked
        && !file
            .check
            .iter()
            .any(|entry| entry.minimum() > 0 && !quarantined(entry) && entry.stage == Stage::Pre)
    {
        return Err("no required check has an expected_minimum above zero".to_owned());
    }
    Ok(())
}

/// The `inputs` values are `commit` and `history` (BACKLOG 28, `INTERFACES` § 3, § 7): any
/// other is an input bay cannot identify, and an unknown input is never a pass, so it makes the
/// policy invalid rather than keying the check's evidence by tree. Each `fixtures` entry is
/// `<name>@sha256:<64 hex>` (ruling R18), each name once: a fixture without a digest is an input
/// bay cannot identify.
fn validate_declared_inputs(entry: &CheckEntry) -> Result<(), String> {
    if let Some(other) = entry
        .inputs
        .iter()
        .find(|input| *input != INPUT_COMMIT && *input != INPUT_HISTORY)
    {
        return Err(format!(
            "check {} declares the input {other:?}; the inputs bay can identify are \
             \"{INPUT_COMMIT}\" and \"{INPUT_HISTORY}\"",
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
        // G500: a read set is matched as `when_paths` is, so a `!` here would drop from the
        // footprint stamp a path the check still sees.
        if path.starts_with('!') {
            return Err(format!(
                "check {} names the path {path:?}; paths takes no '!' exclusions (when_paths does)",
                entry.id
            ));
        }
        if !seen.insert(path.as_str()) {
            return Err(format!("check {} names the path {path} twice", entry.id));
        }
    }
    Ok(())
}

/// A `[[check]]`'s `footprint` (NEW-10; items 1 and 2): none, declared by its `paths` or
/// `when_paths` (`true`), or observed by bay as the check runs (`"observed"`). `true`
/// serializes as `true`, so a declared footprint keeps its definition digest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FootprintMode {
    #[default]
    Off,
    Declared,
    Observed,
}

impl FootprintMode {
    #[must_use]
    pub fn is_off(&self) -> bool {
        *self == Self::Off
    }
}

impl Serialize for FootprintMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Off => serializer.serialize_bool(false),
            Self::Declared => serializer.serialize_bool(true),
            Self::Observed => serializer.serialize_str("observed"),
        }
    }
}

impl<'de> Deserialize<'de> for FootprintMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Flag(bool),
            Word(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Flag(false) => Ok(Self::Off),
            Raw::Flag(true) => Ok(Self::Declared),
            Raw::Word(word) if word == "observed" => Ok(Self::Observed),
            Raw::Word(other) => Err(serde::de::Error::custom(format!(
                "footprint is true or \"observed\", not {other:?}"
            ))),
        }
    }
}

/// NEW-10: a check with `footprint = true` names what it reads, in `paths` or `when_paths`; a
/// footprint of the whole tree would carry nothing, and an empty one everything.
fn validate_footprint(entry: &CheckEntry) -> Result<(), String> {
    if entry.footprint == FootprintMode::Declared
        && entry.paths.is_none()
        && entry.when_paths.is_none()
    {
        return Err(format!(
            "check {} declares footprint = true with neither paths nor when_paths; a footprint names what the check reads",
            entry.id
        ));
    }
    // N-18: a footprint carries a record onto a candidate with another changed set, and a
    // changed-only check sees exactly its own changed set.
    if !entry.footprint.is_off() && entry.changed_only() {
        return Err(format!(
            "check {} declares a footprint and materialize = \"changed\"; a footprint carries a record onto another changed set, and this check sees only its own",
            entry.id
        ));
    }
    // An observed footprint holds only for a verdict that reads nothing but its tree.
    if entry.footprint == FootprintMode::Observed && entry.network {
        return Err(format!(
            "check {} declares footprint = \"observed\" and network = true; what it reads over the network is never observed, so its record could be carried onto a tree it never judged",
            entry.id
        ));
    }
    Ok(())
}

/// The `when_paths` rules of one `[[check]]` (G146): when present, at least one glob, each a
/// relative path pattern without `.`, `..` or empty parts (a trailing `/` names a directory),
/// once, after its `!` when it is an exclusion (G500). A list that could match nothing is a
/// typo, never silent, and so is one that starts with an exclusion: an exclusion drops only
/// what a glob before it takes, so a list of exclusions alone matches no path.
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
    if let Some(first) = globs.first().filter(|glob| glob.starts_with('!')) {
        return Err(format!(
            "check {} starts when_paths with the exclusion {first:?}; an exclusion drops only paths a glob before it includes, so name an include first",
            entry.id
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for glob in globs {
        let pattern = glob.strip_prefix('!').unwrap_or(glob);
        let name = pattern.strip_suffix('/').unwrap_or(pattern);
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

/// The `kind` rules of one `[[check]]` (G245). A run check states its `command`, its `adapter`
/// list and its `expected_minimum`. An approval check is answered by a person's signed record,
/// never run, so it states none of them, nor anything that shapes a run (`env`, `inputs`,
/// `fixtures`, `paths`, `materialize`, `network`); it judges a landing, so it is never `post`; and it names
/// the paths that need the approval in `when_paths`, which it requires: an approval on every
/// change is stated as `when_paths = ["**"]`, never by leaving the list out.
fn validate_kind(entry: &CheckEntry) -> Result<(), String> {
    if !entry.is_approval() {
        // G292: `carry` is an approval's; a run check carries its record only by `footprint`.
        if entry.carry.is_some() {
            return Err(format!(
                "check {} names carry, which only an approval check names; a check bay runs carries its record with footprint = true",
                entry.id
            ));
        }
        if entry.command.trim().is_empty() {
            return Err(format!(
                "check {} has no command; a check bay runs names its command (or is kind = \"approval\")",
                entry.id
            ));
        }
        if entry.expected_minimum.is_none() {
            return Err(format!(
                "check {} has no expected_minimum; a check bay runs states the minimum number of tests it must collect",
                entry.id
            ));
        }
        return Ok(());
    }
    // N-9: an approval is a person's answer; a waiver of it would stand for that answer.
    if entry.waivable {
        return Err(format!(
            "check {} is kind = \"approval\" and waivable = true; an approval is answered by an approver's yes, never waived",
            entry.id
        ));
    }
    let stated = [
        ("command", !entry.command.is_empty()),
        ("adapter", !entry.adapter.is_empty()),
        ("expected_minimum", entry.expected_minimum.is_some()),
        ("env", !entry.env.is_empty()),
        ("inputs", !entry.inputs.is_empty()),
        ("fixtures", !entry.fixtures.is_empty()),
        ("paths", entry.paths.is_some()),
        ("materialize", entry.materialize.is_some()),
        ("network", entry.network),
        ("retry_on_failure", entry.retry_on_failure > 0),
        ("allow_flaky", entry.allow_flaky),
        ("baseline", entry.baseline.is_some()),
    ];
    if let Some((key, _)) = stated.iter().find(|(_, set)| *set) {
        return Err(format!(
            "check {} is kind = \"approval\" and names {key}; an approval is answered by an approver's signed record, never run, so it names no {key}",
            entry.id
        ));
    }
    if entry.stage == Stage::Post {
        return Err(format!(
            "check {} is kind = \"approval\" and stage = \"post\"; an approval is given before a merge, never after it",
            entry.id
        ));
    }
    if entry.when_paths.is_none() {
        return Err(format!(
            "check {} is kind = \"approval\" and has no when_paths; name the paths that need an approval (when_paths = [\"**\"] for every change)",
            entry.id
        ));
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

/// The flake keys of one `[[check]]`: `retry_on_failure` at most [`MAX_RETRY_ON_FAILURE`], and
/// neither key on a post-publication check, which bay never runs or judges for a landing: a
/// key there would change nothing, and a typo is never silent.
fn validate_retry(entry: &CheckEntry) -> Result<(), String> {
    if entry.retry_on_failure > MAX_RETRY_ON_FAILURE {
        return Err(format!(
            "check {} asks retry_on_failure = {}; at most {MAX_RETRY_ON_FAILURE} reruns are allowed",
            entry.id, entry.retry_on_failure
        ));
    }
    if entry.stage == Stage::Post && (entry.retry_on_failure > 0 || entry.allow_flaky) {
        return Err(format!(
            "check {} runs after the merge; retry_on_failure and allow_flaky apply only to a pre-merge check",
            entry.id
        ));
    }
    // N-3: a post-publication check never judges a landing, so a baseline there would change
    // nothing, and a typo is never silent.
    if entry.stage == Stage::Post && entry.baseline.is_some() {
        return Err(format!(
            "check {} runs after the merge; baseline applies only to a pre-merge check",
            entry.id
        ));
    }
    Ok(())
}

/// Whether the `when_paths` list `globs` matches changed path `path` (G500), read in order as
/// `.gitignore` reads it: the last glob that matches the path decides, an include taking it
/// and a `!` exclusion dropping it, and a path no glob matches is not matched. Each glob is
/// matched by [`glob_matches`], an exclusion without its `!`.
#[must_use]
pub fn globs_match(globs: &[String], path: &str) -> bool {
    globs
        .iter()
        .fold(false, |matched, glob| match glob.strip_prefix('!') {
            Some(excluded) => matched && !glob_matches(excluded, path),
            None => matched || glob_matches(glob, path),
        })
}

/// Whether one glob of a `when_paths` list matches changed path `path`: `*` matches within
/// one path part, `?` one character other than `/`, a part `**` any number of parts, and a
/// glob ending in `/` every path under that directory.
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

    use super::{
        CheckEntry, PolicyFile, Quarantine, Signing, covers, manifest_digest, manifest_entry,
        signing_in,
    };

    /// N-21: `[policy] signing` is `all` by default or `approvals`; any other value, or another
    /// key in `[policy]`, does not parse (`POLICY_INVALID`). The mode read from the text is
    /// `approvals` only for that exact value, so unknown never weakens signing.
    #[test]
    fn signing_is_all_or_approvals_and_unknown_is_all() {
        let file = |extra: &str| {
            toml::from_str::<PolicyFile>(&format!("revision = 1\nunchecked = true\n{extra}"))
        };
        assert_eq!(file("").unwrap().policy.signing, Signing::All);
        assert_eq!(
            file("[policy]\nsigning = \"all\"\n")
                .unwrap()
                .policy
                .signing,
            Signing::All
        );
        assert_eq!(
            file("[policy]\nsigning = \"approvals\"\n")
                .unwrap()
                .policy
                .signing,
            Signing::Approvals
        );
        for bad in [
            "[policy]\nsigning = \"none\"\n",
            "[policy]\nsigning = \"Approvals\"\n",
            "[policy]\nsigning = true\n",
            "[policy]\nsign = \"approvals\"\n",
        ] {
            assert!(file(bad).is_err(), "{bad}");
        }
        assert_eq!(
            signing_in("revision = 1\n[policy]\nsigning = \"approvals\"\n"),
            Signing::Approvals
        );
        for text in [
            "revision = 1\n",
            "revision = 1\n[policy]\nsigning = \"none\"\n",
            "revision = 1\nsigning = \"approvals\"\n",
            "[policy\nsigning = \"approvals\"\n",
        ] {
            assert_eq!(signing_in(text), Signing::All, "{text}");
        }
    }

    /// G318: `[policy] reuse_unsigned` is off by default and a boolean; any other value does
    /// not parse (`POLICY_INVALID`), so a typo never turns reuse on.
    #[test]
    fn reuse_unsigned_is_off_by_default_and_a_boolean() {
        let file = |extra: &str| {
            toml::from_str::<PolicyFile>(&format!("revision = 1\nunchecked = true\n{extra}"))
        };
        assert!(!file("").unwrap().policy.reuse_unsigned);
        assert!(
            !file("[policy]\nsigning = \"approvals\"\n")
                .unwrap()
                .policy
                .reuse_unsigned
        );
        assert!(
            file("[policy]\nreuse_unsigned = true\n")
                .unwrap()
                .policy
                .reuse_unsigned
        );
        for bad in [
            "[policy]\nreuse_unsigned = \"yes\"\n",
            "[policy]\nreuse_unsigned = 1\n",
        ] {
            assert!(file(bad).is_err(), "{bad}");
        }
    }

    /// A quarantine's age counts whole days from `since`; without `since` it has none.
    #[test]
    fn a_quarantine_counts_its_age_from_since() {
        let mut quarantine: Quarantine = toml::from_str(
            "id = \"lint\"\nenvironment = \"local\"\nreason = \"red\"\nend = \"2026-03-01\"\nowner = \"platform\"\nsince = \"2026-02-27\"\n",
        )
        .unwrap();
        assert_eq!(quarantine.age_days("2026-03-02"), Some(3));
        assert!(quarantine.overdue("2026-03-02"));
        assert!(!quarantine.overdue("2026-03-01"));
        quarantine.since = None;
        assert_eq!(quarantine.age_days("2026-03-02"), None);
    }

    /// The flake keys stay out of the canonical entry at their defaults, so a policy without
    /// them keeps its definition digest (the G26 vector), and enter it when set.
    #[test]
    fn the_flake_keys_enter_the_digest_only_when_set() {
        let text = "id = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n";
        let plain: CheckEntry = toml::from_str(text).unwrap();
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json.get("retry_on_failure").is_none(), "{json}");
        assert!(json.get("allow_flaky").is_none(), "{json}");
        let set: CheckEntry =
            toml::from_str(&format!("{text}retry_on_failure = 2\nallow_flaky = true\n")).unwrap();
        let ids = BTreeMap::new();
        assert_ne!(
            manifest_entry(&plain, &ids).unwrap().definition_digest,
            manifest_entry(&set, &ids).unwrap().definition_digest
        );
    }

    /// N-9: `waivable = true` enters the definition digest; absent, it is left out of the
    /// canonical entry, so a policy without it keeps its digest (the G26 vector). On an approval
    /// check it is invalid: an approval is a person's answer, never waived.
    #[test]
    fn waivable_enters_the_digest_only_when_set_and_never_on_an_approval() {
        let text = "id = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n";
        let plain: CheckEntry = toml::from_str(text).unwrap();
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json.get("waivable").is_none(), "{json}");
        let set: CheckEntry = toml::from_str(&format!("{text}waivable = true\n")).unwrap();
        assert_eq!(serde_json::to_value(&set).unwrap()["waivable"], true);
        let ids = BTreeMap::new();
        assert_ne!(
            manifest_entry(&plain, &ids).unwrap().definition_digest,
            manifest_entry(&set, &ids).unwrap().definition_digest
        );
        assert!(super::validate_kind(&set).is_ok());
        let approval: CheckEntry = toml::from_str(
            "id = \"a\"\nenvironment = \"people\"\nkind = \"approval\"\nwhen_paths = [\"**\"]\nwaivable = true\n",
        )
        .unwrap();
        assert!(super::validate_kind(&approval).is_err());
    }

    /// N-3: `baseline = "base"` enters the definition digest; absent, it is left out of the
    /// canonical entry, so a policy without it keeps its digest (the G26 vector). Its one value
    /// is `"base"`; on an approval or a post-publication check it is invalid.
    #[test]
    fn a_baseline_enters_the_digest_only_when_set() {
        let text = "id = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n";
        let plain: CheckEntry = toml::from_str(text).unwrap();
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json.get("baseline").is_none(), "{json}");
        let set: CheckEntry = toml::from_str(&format!("{text}baseline = \"base\"\n")).unwrap();
        assert_eq!(serde_json::to_value(&set).unwrap()["baseline"], "base");
        let ids = BTreeMap::new();
        assert_ne!(
            manifest_entry(&plain, &ids).unwrap().definition_digest,
            manifest_entry(&set, &ids).unwrap().definition_digest
        );
        assert!(toml::from_str::<CheckEntry>(&format!("{text}baseline = \"trunk\"\n")).is_err());
        let post: CheckEntry =
            toml::from_str(&format!("{text}baseline = \"base\"\nstage = \"post\"\n")).unwrap();
        assert!(super::validate_retry(&post).is_err());
        let approval: CheckEntry = toml::from_str(
            "id = \"a\"\nenvironment = \"people\"\nkind = \"approval\"\nwhen_paths = [\"**\"]\nbaseline = \"base\"\n",
        )
        .unwrap();
        assert!(super::validate_kind(&approval).is_err());
    }

    /// N-18: `materialize = "changed"` enters the definition digest; absent, it is left out of
    /// the canonical entry (the G26 vector). It keys the check's evidence by commit, keeps the
    /// changed paths and the entry's `paths`, and is invalid with `footprint = true` or on an
    /// approval. Its one value is `"changed"`.
    #[test]
    fn a_changed_only_check_enters_the_digest_and_keeps_its_changed_paths() {
        let text = "id = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n";
        let entry =
            |extra: &str| -> CheckEntry { toml::from_str(&format!("{text}{extra}")).unwrap() };
        let plain = entry("");
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json.get("materialize").is_none(), "{json}");
        let set = entry("materialize = \"changed\"\n");
        assert_eq!(
            serde_json::to_value(&set).unwrap()["materialize"],
            "changed"
        );
        let ids = BTreeMap::new();
        assert_ne!(
            manifest_entry(&plain, &ids).unwrap().definition_digest,
            manifest_entry(&set, &ids).unwrap().definition_digest
        );
        assert!(toml::from_str::<CheckEntry>(&format!("{text}materialize = \"all\"\n")).is_err());
        assert!(!plain.commit_keyed() && set.commit_keyed());
        let changed = ["src/b.rs", "src/a.rs"].map(str::to_owned);
        assert_eq!(plain.materialized(&changed), None);
        assert_eq!(
            set.materialized(&changed),
            Some(["src/a.rs", "src/b.rs"].map(str::to_owned).to_vec())
        );
        let extras = entry("materialize = \"changed\"\npaths = [\".bay/\", \"src/a.rs\"]\n");
        assert_eq!(
            extras.materialized(&changed),
            Some(
                [".bay/", "src/a.rs", "src/b.rs"]
                    .map(str::to_owned)
                    .to_vec()
            )
        );
        assert_eq!(
            extras.materialized(&[]),
            Some([".bay/", "src/a.rs"].map(str::to_owned).to_vec())
        );
        assert_eq!(set.materialized(&[]), Some(Vec::new()));
        assert!(
            super::validate_footprint(&entry(
                "materialize = \"changed\"\nwhen_paths = [\"src/**\"]\n"
            ))
            .is_ok()
        );
        assert!(
            super::validate_footprint(&entry(
                "materialize = \"changed\"\nwhen_paths = [\"src/**\"]\nfootprint = true\n"
            ))
            .is_err()
        );
        let approval: CheckEntry = toml::from_str(
            "id = \"a\"\nenvironment = \"people\"\nkind = \"approval\"\nwhen_paths = [\"**\"]\nmaterialize = \"changed\"\n",
        )
        .unwrap();
        assert!(super::validate_kind(&approval).is_err());
    }

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

    /// G245: an approval check is answered by records, so it names nothing a run uses, is
    /// never `post`, and names its paths; a run check states its command and minimum. `kind =
    /// "approval"` is in the canonical entry, and what an approval lacks is left out of it.
    #[test]
    fn an_approval_check_names_its_paths_and_nothing_a_run_uses() {
        let approval = "id = \"billing-approval\"\nenvironment = \"people\"\nkind = \"approval\"\nwhen_paths = [\"billing/\"]\n";
        let entry =
            |extra: &str| -> CheckEntry { toml::from_str(&format!("{approval}{extra}")).unwrap() };
        assert!(super::validate_kind(&entry("")).is_ok());
        for bad in [
            "command = \"scripts/bay-check\"\n",
            "adapter = [\"scripts/bay-check\"]\n",
            "expected_minimum = 0\n",
            "stage = \"post\"\n",
            "env = [\"TOKEN\"]\n",
            "inputs = [\"commit\"]\n",
            "paths = [\"billing/\"]\n",
            "network = true\n",
        ] {
            assert!(super::validate_kind(&entry(bad)).is_err(), "{bad}");
        }
        let unscoped: CheckEntry =
            toml::from_str("id = \"a\"\nenvironment = \"people\"\nkind = \"approval\"\n").unwrap();
        assert!(super::validate_kind(&unscoped).is_err());
        for run in [
            "id = \"tests\"\nenvironment = \"local\"\nadapter = [\"x\"]\nexpected_minimum = 1\n",
            "id = \"tests\"\nenvironment = \"local\"\ncommand = \"x\"\nadapter = [\"x\"]\n",
        ] {
            let entry: CheckEntry = toml::from_str(run).unwrap();
            assert!(super::validate_kind(&entry).is_err(), "{run}");
        }
        let canonical = serde_json::to_value(entry("")).unwrap();
        assert_eq!(canonical["kind"], "approval");
        for absent in ["command", "adapter", "expected_minimum"] {
            assert!(canonical.get(absent).is_none(), "{absent}");
        }
        let ids = BTreeMap::new();
        let unchanged: CheckEntry = toml::from_str(
            "id = \"tests\"\nenvironment = \"local\"\ncommand = \"x\"\nadapter = [\"x\"]\nexpected_minimum = 1\n",
        )
        .unwrap();
        let stated: CheckEntry = toml::from_str(
            "id = \"tests\"\nenvironment = \"local\"\nkind = \"check\"\ncommand = \"x\"\nadapter = [\"x\"]\nexpected_minimum = 1\n",
        )
        .unwrap();
        assert_eq!(
            manifest_entry(&unchanged, &ids).unwrap().definition_digest,
            manifest_entry(&stated, &ids).unwrap().definition_digest,
            "the default kind leaves the digest as it was"
        );
    }

    /// NEW-10: `footprint = true` is a field of the entry, in its definition digest, so no
    /// record made before it was declared is carried; the default leaves the digest as it was
    /// (the G26 vector). Its read set is `paths` when present, else `when_paths`, and a check
    /// without `footprint = true` has none.
    #[test]
    fn a_footprint_enters_the_definition_digest_and_names_its_read_set() {
        let text = "id = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\nwhen_paths = [\"api/\"]\n";
        let ids = BTreeMap::new();
        let entry =
            |extra: &str| -> CheckEntry { toml::from_str(&format!("{text}{extra}")).unwrap() };
        let digest_of = |extra: &str| {
            manifest_entry(&entry(extra), &ids)
                .unwrap()
                .definition_digest
        };
        assert_eq!(digest_of(""), digest_of("footprint = false\n"));
        assert_ne!(digest_of(""), digest_of("footprint = true\n"));
        assert_eq!(entry("").read_set(), None);
        let when = ["api/".to_owned()];
        assert_eq!(entry("footprint = true\n").read_set(), Some(&when[..]));
        let paths = ["api/".to_owned(), "lib/".to_owned()];
        assert_eq!(
            entry("footprint = true\npaths = [\"api/\", \"lib/\"]\n").read_set(),
            Some(&paths[..])
        );
        let mut bare = entry("footprint = true\n");
        bare.when_paths = None;
        assert!(super::validate_footprint(&bare).is_err());
        assert!(super::validate_footprint(&entry("footprint = true\n")).is_ok());
    }

    /// G292: `carry` enters the definition digest only when set; absent, an approval check
    /// carries and reads its `when_paths`; `carry = false` reads nothing, and a run check that
    /// names `carry` is invalid.
    #[test]
    fn a_carry_enters_the_digest_only_when_set() {
        let approval = "id = \"billing-approval\"\nenvironment = \"people\"\nkind = \"approval\"\nwhen_paths = [\"billing/\"]\n";
        let ids = BTreeMap::new();
        let entry =
            |extra: &str| -> CheckEntry { toml::from_str(&format!("{approval}{extra}")).unwrap() };
        let digest_of = |extra: &str| {
            manifest_entry(&entry(extra), &ids)
                .unwrap()
                .definition_digest
        };
        assert!(
            serde_json::to_value(entry(""))
                .unwrap()
                .get("carry")
                .is_none()
        );
        assert_ne!(digest_of(""), digest_of("carry = false\n"));
        assert_ne!(digest_of(""), digest_of("carry = true\n"));
        let when = ["billing/".to_owned()];
        assert!(entry("").carries());
        assert_eq!(entry("").read_set(), Some(&when[..]));
        assert!(!entry("carry = false\n").carries());
        assert_eq!(entry("carry = false\n").read_set(), None);
        assert!(super::validate_kind(&entry("carry = false\n")).is_ok());
        let run: CheckEntry = toml::from_str(
            "id = \"tests\"\nenvironment = \"local\"\ncommand = \"x\"\nadapter = [\"x\"]\nexpected_minimum = 1\ncarry = true\n",
        )
        .unwrap();
        assert!(super::validate_kind(&run).is_err());
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

    /// G500: a `when_paths` list is read in order, the last glob that matches a path deciding:
    /// an exclusion drops what the globs before it take, and a later include takes it back.
    #[test]
    fn when_paths_exclusions_are_read_in_order() {
        use super::globs_match;
        let list =
            |globs: &[&str]| -> Vec<String> { globs.iter().map(|g| (*g).to_owned()).collect() };
        let agents = list(&["**/AGENTS.md", "!notes/**"]);
        assert!(globs_match(&agents, "AGENTS.md"));
        assert!(globs_match(&agents, "apps/AGENTS.md"));
        assert!(globs_match(&agents, "new/deep/AGENTS.md"));
        assert!(!globs_match(&agents, "notes/AGENTS.md"));
        assert!(!globs_match(&agents, "notes/team/AGENTS.md"));
        assert!(!globs_match(&agents, "apps/README.md"));
        let back = list(&["**/AGENTS.md", "!notes/**", "notes/AGENTS.md"]);
        assert!(globs_match(&back, "notes/AGENTS.md"));
        assert!(!globs_match(&back, "notes/team/AGENTS.md"));
        // An exclusion acts only on what a glob before it took.
        let late = list(&["src/", "!src/gen/", "src/gen/keep.rs", "!src/gen/keep.rs"]);
        assert!(!globs_match(&late, "src/gen/keep.rs"));
        assert!(globs_match(&late, "src/a.rs"));
        assert!(!globs_match(&list(&["src/", "!docs/"]), "docs/a.md"));
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

    /// BACKLOG 28: `commit` and `history` are the inputs bay identifies, and both key evidence
    /// by subject commit; R18: a fixture names its digest.
    #[test]
    fn an_input_or_fixture_bay_cannot_identify_is_invalid() {
        let text = "id = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n";
        let parse =
            |extra: &str| -> CheckEntry { toml::from_str(&format!("{text}{extra}")).unwrap() };
        let check = |extra: &str| super::validate_declared_inputs(&parse(extra));
        let hex = "0123456789abcdef".repeat(4);
        assert!(check("inputs = [\"commit\"]\n").is_ok());
        assert!(check("inputs = [\"history\"]\n").is_ok());
        assert!(check("inputs = [\"commit\", \"history\"]\n").is_ok());
        let history = parse("inputs = [\"history\"]\n");
        assert!(history.commit_keyed() && history.reads_history());
        let commit = parse("inputs = [\"commit\"]\n");
        assert!(commit.commit_keyed() && !commit.reads_history());
        assert!(!parse("").commit_keyed());
        assert!(
            check(&format!(
                "fixtures = [\"data@sha256:{hex}\", \"more@sha256:{hex}\"]\n"
            ))
            .is_ok()
        );
        for bad in [
            "inputs = [\"commits\"]\n".to_owned(),
            "inputs = [\"commit\", \"network\"]\n".to_owned(),
            "inputs = [\"histories\"]\n".to_owned(),
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

    /// A changed path is read as Git stores it, never quoted: a name with a space and a
    /// non-ASCII character comes back whole, from a root commit and from a child alike.
    #[test]
    fn a_changed_path_with_a_space_and_a_non_ascii_character_round_trips() {
        let dir = tempfile::TempDir::new().unwrap();
        let git = crate::git::Git::new(dir.path());
        git.run(&["init", "-q"]).unwrap();
        let commit = |path: &str| {
            let file = dir.path().join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, "x\n").unwrap();
            git.run(&["add", "--", path]).unwrap();
            git.run(&[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "commit",
                "-q",
                "-m",
                path,
            ])
            .unwrap();
            git.run(&["rev-parse", "HEAD"]).unwrap()
        };
        let root_path = "r\u{e9}sum\u{e9} one.txt";
        let child_path = "d\u{f6}cs/a b.txt";
        let root = commit(root_path);
        let child = commit(child_path);
        assert_eq!(super::changed_paths(&git, &root).unwrap(), [root_path]);
        assert_eq!(super::changed_paths(&git, &child).unwrap(), [child_path]);
    }
}
