//! `bay.result/1` — the one result every command returns.
//!
//! Stdout is always this value as JSON. Human text is a rendering of it on stderr.
//! Exit codes follow `INTERFACES` § 1. Reason codes are the fixed table of
//! `INTERFACES` § 2 and § 8: every code carries `allowed` and `retry_when`, and every refusal
//! carries the agent's next step (`next_step`, [`Reason::next`]). This file is the one home of
//! what to do on each reason: the skill and the docs point here, through `bay help --json`.

use serde::{Serialize, Serializer};
use serde_json::Value;

pub const SCHEMA: &str = "bay.result/1";

/// How long one `git` invocation may run.
pub const GIT_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long one `git` call that talks to a remote may run (`push`, `fetch`, `ls-remote`,
/// `clone`, `pull`): a remote that advertises tens of thousands of refs takes longer than any
/// local call to answer a push, and a bound meant for local work refused it (a fleet's
/// `bay start` on a forge, 2026-10-25).
pub const GIT_NETWORK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Ok,
    /// 0.3.0: the word is `merged` (`landed` before).
    #[serde(rename = "merged")]
    Landed,
    Queued,
    Blocked,
    Failed,
}

impl Decision {
    /// Every decision, in the order of `INTERFACES` § 1.
    pub const ALL: &'static [Self] = &[
        Self::Ok,
        Self::Landed,
        Self::Queued,
        Self::Blocked,
        Self::Failed,
    ];

    /// Process exit code for this decision. Usage errors are clap's exit 2.
    #[must_use]
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Ok | Self::Landed | Self::Queued => 0,
            Self::Blocked => 10,
            Self::Failed => 20,
        }
    }

    /// The decision as `bay.result/1` spells it.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Landed => "merged",
            Self::Queued => "queued",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
        }
    }
}

/// One row of the reason-code table.
struct Row {
    code: &'static str,
    allowed: &'static [&'static str],
    retry_when: &'static str,
    transient: bool,
}

macro_rules! reasons {
    ($($variant:ident => $code:literal, [$($allowed:literal),*], $retry:literal, $transient:literal;)*) => {
        /// A reason code. The set is fixed; it is not a policy language.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Reason { $($variant),* }

        impl Reason {
            pub const ALL: &'static [Self] = &[$(Self::$variant),*];

            const fn row(self) -> Row {
                match self {
                    $(Self::$variant => Row {
                        code: $code,
                        allowed: &[$($allowed),*],
                        retry_when: $retry,
                        transient: $transient,
                    }),*
                }
            }
        }
    };
}

reasons! {
    // INTERFACES § 2
    InputChanged => "INPUT_CHANGED", ["read", "refresh", "repair"], "after_fix", false;
    InputUnobserved => "INPUT_UNOBSERVED", ["read", "refresh"], "after_fix", false;
    MergeConflict => "MERGE_CONFLICT", ["read", "refresh", "repair"], "after_fix", false;
    TrunkMoved => "MAIN_MOVED", [], "by_itself", true;
    PublishRetryExhausted => "MERGE_RETRY_EXHAUSTED", ["read", "repair"], "after_fix", false;
    RegistryChanged => "REGISTRY_CHANGED", [], "by_itself", false;
    // 0.3.3 (the Mac's 0.3.2 finding): `refs/bay/registry` breaks a rule a version is loaded by,
    // so nothing signed can be judged; `bay doctor` names it and the line.
    RegistryUnreadable => "REGISTRY_UNREADABLE", ["read"], "after_setup", false;
    CheckFlaky => "CHECK_FLAKY", ["repair"], "after_fix", false;
    ChangeNotLanded => "CHANGE_NOT_MERGED", ["read"], "after_wait", false;
    HookTimeout => "HOOK_TIMEOUT", [], "by_itself", true;
    // 0.3.0: `QUEUE_HELD` and `PUBLICATION_QUEUED` are one code; `waiting_for` says `queue`.
    Queued => "QUEUED", ["wait"], "by_itself", false;
    QueueDeadlineExceeded => "QUEUE_DEADLINE_EXCEEDED", ["read", "repair"], "after_fix", false;
    CheckFailed => "CHECK_FAILED", ["repair"], "after_fix", false;
    CheckUnavailable => "CHECK_UNAVAILABLE", ["wait"], "after_setup", true;
    CheckUnknown => "CHECK_UNKNOWN", ["wait", "repair"], "after_wait", false;
    PolicyChanged => "POLICY_CHANGED", ["refresh"], "after_fix", false;
    PolicyInvalid => "POLICY_INVALID", ["repair"], "after_new_main", false;
    // The missing-policy ruling; not yet in INTERFACES § 2: no `.bay/policy.toml` at the base.
    PolicyMissing => "POLICY_MISSING", ["read"], "after_setup", false;
    ViewIncomplete => "VIEW_INCOMPLETE", ["refresh"], "after_fix", false;
    PublishOutcomeUnknown => "MERGE_OUTCOME_UNKNOWN", [], "by_itself", false;
    // G244: a post-publication check failed on a landing that touches the same paths.
    TrunkRed => "MAIN_RED", ["read", "repair"], "after_new_main", false;
    ChangeAlreadyLanded => "CHANGE_ALREADY_MERGED", ["read"], "never", false;
    ChangeSuperseded => "CHANGE_SUPERSEDED", ["read"], "after_wait", false;
    // Ruled 2026-10-19 (rulings-open-questions.md Q8): another live session of the same actor
    // name holds the change.
    ChangeHeld => "CHANGE_HELD", ["read", "wait"], "after_wait", false;
    AbandonRefused => "ABANDON_REFUSED", ["read"], "after_wait", false;
    // G152; not yet in INTERFACES § 2: the policy at the base sets `[agent] require_actor` and
    // the actor's name came from a default, not `BAY_ACTOR`.
    ActorUnnamed => "ACTOR_UNNAMED", ["read"], "after_setup", false;
    // G243b; not yet in INTERFACES § 2: the policy at the base sets `[agent]
    // require_actor_proof` and the agent key cannot prove which actor signs: several actors run
    // sessions under this OS account, which can read a file-based agent key.
    ActorUnproven => "ACTOR_UNPROVEN", ["read"], "after_setup", false;
    CoverageUnknown => "COVERAGE_UNKNOWN", ["wait"], "after_setup", false;
    // Ruling R-H1 (AGENT-HOOKS-0.1 § 6.3, `context_invalid`); not yet in INTERFACES § 2: a
    // `tool.before` payload bay cannot read is denied.
    HookInputInvalid => "HOOK_INPUT_INVALID", ["read"], "after_setup", false;
    // Ruling R-H2 widened (H3, G60); not yet in INTERFACES § 2: a shell command on the deny list
    // (G39), as its `tool.before` row's verdict reason. The same command is never allowed.
    ShellDenied => "SHELL_DENIED", ["read"], "never", false;
    // `DESIGN` § 11c, "Two sessions in one tree" (G101); not yet in INTERFACES § 2: a write of a
    // path another live session of the same tree holds, as its `tool.before` deny.
    PathHeld => "PATH_HELD", ["read", "wait"], "after_wait", false;
    // BACKLOG 57: a change to a lockable path whose lock the change never held, refused at
    // candidate build (`crate::locks::not_held`).
    LockNotHeld => "LOCK_NOT_HELD", ["read", "repair"], "after_fix", false;
    // BACKLOG 49: a change's `.bay-assets` names bytes the assets plugin does not hold
    // (`crate::assets::not_held_at_merge`), refused at candidate build.
    AssetMissing => "ASSET_MISSING", ["read", "repair"], "after_fix", false;
    // BACKLOG 64 F1: the candidate puts a file back as it was before another change main
    // merged after this change began, with no `Reverts:` naming it (`crate::publish`).
    UndoesMerged => "UNDOES_MERGED", ["read", "refresh", "repair"], "after_fix", false;
    RemoteNotFetched => "REMOTE_NOT_FETCHED", ["read"], "after_setup", false;
    // Ruling: config location (a repository with no `.bay/config.toml` anywhere bay looks);
    // not yet in INTERFACES § 2
    ConfigMissing => "CONFIG_MISSING", ["read"], "after_setup", false;
    // INTERFACES § 8 (doctor)
    // 0.3.0: one code for every tool below its floor (git, bay, an agent product) and one for
    // an absent agent product; `detail.tool` names the tool (`git`, `bay`, `claude`, `codex`).
    ToolBelowFloor => "TOOL_BELOW_FLOOR", ["read"], "after_setup", false;
    ToolAbsent => "TOOL_ABSENT", ["read"], "after_setup", false;
    RemoteUnreachable => "REMOTE_UNREACHABLE", ["read"], "after_setup", false;
    HooksMissing => "HOOKS_MISSING", ["read"], "after_setup", false;
    SkillMismatch => "SKILL_MISMATCH", ["read"], "after_setup", false;
    SandboxAbsent => "SANDBOX_ABSENT", ["read"], "after_setup", false;
    // `bay doctor --role runner` (G63); not yet in INTERFACES § 8
    CheckEnvUnset => "CHECK_ENV_UNSET", ["read"], "after_setup", false;
    ExecutorKeyMissing => "RUNNER_KEY_MISSING", ["read"], "after_setup", false;
    // G145; not yet in INTERFACES § 8: a file-based release, producer, executor or registry key,
    // or a token beside it, that the account running agent sessions can read.
    KeyExposed => "KEY_EXPOSED", ["read"], "after_setup", false;
    // INTERFACES § 2 (`bay update`, the binary's lifecycle in `DESIGN`)
    UpdateUnavailable => "UPDATE_UNAVAILABLE", ["read"], "after_setup", false;
    UpdateUnverified => "UPDATE_UNVERIFIED", ["read"], "after_wait", false;
    UpdatePinned => "UPDATE_PINNED", ["read"], "after_setup", false;
    // `DESIGN` § 11c and rulings R9, T1, T7 and T10 (`bay talk`); not yet in INTERFACES § 2
    // 0.3.0: one code for a talk bay will not send; `detail.why` says why (it was invalid, a
    // different body under one id, closed, a supersede only its sender or `decides` may make, a
    // second answer, a sender who is no actor, or too many open talks).
    TalkRefused => "TALK_REFUSED", ["read", "repair"], "after_fix", false;
    // G77 (the review dossier's K6); not yet in INTERFACES § 2: `bay talk --to-change` names a
    // trunk commit that is no executor-signed bay publication, so no lander can be read from it.
    UnattributedLanding => "OUTSIDE_MERGE", ["read"], "never", false;
    // G159 (owner ruling Q1); not yet in INTERFACES § 2: the host hook refuses a push of an
    // evidence notes ref whose note drops or changes a record the old note held.
    EvidenceWithdrawn => "EVIDENCE_WITHDRAWN", ["read"], "never", false;
    // NEW-5, NEW-9 (owner ruling 2026-10-20); not yet in INTERFACES § 2: the host hook refuses a
    // push that moves a promotion target to a commit that is not promotable (off its source's
    // first-parent line, not a fast-forward, a required check not passing, inside its soak, or
    // frozen without Fixes:/Reverts:). `bay status` names the promotable commit.
    PromotionRefused => "PROMOTION_REFUSED", ["read", "wait", "repair"], "after_wait", false;
    // NEW-4; not yet in INTERFACES § 2: a change of no lane whose view reaches commits of a lane
    // ref that are not on trunk.
    LaneLeak => "LANE_LEAK", ["read", "repair"], "after_fix", false;
    // G245 (owner ruling 2026-10-20); not yet in INTERFACES § 2: every other required check
    // passed, and an approval check the candidate's paths wake has no pass yet from an enrolled
    // approver's key for this exact tree.
    ApprovalPending => "APPROVAL_PENDING", ["wait"], "after_person", false;
    // G264 (N-11); not yet in INTERFACES § 2: the policy at the base sets `[agent]
    // max_live_changes` and the actor already holds that many live changes; or (G310) it sets
    // `[limits] max_waiting_approvals` and that many live changes already wait for a yes.
    WipLimit => "WIP_LIMIT", ["read"], "after_wait", false;
    // G265 (N-13); not yet in INTERFACES § 2: the box's `bay.minFreeMiB` floor is above the free
    // space of the filesystem a view or a materialized tree would be written to.
    DiskLow => "DISK_LOW", ["read"], "after_setup", false;
    // N-8; not yet in INTERFACES § 2: trunk is held, by a `[[hold]]` of the trunk or the
    // hold adapter (`crate::hold`), and the change carries neither `Fixes:` nor `Reverts:`. The
    // operation stays queued, publishes nothing, and is tried again once the hold clears.
    TrunkHeld => "MAIN_HELD", ["wait"], "after_person", false;
    // N-5; not yet in INTERFACES § 2: `bay start --from <branch>` names a branch that changes
    // paths `--paths` does not declare. Nothing is created; a start whose paths cover them can.
    PathsUndeclared => "PATHS_UNDECLARED", ["read", "repair"], "after_fix", false;
    // 0.5.4 (the 0.5.3 acceptance finding): `bay start --from <branch>` names a branch whose copy
    // here holds commits the remote's lacks (a local rebase). Nothing is created; neither is
    // taken silently.
    BranchDiverged => "BRANCH_DIVERGED", ["read", "repair"], "after_fix", false;
    // G273 (N-16); not yet in INTERFACES § 2: the change was started `--after` a change that has
    // not landed on trunk yet. The operation stays queued, publishes nothing, holds no queue
    // place, and is tried again once every change it names has landed.
    DependencyPending => "DEPENDENCY_PENDING", ["wait"], "after_wait", false;
    // G273 (N-16); not yet in INTERFACES § 2: a change this one was started `--after` ended
    // without landing (abandoned, superseded or no effect), so this one can never publish.
    DependencyAbandoned => "DEPENDENCY_ABANDONED", ["read", "repair"], "after_fix", false;
    // Owner ruling 2026-10-21; not yet in INTERFACES § 2: the change is over a `[limits]` size
    // limit of the policy at the base (`crate::limits`), refused at submit and at candidate
    // build, before any check runs.
    ChangeTooLarge => "CHANGE_TOO_LARGE", ["read", "repair"], "after_fix", false;
    // G520 (B4, a fleet finding 2026-10-28); not yet in INTERFACES § 2: a write in a view whose
    // journal records another actor than the writer (`crate::view::not_yours`): `bay merge`,
    // `bay save`, `bay start <change>`, and the agent hook's edit or `git commit`.
    ViewNotYours => "VIEW_NOT_YOURS", ["read"], "never", false;
}

impl Reason {
    #[must_use]
    pub const fn code(self) -> &'static str {
        self.row().code
    }

    /// What is still allowed. Empty means "none needed": bay acts on its own. Every word is one
    /// of four (G293): `read`, `refresh`, `repair` and `wait`.
    #[must_use]
    pub const fn allowed(self) -> &'static [&'static str] {
        self.row().allowed
    }

    #[must_use]
    pub const fn retry_when(self) -> &'static str {
        self.row().retry_when
    }

    /// The reason with this code.
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|reason| reason.code() == code)
    }

    /// A transient code allows bounded retries; any other code refuses an unchanged retry.
    #[must_use]
    pub const fn transient(self) -> bool {
        self.row().transient
    }
}

impl Serialize for Reason {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.code())
    }
}

/// Which trunk the result was computed from, and when it was fetched.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Freshness {
    /// Printed as `main` (0.3.5).
    #[serde(rename = "main")]
    pub trunk: String,
    pub fetched_at: String,
}

/// Fields appear in the order of `INTERFACES` § 1. Absent fields are omitted. The JSON also
/// carries `say` after `reason` (owner ruling 2026-10-21): one plain sentence, made from the
/// decision, the reason and the detail by [`BayResult::say`], never stored, so no call site
/// writes one.
#[derive(Debug)]
pub struct BayResult {
    pub schema: &'static str,
    pub op_id: Option<String>,
    pub candidate: Option<String>,
    pub decision: Decision,
    pub state: Option<String>,
    pub reason: Option<Reason>,
    pub change: Option<String>,
    pub detail: Option<Value>,
    pub allowed: Option<Vec<&'static str>>,
    pub retry_when: Option<&'static str>,
    pub waiting_for: Option<String>,
    /// Seconds since the operation's request, for an operation still under way (G80).
    pub elapsed_s: Option<u64>,
    /// The evidence records the operation has so far (G80).
    pub evidence: Option<Vec<String>>,
    pub receipt: Option<String>,
    pub freshness: Option<Freshness>,
    /// A command's own human rendering, when it has one (doctor's status line).
    pub human: Option<String>,
}

/// `bay.result/1` as it is written: the fields of [`BayResult`] in their order, with `say` after
/// `reason`, and a refusal's `next_step` after `retry_when`, both derived from the reason and
/// never stored. Absent fields are omitted.
#[derive(Serialize)]
struct Wire<'a> {
    schema: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    op_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    candidate: Option<&'a str>,
    decision: Decision,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<Reason>,
    say: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    change: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed: Option<&'a [&'static str]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_when: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_step: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    waiting_for: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_s: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    receipt: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    freshness: Option<&'a Freshness>,
}

/// 0.3.5 (owner ruling 2026-10-23): the words a result prints for keys bay names in its own
/// code and stored records. A result is translated once, here, as it is written: the code and
/// every record in Git keep their names, and nothing printed carries an old word. A key is
/// renamed only when its new name is not already in the same object.
const PRINTED_KEYS: &[(&str, &str)] = &[
    ("trunk", "main"),
    ("trunk_commit", "main_commit"),
    ("trunk_tip", "main_tip"),
    ("trunk_moved", "main_moved"),
    ("trunk_red", "main_red"),
    ("landing", "merge"),
    ("landings", "merges"),
    ("landing_commits", "merge_commits"),
    ("landings_off_first_parent", "merges_off_first_parent"),
    ("landed", "merged"),
    ("landed_as", "merged_as"),
    ("landed_revision", "merged_revision"),
    ("landed_intent", "merged_intent"),
    ("landing_cost", "merge_cost"),
    ("published", "is_merged"),
    ("landed_on_your_paths", "merged_on_your_paths"),
    ("publish_retries", "merge_retries"),
    ("published_at", "merged_at"),
    ("executor", "runner"),
    ("executors", "runners"),
    ("executor_name", "runner_name"),
];

/// The keys whose string values name a state, a class, a row or an event, and the words those
/// values print as (0.3.5).
const PRINTED_VALUE_KEYS: &[&str] = &[
    "state",
    "class",
    "name",
    "kind",
    "event",
    "type",
    "outcome",
    "recorded_state",
    "receipt",
    "warning",
    "warnings",
    "code",
];

const PRINTED_VALUES: &[(&str, &str)] = &[
    ("landed", "merged"),
    ("published", "merged"),
    ("trunk", "main"),
    ("trunk-hook", "main-hook"),
    ("trunk_red", "main_red"),
    ("trunk_red_cleared", "main_red_cleared"),
    ("trunk_not_linear", "main_not_linear"),
    ("unattributed_landing", "outside_merge"),
    ("landed_on_your_paths", "merged_on_your_paths"),
    ("no_live_executor", "no_live_runner"),
];

/// The runner's role words, printed under the keys that name a role (the key class itself is
/// `runner` since 0.4.0).
const PRINTED_ROLE_KEYS: &[&str] = &["waiting_for", "lands_by"];
const PRINTED_ROLES: &[(&str, &str)] = &[
    ("executor", "runner"),
    ("executor_capacity", "runner_capacity"),
];

/// Keys whose values are ids that begin with an event kind (`trunk_not_linear:<sha>`).
const PRINTED_ID_KEYS: &[&str] = &["id", "recurrence_key"];

fn printed_value(word: &str) -> &str {
    PRINTED_VALUES
        .iter()
        .find(|(old, _)| *old == word)
        .map_or(word, |(_, new)| new)
}

fn printed_role(word: &str) -> &str {
    PRINTED_ROLES
        .iter()
        .find(|(old, _)| *old == word)
        .map_or(word, |(_, new)| new)
}

/// An id that begins with an old event kind, with the new one.
fn printed_id(id: &str) -> String {
    for (old, new) in PRINTED_VALUES {
        if let Some(rest) = id.strip_prefix(old)
            && (rest.starts_with(':') || rest.starts_with('/'))
        {
            return format!("{new}{rest}");
        }
    }
    id.to_owned()
}

/// `value` with the printed words of [`PRINTED_KEYS`] and [`PRINTED_VALUES`].
#[must_use]
pub fn printed(value: &Value) -> Value {
    printed_under(value, Under::Other)
}

/// What the key a value sits under says to translate in it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Under {
    Other,
    Word,
    Role,
    Id,
}

fn printed_under(value: &Value, under: Under) -> Value {
    match value {
        Value::Object(object) => {
            let mut out = serde_json::Map::new();
            for (key, inner) in object {
                let new = PRINTED_KEYS
                    .iter()
                    .find(|(old, _)| old == key)
                    .map(|(_, new)| *new)
                    .filter(|new| !object.contains_key(*new))
                    .unwrap_or(key);
                let key = key.as_str();
                let under = if PRINTED_ROLE_KEYS.contains(&key) {
                    Under::Role
                } else if PRINTED_VALUE_KEYS.contains(&key) {
                    Under::Word
                } else if PRINTED_ID_KEYS.contains(&key) {
                    Under::Id
                } else {
                    Under::Other
                };
                out.insert(new.to_owned(), printed_under(inner, under));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| printed_under(item, under))
                .collect(),
        ),
        Value::String(word) => Value::String(match under {
            Under::Word => printed_value(word).to_owned(),
            Under::Role => printed_role(printed_value(word)).to_owned(),
            Under::Id => printed_id(word),
            Under::Other => word.clone(),
        }),
        other => other.clone(),
    }
}

impl Serialize for BayResult {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let say = self.say();
        let detail = self.detail.as_ref().map(printed);
        let waiting_for = self.waiting_for.as_deref().map(printed_role);
        let state = self.state.as_deref().map(printed_value);
        Wire {
            schema: self.schema,
            op_id: self.op_id.as_deref(),
            candidate: self.candidate.as_deref(),
            decision: self.decision,
            state,
            reason: self.reason,
            say: &say,
            change: self.change.as_deref(),
            detail: detail.as_ref(),
            allowed: self.allowed.as_deref(),
            retry_when: self.retry_when,
            next_step: self.reason.map(Reason::next),
            waiting_for,
            elapsed_s: self.elapsed_s,
            evidence: self.evidence.as_deref(),
            receipt: self.receipt.as_deref(),
            freshness: self.freshness.as_ref(),
        }
        .serialize(serializer)
    }
}

impl BayResult {
    #[must_use]
    pub const fn new(decision: Decision) -> Self {
        Self {
            schema: SCHEMA,
            op_id: None,
            candidate: None,
            decision,
            state: None,
            reason: None,
            change: None,
            detail: None,
            allowed: None,
            retry_when: None,
            waiting_for: None,
            elapsed_s: None,
            evidence: None,
            receipt: None,
            freshness: None,
            human: None,
        }
    }

    #[must_use]
    pub fn ok(detail: Value) -> Self {
        Self::new(Decision::Ok).with_detail(detail)
    }

    #[must_use]
    pub fn failed(detail: impl Into<String>) -> Self {
        Self::new(Decision::Failed).with_detail(Value::String(detail.into()))
    }

    /// A refusal. `allowed` and `retry_when` always come from the reason-code table.
    #[must_use]
    pub fn refused(decision: Decision, reason: Reason) -> Self {
        let mut result = Self::new(decision);
        result.reason = Some(reason);
        result.allowed = Some(reason.allowed().to_vec());
        result.retry_when = Some(reason.retry_when());
        result
    }

    #[must_use]
    pub fn blocked(reason: Reason) -> Self {
        Self::refused(Decision::Blocked, reason)
    }

    #[must_use]
    pub fn with_detail(mut self, detail: Value) -> Self {
        self.detail = Some(detail);
        self
    }

    #[must_use]
    pub fn with_change(mut self, change: impl Into<String>) -> Self {
        self.change = Some(change.into());
        self
    }

    #[must_use]
    pub fn with_op(mut self, op_id: impl Into<String>) -> Self {
        self.op_id = Some(op_id.into());
        self
    }

    /// One JSON document, no extra whitespace, plus a trailing newline.
    ///
    /// # Errors
    ///
    /// Returns the serde error when `detail` cannot be serialized.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        Ok(format!("{}\n", serde_json::to_string(self)?))
    }

    /// Lines a person can read. The same fields as the JSON.
    #[must_use]
    pub fn to_human(&self) -> String {
        if let Some(line) = &self.human {
            return format!("{line}\n");
        }
        // The plain sentence first (owner ruling 2026-10-21), then the fields.
        let mut lines = vec![self.say(), format!("decision: {}", self.decision.word())];
        if let Some(reason) = self.reason {
            lines.push(format!("reason:   {}", reason.code()));
        }
        if let Some(change) = &self.change {
            lines.push(format!("change:   {change}"));
        }
        if let Some(op_id) = &self.op_id {
            lines.push(format!("op:       {op_id}"));
        }
        if let Some(detail) = &self.detail {
            push_detail(&mut lines, detail);
        }
        if let Some(allowed) = &self.allowed {
            let allowed = if allowed.is_empty() {
                "none needed".to_owned()
            } else {
                allowed.join(", ")
            };
            lines.push(format!("allowed:  {allowed}"));
        }
        if let Some(retry_when) = self.retry_when {
            lines.push(format!("retry when: {retry_when}"));
        }
        if let Some(reason) = self.reason {
            lines.push(format!("next step: {}", reason.next()));
        }
        lines.push(String::new());
        lines.join("\n")
    }
}

impl BayResult {
    /// G313 (owner ruling 2026-10-22, ch. 35 R5): what a person at a terminal reads: the plain
    /// sentence, the change it is about, and how to see the rest. No document, no reason code
    /// and no field a person need not read; `--json` prints all of it.
    #[must_use]
    pub fn to_terminal(&self) -> String {
        if let Some(line) = &self.human {
            return format!("{line}\n");
        }
        let mut lines = vec![self.say()];
        if let Some(change) = &self.change {
            lines.push(format!("change: {change}"));
        }
        lines.push("(add --json to see everything)".to_owned());
        lines.push(String::new());
        lines.join("\n")
    }
}

fn push_detail(lines: &mut Vec<String>, detail: &Value) {
    match detail {
        Value::Object(map) => {
            for (key, value) in map {
                let rendered = match value {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                lines.push(format!("{key}: {rendered}"));
            }
        }
        Value::String(text) => lines.push(text.clone()),
        other => lines.push(other.to_string()),
    }
}

/// G311 (owner ruling 2026-10-22, research ch. 35 R1): the one word list. Each word a person
/// who does not know Git need not know (owner ruling 2026-10-21), with the plain word said
/// instead. No `say` sentence holds one, nor an inflection of one ([`unplain`]); `bay help
/// --json` exports the list (`detail.words`), and the skill's "never say" line and the site's
/// copy checker are held to it, so the list has one home.
pub const PLAIN_WORDS: &[PlainWord] = &[
    PlainWord {
        word: "branch",
        say: "copy",
        avoid: &["fork"],
        means: "A separate line of the project's history. bay keeps each change apart, so you never make one.",
    },
    PlainWord {
        word: "commit",
        say: "saved version",
        avoid: &["snapshot", "revision"],
        means: "One saved state of the files, kept in the project's history.",
    },
    PlainWord {
        word: "merge",
        say: "combine",
        avoid: &["integrate"],
        means: "Adding a checked change to main, together with the work already there.",
    },
    PlainWord {
        word: "trunk",
        say: "main",
        avoid: &["master", "default branch", "upstream", "shared copy"],
        means: "The one version of the project that everyone's work is added to.",
    },
    PlainWord {
        word: "rebase",
        say: "update",
        avoid: &["replay"],
        means: "Bringing a change up to date with main, so it starts from main's newest files.",
    },
    PlainWord {
        word: "ref",
        say: "name",
        avoid: &["pointer"],
        means: "What bay calls one of the records it keeps in the project's history.",
    },
    PlainWord {
        word: "view",
        say: "folder",
        avoid: &["workspace", "worktree", "checkout"],
        means: "The folder bay makes for one change, holding only the files it works on.",
    },
];

/// One entry of [`PLAIN_WORDS`] (research ch. 35 § 8): the technical `word` a person never
/// reads, the word `say` puts in its place, the other names `avoid` lists for the same idea, and
/// what it `means`, in one line of plain words.
#[derive(Clone, Copy, Debug)]
pub struct PlainWord {
    pub word: &'static str,
    pub say: &'static str,
    pub avoid: &'static [&'static str],
    pub means: &'static str,
}

/// Whether `word` is one of [`PLAIN_WORDS`] or an inflection of one (`commits`, `merged`,
/// `committing`), compared without case.
#[must_use]
pub fn unplain(word: &str) -> bool {
    let word = word.to_ascii_lowercase();
    PLAIN_WORDS.iter().any(|entry| {
        let stem = entry.word;
        let bare = stem.strip_suffix('e').unwrap_or(stem);
        let rest = word.strip_prefix(stem).or_else(|| word.strip_prefix(bare));
        rest.is_some_and(|rest| ["", "s", "es", "d", "ed", "ing", "ted", "ting"].contains(&rest))
    })
}

/// `text` with each [`unplain`] word put plainly: for a message bay did not write as a
/// sentence, such as a fault's. A path that holds one, such as `refs/bay/registry`, becomes
/// "bay's records" whole: a word changed inside it would name a path that does not exist (G304).
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut token = String::new();
    // A word right after `bay` is a command's name (`bay merge`): it is said as it is typed, or
    // the sentence names a command that does not exist.
    let mut after_bay = false;
    let flush = |token: &str, out: &mut String, after_bay: &mut bool| {
        let core = token.trim_matches(|c: char| "()`'\".,;:".contains(c));
        if *after_bay {
            out.push_str(token);
        } else {
            out.push_str(&plain_token(token));
        }
        *after_bay = core == "bay";
    };
    for c in text.chars() {
        if c.is_whitespace() {
            flush(&token, &mut out, &mut after_bay);
            token.clear();
            out.push(c);
        } else {
            token.push(c);
        }
    }
    flush(&token, &mut out, &mut after_bay);
    out
}

/// One whitespace-free token of [`plain`]: a path holding an [`unplain`] word as "bay's
/// records", with the punctuation around it kept; any other token word by word.
fn plain_token(token: &str) -> String {
    let core = token.trim_matches(|c: char| "()`'\".,;:".contains(c));
    // `<rev>:<file>`, as Git names a file at a commit: the file is what a person can open, so
    // it is named alone when its own path is plain (`.bay/config.toml`).
    if let Some((rev, file)) = core.rsplit_once(':')
        && rev.contains('/')
        && !file.is_empty()
        && !file.split(|c: char| !c.is_ascii_alphabetic()).any(unplain)
    {
        let start = token.find(core).unwrap_or(0);
        return format!("{}{file}{}", &token[..start], &token[start + core.len()..]);
    }
    // A path on disk is what a person opens or names in a fix: kept whole, never put plainly (a
    // fleet finding, 2026-10-27: "cannot change to 'bay's records'" stood for a view's folder).
    if core.starts_with('/') || core.starts_with("~/") {
        return token.to_owned();
    }
    if core.contains('/') && core.split(|c: char| !c.is_ascii_alphabetic()).any(unplain) {
        let start = token.find(core).unwrap_or(0);
        return format!(
            "{}bay's records{}",
            &token[..start],
            &token[start + core.len()..]
        );
    }
    plain_words(token)
}

/// `text` with each [`unplain`] word put plainly, word by word.
fn plain_words(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    for c in text.chars() {
        if c.is_ascii_alphabetic() {
            word.push(c);
        } else {
            out.push_str(plain_word(&word));
            word.clear();
            out.push(c);
        }
    }
    out.push_str(plain_word(&word));
    out
}

/// `word`, or its plain stand-in when it is [`unplain`].
fn plain_word(word: &str) -> &str {
    if !unplain(word) {
        return word;
    }
    match word.to_ascii_lowercase().as_str() {
        "trunk" | "trunks" => "main",
        "commit" => "saved version",
        "commits" => "saved versions",
        "committed" => "saved",
        "committing" => "saving",
        "merge" => "combine",
        "merges" => "combines",
        "merged" => "combined",
        "merging" => "combining",
        "branch" => "copy",
        "branches" => "copies",
        "rebase" => "update",
        "rebases" => "updates",
        "rebased" => "updated",
        "rebasing" => "updating",
        "ref" => "name",
        "refs" => "names",
        "view" => "folder",
        "views" => "folders",
        // Another inflection: the stand-in of its stem.
        other => PLAIN_WORDS
            .iter()
            .find(|entry| other.starts_with(entry.word))
            .map_or("that", |entry| entry.say),
    }
}

/// The longest first sentence of a fault's message that `say` repeats. A sentence is never cut,
/// since a cut can flip its meaning ("... are not both set" read "... are"); a longer one is
/// left to the result's detail.
const SHORT_WORDS: usize = 30;

/// The string at the first of `pointers` in `detail` that holds a non-empty one.
fn text_at<'v>(detail: Option<&'v Value>, pointers: &[&str]) -> Option<&'v str> {
    let detail = detail?;
    pointers.iter().find_map(|pointer| {
        detail
            .pointer(pointer)?
            .as_str()
            .filter(|text| !text.trim().is_empty())
    })
}

/// The strings of the array `detail[key]`, for the first of `keys` that holds any.
fn strings(detail: Option<&Value>, keys: &[&str]) -> Vec<String> {
    let Some(detail) = detail else {
        return Vec::new();
    };
    keys.iter()
        .find_map(|key| {
            let found: Vec<String> = detail
                .get(*key)?
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            (!found.is_empty()).then_some(found)
        })
        .unwrap_or_default()
}

/// `items` as a short list a person reads: `a`, `a and b`, or `a, b and 3 more`.
fn listed(items: &[String]) -> Option<String> {
    match items {
        [] => None,
        [one] => Some(one.clone()),
        [one, two] => Some(format!("{one} and {two}")),
        [one, two, rest @ ..] => Some(format!("{one}, {two} and {} more", rest.len())),
    }
}

/// ` ({what})` when there is something to name, else nothing.
fn named(what: Option<String>) -> String {
    match what {
        Some(what) => format!(" ({what})"),
        None => String::new(),
    }
}

/// Who may give an approval the detail says is owed (G245), once each, in order.
fn approvers(detail: Option<&Value>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let checks = detail
        .and_then(|detail| detail.pointer("/approval/checks"))
        .and_then(Value::as_array);
    for check in checks.into_iter().flatten() {
        let given = check["approvers"].as_array().into_iter().flatten();
        for name in given.filter_map(Value::as_str) {
            if !names.iter().any(|known| known == name) {
                names.push(name.to_owned());
            }
        }
    }
    names
}

/// `bytes` in MiB to one decimal, rounded up, so a file over a limit never reads as at it.
fn mib(bytes: u64) -> String {
    let tenths = bytes.saturating_mul(10).div_ceil(1024 * 1024);
    format!("{}.{} MiB", tenths / 10, tenths % 10)
}

/// What a `CHANGE_TOO_LARGE` detail says is over its limit: the first limit exceeded, and how
/// many more are.
fn too_large(detail: Option<&Value>) -> Option<String> {
    let rows = detail?.get("exceeded")?.as_array()?;
    let first = rows.first()?;
    let max = first["max"].as_u64()?;
    let value = first["value"].as_u64()?;
    let what = match first["limit"].as_str()? {
        "max_file_mib" => {
            let path = first["paths"][0].as_str().unwrap_or("a file");
            format!("{path} is {}, over {max} MiB", mib(value))
        }
        "max_files" => format!("{value} files, over {max}"),
        "max_added_lines" => format!("{value} added lines, over {max}"),
        _ => return None,
    };
    Some(match rows.len() - 1 {
        0 => what,
        1 => format!("{what}, and one more limit"),
        more => format!("{what}, and {more} more limits"),
    })
}

/// The first words of a fault's message, put plainly.
fn short(detail: Option<&Value>) -> Option<String> {
    let text = match detail? {
        Value::String(text) => text.as_str(),
        other => text_at(Some(other), &["/why", "/error", "/message"])?,
    };
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    // The first sentence, whole: up to the first ". " (or the line's end).
    let sentence = line.split_once(". ").map_or(line, |(first, _)| first);
    let words: Vec<&str> = sentence.split_whitespace().collect();
    // Git's own words (a command it ran, or what git printed) are never put plainly: rewritten,
    // ref names inside them read as nonsense (fleet findings, 0.5.6 and 2026-10-27). Only bay's
    // words before them make the sentence; git's stay whole in `detail`.
    let git = words.iter().position(|word| {
        *word == "git"
            || ["fatal:", "error:", "remote:", "hint:"]
                .iter()
                .any(|prefix| word.starts_with(prefix))
    });
    let mine = &words[..git.unwrap_or(words.len())];
    if mine.len() > SHORT_WORDS {
        return None;
    }
    let short = plain(mine.join(" ").trim_end_matches(['.', ':', ';', ',']));
    match git {
        _ if short.is_empty() => None,
        Some(_) => Some(format!("{short}; git's own words are in the detail")),
        None => Some(short),
    }
}

impl Reason {
    /// The agent's next step on this reason: one imperative sentence, at most 25 words, every
    /// command in it written as typed (`` `bay merge` ``). Every refusal carries it as
    /// `next_step`, and `bay help --json` lists it with each code; it is the one home of what to
    /// do on a reason. Every reason is in this match, so a new one cannot compile without one.
    #[allow(clippy::too_many_lines)]
    #[must_use]
    pub const fn next(self) -> &'static str {
        match self {
            Self::InputChanged => {
                "Read the new version `detail` names, run `bay start <change>`, adjust the view, then `bay merge` again."
            }
            Self::InputUnobserved => {
                "Read the required version `detail` names, then run `bay start <change>` and `bay merge` again."
            }
            Self::MergeConflict => {
                "Run `bay start <change>`, resolve the listed paths in the view, then `bay merge` again."
            }
            Self::TrunkMoved => {
                "Do nothing: bay retries once more, then queues the change. `bay merge --wait` shows the outcome."
            }
            Self::PublishRetryExhausted => {
                "Read the cause in `detail` and tell the user; once it is fixed, `bay merge` again."
            }
            Self::RegistryUnreadable => {
                "Tell the user the key registry breaks a rule (`detail`); a registry key repairs it, for a re-enrolled removed key with `bay enrol --remove`."
            }
            Self::RegistryChanged => {
                "Do nothing: a runner with a valid key signs it again. `bay status <change>` shows the outcome."
            }
            Self::CheckFlaky => {
                "Tell the user the check gives two answers for one tree; once it is fixed, `bay merge` again. Never quarantine it yourself."
            }
            Self::ChangeNotLanded => {
                "Wait until the named change merges; `bay status <change>` shows its state. Then run the command again."
            }
            Self::HookTimeout => {
                "Do nothing: bay retries after a short wait. If it keeps happening, run `bay doctor` and tell the user."
            }
            Self::Queued => {
                "Keep working: the change waits its turn in the queue. `bay merge --wait` or `bay status <change>` shows the outcome."
            }
            Self::QueueDeadlineExceeded => {
                "Read `detail`, shrink or split the change in the view, then `bay merge` again; or ask the user to raise the budget."
            }
            Self::CheckFailed => {
                "Read the failing check in `detail`, repair the change in the view, then `bay merge` again."
            }
            Self::CheckUnavailable => {
                "Wait: bay retries once a worker can run the check. If `bay status --all` shows no live runner, tell the user."
            }
            Self::CheckUnknown => {
                "Wait for the check's result with `bay merge --wait`; never treat it as a pass. Repair the change if the check cannot answer."
            }
            Self::PolicyChanged => {
                "Run `bay start <change>`, then `bay merge` again: the change is checked under the current policy."
            }
            Self::PolicyInvalid => {
                "Tell the user a rules file in `.bay/` is invalid, as `detail` says; repair it in a new revision."
            }
            Self::PolicyMissing => {
                "Tell the user main has no `.bay/policy.toml`, so nothing can merge; set it up only if they ask."
            }
            Self::ViewIncomplete => {
                "Run `bay start <change>`; copy back any files `detail.unsaved_kept` names, then `bay merge`."
            }
            Self::PublishOutcomeUnknown => {
                "Do nothing yet: bay reads main again. Never report it merged; `bay status <change>` shows the outcome."
            }
            Self::TrunkRed => {
                "Wait for a fix to the red paths, or fix it yourself with `bay start <intent> --fixes <change>`, the change `detail.fixes` names."
            }
            Self::ChangeAlreadyLanded => {
                "Do not retry: the change already merged. For more edits, `bay start <intent>` opens the next change."
            }
            Self::ChangeSuperseded => {
                "Read the change that took over in `detail`; work in it, or `bay start` again once it merges or is abandoned."
            }
            Self::ChangeHeld => {
                "Another live session holds this change: wait until it ends, or work on another change. `bay status --all` shows the holder."
            }
            Self::AbandonRefused => {
                "Only the change's own actor may abandon it: ask that actor, or leave the change as it is."
            }
            Self::ActorUnnamed => "Set `BAY_ACTOR` to your actor name, then run the command again.",
            Self::ActorUnproven => {
                "Tell the user policy needs proof of the actor: a hardware agent key, or one OS account per actor."
            }
            Self::CoverageUnknown => {
                "Keep editing; `bay merge` again once coordination data is reachable."
            }
            Self::HookInputInvalid => {
                "Read `detail.why`: a hook entry under a wrong name needs `bay install`; a payload this bay cannot read needs `bay update`."
            }
            Self::ShellDenied => {
                "Do not run that command again; it is never allowed. Ask the user if the work truly needs it."
            }
            Self::PathHeld => {
                "Another session holds the path: wait until it stops, or edit other paths."
            }
            Self::LockNotHeld => {
                "Edit the files `detail.paths` names again in a session whose hook takes their lock, then `bay merge` again."
            }
            Self::AssetMissing => {
                "Save the files `detail.paths` names again, so the store holds their bytes (or fix the store `detail.why` names), then `bay merge` again."
            }
            Self::UndoesMerged => {
                "Run `bay start <change>`, keep the work `detail.change` merged in `detail.paths`, then `bay merge` again; to undo it on purpose, use `bay undo`."
            }
            Self::RemoteNotFetched => {
                "Run `bay status` first: it fetches main and the fleet's live records, which `bay status --all` only reads; then run `bay status --all` again. If `bay status` fails, `bay doctor` names what is wrong."
            }
            Self::ConfigMissing => {
                "Tell the user bay is not set up here; commit `.bay/config.toml` to main only if they ask."
            }
            Self::ToolBelowFloor => {
                "Update the tool `detail.tool` names to `detail.floor`, or remove its `[agent]` product, then run the command again."
            }
            Self::ToolAbsent => {
                "Tell the user to install the tool `detail.tool` names, or remove its `[agent]` product, then run `bay doctor` again."
            }
            Self::RemoteUnreachable => {
                "Check the network and this machine's key for the remote, then run `bay doctor` again."
            }
            Self::HooksMissing => {
                "Do what the hooks row's `recovery` says: merge the tracked settings file, or run `bay install`."
            }
            Self::SkillMismatch => {
                "Run `bay install` to copy this version's skill, then `bay doctor` again."
            }
            Self::SandboxAbsent => {
                "Tell the user to install the sandbox `detail` names, or set the runner's sandbox to none."
            }
            Self::CheckEnvUnset => {
                "Tell the user to set the variables `detail` names for the runner, then run `bay doctor --role runner` again."
            }
            Self::ExecutorKeyMissing => {
                "Tell the user to set and enroll the runner's `runner` and `producer` keys, then run `bay doctor --role runner` again."
            }
            Self::KeyExposed => {
                "Tell the user to move the key `detail` names onto hardware, or out of the account agent sessions run as."
            }
            Self::UpdateUnavailable => "Run `bay update` again later, once a release is reachable.",
            Self::UpdateUnverified => {
                "Keep the installed bay and tell the user; run `bay update` again once the release is republished."
            }
            Self::UpdatePinned => {
                "Keep this version: `bay update` moves only after this machine's pin moves."
            }
            Self::TalkRefused => {
                "Read `detail.why`, correct or supersede the talk as it says, then `bay talk` again."
            }
            Self::UnattributedLanding => {
                "Do not retry: that commit did not merge through bay, so it names no author bay knows. Ask whoever made it."
            }
            Self::EvidenceWithdrawn => {
                "Do not retry: an evidence record is never removed or changed. Push notes that keep every record."
            }
            Self::PromotionRefused => {
                "Push the commit `bay status` names promotable for that target, or wait until one is."
            }
            Self::LaneLeak => {
                "Take the lane commits out with `bay start <change> --from <branch>`, or start a lane change: `bay start <intent> --lane <lane>`."
            }
            Self::ApprovalPending => {
                "Tell the user who can approve (`detail.approval`); the approver runs `bay approve <change>`. Then the runner merges it, or `bay merge --wait`."
            }
            Self::WipLimit => {
                "Merge or abandon one of the changes `detail` names (`live_changes`, or `review.oldest`), then run the command again."
            }
            Self::DiskLow => {
                "Tell the user to free disk space on this machine, or lower `bay.minFreeMiB`, then run the command again."
            }
            Self::TrunkHeld => {
                "Wait: the change stays queued and merges once the hold clears; `bay status` shows the hold. Only fixes and reverts merge now."
            }
            Self::PathsUndeclared => {
                "Run `bay start <intent> --from <branch>` again, with `--paths` covering every path in `detail.uncovered`."
            }
            Self::BranchDiverged => {
                "Run `bay start` again with `--from <commit>`: `detail.local` takes this repository's branch, `detail.remote` the remote's. Or push yours first."
            }
            Self::DependencyPending => {
                "Wait: the change merges after each change it names with `--after` merges; `bay status <change>` shows them."
            }
            Self::DependencyAbandoned => {
                "Run `bay abandon`, then `bay start <intent> --new` without that `--after`; or restart the dropped change."
            }
            Self::ChangeTooLarge => {
                "Keep large files elsewhere or split the change, then `bay merge` again. Never raise the limits to get it through."
            }
            Self::ViewNotYours => {
                "Work in a view of your own; to take this one over from its actor, run `bay start <change> --adopt`."
            }
        }
    }

    /// The plain sentence of this reason that names nothing from a detail; `None` for a reason
    /// whose sentence does ([`Reason::say`]). Every reason is in this match, so a new one must
    /// be given a sentence.
    #[allow(clippy::too_many_lines)]
    const fn fixed_say(self) -> Option<&'static str> {
        Some(match self {
            Self::InputChanged
            | Self::InputUnobserved
            | Self::MergeConflict
            | Self::CheckFlaky
            | Self::CheckFailed
            | Self::CheckUnknown
            | Self::TrunkRed
            | Self::PathHeld
            | Self::LaneLeak
            | Self::ApprovalPending
            | Self::TrunkHeld
            | Self::PathsUndeclared
            | Self::BranchDiverged
            | Self::ChangeTooLarge
            | Self::ViewNotYours
            | Self::WipLimit
            | Self::ToolBelowFloor
            | Self::ToolAbsent
            | Self::HookInputInvalid => return None,
            Self::TrunkMoved => {
                "Saved. Main moved on meanwhile, so bay tries once more on its own; nothing to do."
            }
            Self::PublishRetryExhausted => {
                "Nothing was added: adding the change kept failing. Ask whoever runs bay here to look."
            }
            Self::RegistryUnreadable => {
                "Nothing signed can be checked: the project's list of trusted keys has a mistake. Ask whoever holds its key to fix it."
            }
            Self::RegistryChanged => {
                "Saved. The list of trusted keys changed meanwhile; bay tries again once it is signed, nothing to do."
            }
            Self::ChangeNotLanded => {
                "Not on main yet: that change is still waiting. Wait until it is added."
            }
            Self::HookTimeout => {
                "Not answered yet: bay took too long, and it tries again shortly; nothing to do."
            }
            Self::Queued => "Saved. It waits in line behind other changes; keep working meanwhile.",
            Self::QueueDeadlineExceeded => {
                "Nothing was added: this change waited in line too long. Make it smaller, or ask for more time."
            }
            Self::CheckUnavailable => {
                "Saved. It waits for a machine that can run its checks; nothing to do."
            }
            Self::PolicyChanged => {
                "Not added yet: the rules changed while this was checked. bay checks it again under the new rules."
            }
            Self::PolicyInvalid => "Not added: a rules file in .bay/ has a mistake. Fix it first.",
            Self::PolicyMissing => {
                "Nothing can be added: this project has no rules file (.bay/policy.toml) yet. Ask whoever set it up."
            }
            Self::ViewIncomplete => {
                "Not done yet: an earlier step was interrupted. Resume it before going on."
            }
            Self::PublishOutcomeUnknown => {
                "Not known yet: bay cannot tell whether the change was added. It looks again; do not report it as saved."
            }
            Self::ChangeAlreadyLanded => {
                "Nothing to add: this change is already on main. Start a new one for more edits."
            }
            Self::ChangeSuperseded => {
                "Nothing was added: someone else's change took over this work. Read it before going on."
            }
            Self::ChangeHeld => {
                "Not now: another session is working on this change. Wait for it to finish."
            }
            Self::AbandonRefused => {
                "Nothing was given up: only the person who owns this change can give it up."
            }
            Self::ActorUnnamed => "To save, set your name in BAY_ACTOR, then try again.",
            Self::ActorUnproven => {
                "Nothing was saved: bay cannot prove who is saving on this computer. Ask for a separate account or a hardware key."
            }
            Self::CoverageUnknown => {
                "Allowed, unchecked: bay cannot reach its shared notes right now. Keep working, and save again later."
            }
            Self::ShellDenied => "Stopped: that command could lose work, and it is never allowed.",
            Self::LockNotHeld => {
                "Nothing was added: files that need a lock were edited without it, or another session holds it now. Edit them again to take it."
            }
            Self::AssetMissing => {
                "Nothing was added: the store does not hold some large files this change names. Save them again."
            }
            Self::UndoesMerged => {
                "Nothing was added: this change would undo work that reached main after it began. Bring in main, keep that work, then try again."
            }
            Self::RemoteNotFetched => {
                "Nothing was shown: this machine has not yet read what the other machines last said. Run bay status first, then this again."
            }
            Self::ConfigMissing => {
                "Nothing was done: bay is not set up in this project yet. Ask whoever runs it, or set it up."
            }
            Self::RemoteUnreachable => {
                "Nothing was done: bay cannot reach main from here. Check the network and the access key."
            }
            Self::HooksMissing => {
                "Not guarded yet: bay's safety settings are missing here. Run bay install."
            }
            Self::SkillMismatch => {
                "Out of date: the bay guide installed here is older than bay. Run bay install."
            }
            Self::SandboxAbsent => {
                "No checks can run here: the safe space bay runs them in is missing. Ask whoever runs bay."
            }
            Self::CheckEnvUnset => {
                "No checks can run here: this checking machine lacks settings they need. Ask whoever runs it."
            }
            Self::ExecutorKeyMissing => {
                "Nothing can be added from here: this checking machine has no key to sign with. Ask whoever runs it."
            }
            Self::KeyExposed => {
                "At risk: a secret key here can be read by the assistant's account. Ask whoever runs bay to move it."
            }
            Self::UpdateUnavailable => {
                "Not updated: no bay update could be reached right now. Try again later."
            }
            Self::UpdateUnverified => {
                "Not updated: the bay update could not be verified, so it was not installed."
            }
            Self::UpdatePinned => "Not updated: this project keeps bay at a fixed version.",
            Self::TalkRefused => {
                "Not sent: read why in the detail, fix the message, then send it again."
            }
            Self::UnattributedLanding => {
                "Not through bay: that change reached main another way, so bay cannot say who made it."
            }
            Self::EvidenceWithdrawn => {
                "Refused: a saved record of checks can never be removed or changed."
            }
            Self::PromotionRefused => {
                "Not promoted: that version is not ready for the next stage. Use the one bay status names, or wait."
            }
            Self::DiskLow => {
                "Nothing was done: this computer is almost out of disk space. Free some, then try again."
            }
            Self::DependencyPending => "Saved. It waits until the change it depends on is added.",
            Self::DependencyAbandoned => {
                "Nothing can be added: a change this one depends on was dropped. Start it again without it."
            }
        })
    }

    /// One plain sentence for a person who does not know Git (owner ruling 2026-10-21): what
    /// happened and the one next step, with what `detail` names when it is there.
    #[allow(clippy::too_many_lines)]
    #[must_use]
    pub fn say(self, detail: Option<&Value>) -> String {
        // A fleet finding: GitHub's limit on unauthenticated reads refused updates; the
        // sentence says when it resets, and how to read past it.
        if self == Self::UpdateUnavailable
            && let Some(at) = text_at(detail, &["/why"])
                .and_then(|why| why.split("retry after ").nth(1))
                .map(|rest| rest.split([';', ' ']).next().unwrap_or(rest))
        {
            return format!(
                "Not updated: GitHub limits reads until {at}. Try again then, or set GH_TOKEN."
            );
        }
        // T7, T12: talk is off unless main's policy turns it on; the sentence names the line.
        if self == Self::TalkRefused
            && detail.and_then(|detail| detail.get("talk_off")) == Some(&Value::Bool(true))
        {
            return "Not sent: talk is off in this project; `[talk] enabled = true` in main's policy turns it on.".to_owned();
        }
        // G305: doctor's evidence that the trunk changes around bay.
        if self == Self::HooksMissing
            && let (Some(count), Some(window)) = (
                detail
                    .and_then(|detail| detail.pointer("/guard/around_bay/count"))
                    .and_then(Value::as_u64),
                detail
                    .and_then(|detail| detail.pointer("/guard/around_bay/of"))
                    .and_then(Value::as_u64),
            )
        {
            return format!(
                "bay does not guard this project yet: {count} of the last {window} changes reached main without its checks. Turn on its safety settings, then run bay install."
            );
        }
        if let Some(fixed) = self.fixed_say() {
            return fixed.to_owned();
        }
        let paths = || {
            named(listed(&strings(
                detail,
                &["conflicts", "uncovered", "paths"],
            )))
        };
        let check = || named(text_at(detail, &["/checks/0/check", "/check"]).map(str::to_owned));
        let input = || named(text_at(detail, &["/input", "/inputs/0/input"]).map(str::to_owned));
        match self {
            Self::InputChanged => format!(
                "Nothing was added: something this change relies on was updated{}. Read the new version, adjust, then save again.",
                input()
            ),
            Self::InputUnobserved => format!(
                "Not added yet: this change must first read the latest version of something it relies on{}. Then save again.",
                input()
            ),
            Self::MergeConflict => format!(
                "Nothing was added: someone changed the same lines{}. The changes must be combined first.",
                paths()
            ),
            Self::CheckFlaky => format!(
                "Nothing was added: a check{} gave two answers for the same change. Ask a developer to fix that check.",
                check()
            ),
            Self::CheckFailed => format!(
                "Nothing was added: a check{} did not pass. Fix it, then save again.",
                check()
            ),
            Self::CheckUnknown => format!(
                "Not added yet: a check gave no clear answer{}. Wait for it.",
                check()
            ),
            Self::TrunkRed => format!(
                "Nothing can be added yet: main is broken in these files{}. Wait for the fix, or help fix it.",
                paths()
            ),
            Self::PathHeld => format!(
                "Not now: another session is editing these files{}. Wait for it to finish.",
                paths()
            ),
            Self::LaneLeak => format!(
                "Nothing was added: this change holds try-out work that must not be added{}. Redo it without that work.",
                paths()
            ),
            Self::ApprovalPending => {
                let who = match approvers(detail).as_slice() {
                    [] => "a person's OK".to_owned(),
                    [one] => format!("{one}'s OK"),
                    [one, two] => format!("an OK from {one} or {two}"),
                    [one, two, ..] => format!("an OK from {one}, {two} or another approver"),
                };
                let because = match listed(&strings(detail, &["paths"])) {
                    Some(paths) => format!(" because it changes {paths}"),
                    None => String::new(),
                };
                format!("Saved, not added yet: it waits for {who}{because}.")
            }
            Self::TrunkHeld => format!(
                "Not added yet: main is on hold{}. Your change waits.",
                named(text_at(detail, &["/hold/reason"]).map(plain))
            ),
            Self::PathsUndeclared => format!(
                "Nothing was added: the work touches files it did not list{}. Start again, listing them.",
                paths()
            ),
            Self::BranchDiverged => format!(
                "Nothing was started: your copy{} here and the one online differ. Say which one you mean, or send yours first.",
                named(text_at(detail, &["/ref"]).map(plain))
            ),
            Self::ChangeTooLarge => format!(
                "Nothing was saved: this change is too big{}. Keep large files elsewhere, such as a shared drive.",
                named(too_large(detail))
            ),
            // G520: whose folder it is, by the name bay knows them by.
            Self::ViewNotYours => format!(
                "Not done: this folder holds {} work. Use a folder of your own, or take this one over first.",
                text_at(detail, &["/view_actor"])
                    .map_or_else(|| "someone else's".to_owned(), |owner| format!("{}'s", plain(owner)))
            ),
            // G310: a full review queue is named apart from the actor's own live changes.
            Self::WipLimit if detail.is_some_and(|detail| detail.get("review").is_some()) => {
                "Not added yet: too many changes already wait for a person's OK. Answer or finish those first, then save again."
                    .to_owned()
            }
            Self::WipLimit => {
                "Nothing was started: you already have the most open changes allowed. Finish or give one up, then start again."
                    .to_owned()
            }
            // A hook entry under an old name: the fix is the entry, not a newer bay.
            Self::HookInputInvalid
                if detail.and_then(|detail| detail.get("misnamed")) == Some(&Value::Bool(true)) =>
            {
                "Stopped: bay's safety settings here call it by an old name. Run bay install."
                    .to_owned()
            }
            Self::HookInputInvalid => {
                "Stopped: bay could not read what the assistant asked to do.".to_owned()
            }
            Self::ToolBelowFloor => match text_at(detail, &["/tool"]) {
                Some("git") => "Nothing was done: the Git program on this computer is too old. Update it, then try again.",
                Some("claude") => "Nothing was done: Claude Code on this computer is too old for bay. Update it.",
                _ => "Nothing was done: the coding assistant on this computer is too old for bay. Update it.",
            }
            .to_owned(),
            Self::ToolAbsent => match text_at(detail, &["/tool"]) {
                Some("claude") => "Nothing was done: Claude Code is not installed here, but bay expects it. Install it.",
                _ => "Nothing was done: the coding assistant bay expects is not installed here. Install it.",
            }
            .to_owned(),
            _ => "Not done yet; read the detail for what is needed, then try again.".to_owned(),
        }
    }
}

impl BayResult {
    /// One plain sentence for a person who does not know Git (owner ruling 2026-10-21): what
    /// happened and the one next step, at most about 25 words, with none of [`PLAIN_WORDS`]. Made
    /// here from the decision, the reason and a few detail fields, never at a call site.
    #[must_use]
    pub fn say(&self) -> String {
        let detail = self.detail.as_ref();
        if let Some(reason) = self.reason {
            return reason.say(detail);
        }
        match self.decision {
            Decision::Landed if text_at(detail, &["/unchecked"]).is_some() => {
                "Saved and added to main; no checks are set up here, so nothing was checked."
                    .to_owned()
            }
            // N-9: a waived check did not pass, and the sentence never says it did.
            Decision::Landed if waived(detail).is_some() => format!(
                "Saved and added to main; a check{} did not pass and was waived by a person.",
                named(waived(detail))
            ),
            Decision::Landed => "Saved and checked; it is now on main.".to_owned(),
            Decision::Queued if self.waiting_for.as_deref() == Some("remote") => {
                "Saved here; it is sent and checked once main can be reached.".to_owned()
            }
            Decision::Queued => match (
                detail.and_then(|detail| detail["place"].as_u64()),
                detail.and_then(|detail| detail["in_line"].as_u64()),
            ) {
                (Some(1), _) => {
                    "Saved. It is first in line; bay checks it and adds it to main next.".to_owned()
                }
                (Some(place), Some(in_line)) => format!(
                    "Saved. It is place {place} of {in_line} in line; bay checks it when its turn comes."
                ),
                _ => "Saved. bay checks it and adds it to main next.".to_owned(),
            },
            Decision::Ok => change_say(self)
                .or_else(|| talk_say(self))
                .unwrap_or_else(|| ok_say(self).to_owned()),
            Decision::Blocked => {
                "Not done yet; read the detail for what is needed, then try again.".to_owned()
            }
            // G389: a setup that stopped after writing keys or settings never says nothing
            // changed.
            Decision::Failed if detail.and_then(|detail| detail.get("changed")).is_some() => {
                match short(detail) {
                    Some(short) => {
                        format!("Not finished: {short}. Run it again; it keeps what it set up.")
                    }
                    None => "Not finished; run it again, it keeps what it set up.".to_owned(),
                }
            }
            Decision::Failed => match short(detail) {
                Some(short) => format!("Nothing was changed: {short}."),
                None => "Nothing was changed: bay could not finish.".to_owned(),
            },
        }
    }
}

/// N-9: the check of the first `coverage_gaps` entry a waiver let through, if any.
fn waived(detail: Option<&Value>) -> Option<String> {
    detail?
        .get("coverage_gaps")?
        .as_array()?
        .iter()
        .find(|gap| gap.get("waived_by").is_some())
        .and_then(|gap| gap["check"].as_str())
        .map(str::to_owned)
}

/// G304: the sentence of `bay approve`'s `ok`, by who adds the change next (`lands_by`), and
/// whether it was a yes, a no or a waiver.
fn answered_say(detail: Option<&Value>) -> Option<&'static str> {
    let detail = detail?;
    let lands_by = detail.get("lands_by")?.as_str()?;
    let waived = detail.get("waived").is_some();
    Some(match (lands_by, waived) {
        ("author_repairs", _) => "You said no; the change waits until its author fixes it.",
        ("executor", _) => "Approved; the change is added to main on its own.",
        ("here", false) => "Approved; save the change again here to add it to main.",
        ("here", true) => {
            "Waived; save the change again here to add it, with that check named as not passed."
        }
        (_, false) => "Approved; the change is added once its author saves it again.",
        (_, true) => {
            "Waived; the change is added once its author saves it again, with that check named as not passed."
        }
    })
}

/// The sentence of a status read of one change, from that change's own row (fleet findings,
/// 2026-10-27: four seats read "Done; nothing else is needed" for a change still in line).
fn change_say(result: &BayResult) -> Option<String> {
    let id = result.change.as_deref()?;
    let [row] = result
        .detail
        .as_ref()?
        .get("changes")?
        .as_array()?
        .as_slice()
    else {
        return None;
    };
    if row["change"] != id {
        return None;
    }
    let id = &crate::change::shown(row["intent"].as_str().unwrap_or_default(), id);
    // G437: the row's state is bay's own word (`landed`), read before the printed word
    // (`merged`) replaces it; the sentence never matched what status writes.
    match row["state"].as_str() {
        Some("landed") => return Some(format!("{id} is on main.")),
        Some("abandoned") => {
            return Some(format!("{id} was given up; nothing more happens to it."));
        }
        _ => {}
    }
    let op = &row["operation"];
    if op.is_null() {
        return Some(format!(
            "{id} is not submitted yet; run `bay merge` on it when it is ready."
        ));
    }
    if !op["lease"].is_null() {
        return Some(format!("{id} is being checked now."));
    }
    let outcome = op["outcome"].as_str()?;
    if outcome == Reason::Queued.code() {
        return Some(match op["place"].as_u64() {
            Some(place) => format!(
                "{id} waits in line at place {place}; bay checks it and adds it to main in its turn."
            ),
            None => format!("{id} waits in line; bay checks it and adds it to main in its turn."),
        });
    }
    Reason::from_code(outcome).map(|reason| format!("{id}: {}", reason.say(None)))
}

/// T12: the sentence of a `bay talk` result (its `detail.talk`, or a round's `detail.round`),
/// by its state: what happened and what comes next, so a dry run and an open ask never read as
/// finished. A name, a time or an id is shown only as bay writes one, never the talk's prose
/// (ruling R13).
fn talk_say(result: &BayResult) -> Option<String> {
    let detail = result.detail.as_ref()?;
    let shown = |key: &str| {
        detail
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| crate::names::operator_name(text).is_ok())
    };
    let dry_run = detail.get("dry_run") == Some(&Value::Bool(true));
    // `bay talk --show` and `--round <id>` read; they change nothing.
    if result.state.as_deref() == Some("shown") {
        return Some(if detail.get("round").is_some() {
            "Here is the round: one row per recipient, with its talk, its standing and its answer; nothing was changed.".to_owned()
        } else {
            "Here is the talk and every reply to it, in order; nothing was changed.".to_owned()
        });
    }
    if detail.get("round").is_some() {
        let count = detail.get("talks").and_then(Value::as_array)?.len();
        return Some(match (dry_run, shown("round")) {
            (true, _) => format!(
                "Nothing was sent: this was a dry run of {count} talks, one to each recipient."
            ),
            (false, Some(round)) => format!(
                "Sent {count} talks, one to each recipient; each owes its own reply. See who answered with bay talk --round {round}."
            ),
            (false, None) => {
                format!("Sent {count} talks, one to each recipient; each owes its own reply.")
            }
        });
    }
    detail.get("talk")?.as_str()?;
    if dry_run {
        return Some(
            "Nothing was sent: this was a dry run, and the talk passed every check. Run it again without --dry-run to send it."
                .to_owned(),
        );
    }
    let who = shown("who").unwrap_or("the recipient");
    let from = shown("from").unwrap_or("the sender");
    let status = shown("status").filter(|word| crate::talk::STATUS_WORDS.contains(word));
    let by = detail
        .get("follow_up")
        .and_then(Value::as_str)
        .and_then(crate::clock::unix_of)
        .map(crate::clock::rfc3339_of);
    Some(match (result.state.as_deref()?, by) {
        ("sent", Some(by)) => format!("Sent to {who}; {who} owes a reply by {by}."),
        ("sent", None) => format!("Sent to {who}; {who} owes a reply."),
        ("already_sent", Some(by)) => {
            format!("Already sent to {who}, who still owes a reply by {by}; nothing new was sent.")
        }
        ("already_sent", None) => format!("Already sent to {who}; nothing new was sent."),
        ("not_pushed", _) => {
            "Kept here, not sent yet: main could not be reached. Run the same command again to send it."
                .to_owned()
        }
        ("answered", _) => format!("Answered; {from}, who asked, is told."),
        ("already_answered", _) => {
            "Already answered with that value; nothing new was sent.".to_owned()
        }
        ("backbriefed", by) => {
            let status = status.unwrap_or("progress");
            match by {
                Some(by) => format!(
                    "Reported {status}; {from}, who asked, is told. Your next reply is owed by {by}."
                ),
                None => format!("Reported {status}; {from}, who asked, is told."),
            }
        }
        ("already_backbriefed", _) => format!(
            "Already reported {}; nothing new was sent.",
            status.unwrap_or("that")
        ),
        _ => return None,
    })
}

/// The sentence of an `ok` result with no reason, by what it carries.
fn ok_say(result: &BayResult) -> &'static str {
    let detail = result.detail.as_ref();
    // The publication's `no_effect` and `superseded` states (`crate::publish`).
    match result.state.as_deref() {
        Some("no_effect") => {
            return "Nothing new to add; main already has these edits.";
        }
        Some("superseded") => {
            return "A newer save of this change took over; nothing else to do here.";
        }
        _ => {}
    }
    // G305: doctor (whose detail names the box's `role`) is ready, but bay does not guard this
    // repository's Git here.
    if detail.is_some_and(|detail| {
        detail.get("role").is_some() && detail.pointer("/guard/hook") == Some(&Value::from("off"))
    }) {
        return "bay works here, but it does not guard this project yet: changes can reach main without its checks. Turn on its safety settings to guard it.";
    }
    // G304: an approval or a waiver is given, and the change is not on main yet;
    // the sentence says who adds it (`lands_by`, written by `bay approve`).
    if let Some(sentence) = answered_say(detail) {
        return sentence;
    }
    // The owner setup (`bay install --approver`, owner ruling 2026-10-25): one plain sentence.
    if let Some(detail) = detail.filter(|detail| detail.get("approver").is_some()) {
        return if detail["enrolled"] == true || detail["started_registry"] == true {
            "Done: you can now approve changes. See what waits for you with bay waiting, and approve one with bay approve <change>."
        } else {
            "Already set up: you can approve changes. See what waits for you with bay waiting."
        };
    }
    // G320: a clone install wrote hook entries that main does not hold yet.
    if detail.is_some_and(|detail| detail.get("hooks_to_land") == Some(&Value::Bool(true))) {
        return "bay is set up here; add the new settings file to main, so every copy of the project gets the safety settings.";
    }
    // G320: `bay install --machine` writes the program and its guide, never hook settings.
    if detail.is_some_and(|detail| detail.get("scope") == Some(&Value::from("machine"))) {
        return "bay and its guide are installed on this computer; safety settings come with the project itself.";
    }
    let view = detail.is_some_and(|detail| detail.get("view").is_some());
    let nothing_saved = detail
        .and_then(|detail| detail.get("saved"))
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty);
    if view {
        "Ready; make your edits in the folder shown, then save them."
    } else if nothing_saved {
        "Nothing to save; no files were changed."
    } else {
        "Done; nothing else is needed."
    }
}

#[cfg(test)]
mod tests {
    use super::{BayResult, Decision, Freshness, Reason, SCHEMA, plain, unplain};
    use serde_json::json;
    use std::collections::HashSet;

    #[test]
    fn a_talk_read_says_it_changed_nothing_and_never_that_it_sent() {
        let shown = |detail: serde_json::Value| {
            let mut result = BayResult::ok(detail);
            result.state = Some("shown".to_owned());
            result.say()
        };
        let round = shown(json!({"round": "r1", "talks": [{"who": "a"}, {"who": "b"}]}));
        assert!(round.starts_with("Here is the round"), "{round}");
        assert!(!round.contains("Sent"), "{round}");
        let talk = shown(json!({"talk": "abcdefghijklmnop", "entries": []}));
        assert!(talk.starts_with("Here is the talk"), "{talk}");
    }

    #[test]
    fn git_words_in_a_fault_are_never_put_plainly() {
        let say = BayResult::failed(
            "the merging push was refused: git push -q --force-with-lease=refs/bay/queue/760:abc origin d6d:refs/bay/queue/760 failed",
        )
        .say();
        assert_eq!(
            say,
            "Nothing was changed: the combining push was refused; git's own words are in the detail."
        );
        let fatal =
            BayResult::failed("fatal: cannot change to '/home/u/repo/.git/bay/views/x'").say();
        assert!(!fatal.contains("bay's records"), "{fatal}");
    }

    #[test]
    fn a_path_on_disk_in_a_fault_is_kept_whole() {
        let said = super::plain(
            "fatal: cannot change to '/home/u/repo/.git/bay/views/abcd1234': No such file or directory",
        );
        assert!(
            said.contains("'/home/u/repo/.git/bay/views/abcd1234'"),
            "{said}"
        );
        let refs = super::plain("no key registry (refs/bay/registry), so the trunk waits");
        assert!(
            !refs.contains("refs/bay"),
            "a ref is still put plainly: {refs}"
        );
    }

    #[test]
    fn an_update_github_limits_says_when_to_try_again() {
        let say = Reason::UpdateUnavailable.say(Some(&json!({"why":
            "cannot download https://api.github.com/x: 403; GitHub asks to retry after 2026-10-27T13:00:00Z"})));
        assert_eq!(
            say,
            "Not updated: GitHub limits reads until 2026-10-27T13:00:00Z. Try again then, or set GH_TOKEN."
        );
        let plain = Reason::UpdateUnavailable.say(Some(&json!({"why": "offline"})));
        assert!(plain.contains("Try again later"), "{plain}");
    }

    #[test]
    fn a_status_of_one_change_says_that_changes_state() {
        let named = BayResult::ok(json!({"changes": [{"change": "abcd1234",
            "intent": "CARD-7 the ball lane", "state": "landed", "operation": {}}]}))
        .with_change("abcd1234".to_owned())
        .say();
        assert!(
            named.starts_with("CARD-7 the ball lane (abcd1234)"),
            "{named}"
        );
        let read = |row: serde_json::Value| {
            BayResult::ok(json!({"changes": [row]}))
                .with_change("abcd1234".to_owned())
                .say()
        };
        let queued = read(json!({"change": "abcd1234", "state": "ready",
            "operation": {"lease": null, "outcome": Reason::Queued.code(), "state": "admitted"}}));
        assert!(queued.contains("waits in line"), "{queued}");
        let merged = read(json!({"change": "abcd1234", "state": "landed", "operation": {}}));
        assert!(merged.contains("is on main"), "{merged}");
        let draft = read(json!({"change": "abcd1234", "state": "open", "operation": null}));
        assert!(draft.contains("not submitted"), "{draft}");
        let refused = read(json!({"change": "abcd1234", "state": "ready",
            "operation": {"lease": null, "outcome": Reason::CheckFailed.code()}}));
        assert!(!refused.contains("nothing else is needed"), "{refused}");
    }

    #[test]
    fn ok_json_names_the_schema_and_decision() {
        let result = BayResult::ok(json!({"git": "git version 2.54.0"}));
        let value: serde_json::Value = serde_json::from_str(&result.to_json().unwrap()).unwrap();
        assert_eq!(value["schema"], SCHEMA);
        assert_eq!(value["decision"], "ok");
        assert_eq!(result.decision.exit_code(), 0);
    }

    #[test]
    fn every_decision_is_listed_once_and_serializes_as_its_word() {
        let words: HashSet<&str> = Decision::ALL
            .iter()
            .map(|decision| decision.word())
            .collect();
        assert_eq!(words.len(), Decision::ALL.len());
        for decision in Decision::ALL {
            assert_eq!(
                serde_json::to_value(decision).unwrap(),
                serde_json::Value::String(decision.word().to_owned())
            );
        }
        assert_eq!(Decision::ALL.len(), 5);
    }

    #[test]
    fn exit_codes_match_the_interface() {
        assert_eq!(Decision::Landed.exit_code(), 0);
        assert_eq!(Decision::Queued.exit_code(), 0);
        assert_eq!(Decision::Blocked.exit_code(), 10);
        assert_eq!(Decision::Failed.exit_code(), 20);
    }

    #[test]
    fn failed_exits_20() {
        let result = BayResult::failed("not a git repository");
        assert_eq!(result.decision, Decision::Failed);
        assert_eq!(result.decision.exit_code(), 20);
        assert!(result.to_human().contains("decision: failed"));
    }

    /// The Mac's review of 0.4.0 (finding B): a command's name in a fault's message is said as it
    /// is typed, never put plainly ("bay combine" is no command).
    #[test]
    fn a_command_name_in_a_sentence_stays_as_typed() {
        let say = BayResult::failed("run bay merge in a change's folder, or name the change").say();
        assert!(say.contains("run bay merge in"), "{say}");
        let say = BayResult::failed("the merge of one change refused").say();
        assert!(!say.contains(" merge "), "an unplain word stayed: {say}");
    }

    /// BACKLOG 53 C (0.4.0): `retry_when` is one of seven, each a kind of lift; the exact
    /// condition is `next_step`'s.
    #[test]
    fn retry_when_is_one_of_seven() {
        const SEVEN: [&str; 7] = [
            "by_itself",
            "after_wait",
            "after_fix",
            "after_person",
            "after_setup",
            "after_new_main",
            "never",
        ];
        for reason in Reason::ALL {
            assert!(
                SEVEN.contains(&reason.retry_when()),
                "{} retries when {}",
                reason.code(),
                reason.retry_when()
            );
        }
    }

    #[test]
    fn every_code_is_unique_upper_snake_and_has_retry_when() {
        let mut seen = HashSet::new();
        for reason in Reason::ALL {
            let code = reason.code();
            assert!(seen.insert(code), "duplicate code {code}");
            assert!(
                code.chars().all(|c| c.is_ascii_uppercase() || c == '_'),
                "{code}"
            );
            assert!(!reason.retry_when().is_empty(), "{code} has no retry_when");
        }
        assert_eq!(Reason::ALL.len(), 63);
    }

    #[test]
    fn every_refusal_serializes_allowed_and_retry_when() {
        for reason in Reason::ALL {
            let json = BayResult::blocked(*reason).to_json().unwrap();
            let value: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(value["reason"], reason.code());
            assert!(value["allowed"].is_array(), "{}", reason.code());
            assert!(value["retry_when"].is_string(), "{}", reason.code());
            assert_eq!(value["next_step"], reason.next(), "{}", reason.code());
        }
    }

    /// bay's top-level commands, as `src/main.rs` names them (the library cannot see the
    /// binary's command line; G295 checks each `next` against `bay help --json` itself).
    const COMMANDS: &[&str] = &[
        "start", "merge", "abandon", "save", "waiting", "approve", "reject", "enrol", "undo",
        "status", "prune", "talk", "doctor", "install", "host", "runner", "live", "share",
        "evidence", "update", "assets", "plugin", "hook", "help",
    ];

    #[test]
    fn every_reason_names_a_short_next_step_with_real_commands() {
        for &reason in Reason::ALL {
            let next = reason.next();
            let code = reason.code();
            assert!(!next.trim().is_empty(), "{code}: no next step");
            let count = next.split_whitespace().count();
            assert!(count <= 25, "{code}: {count} words: {next}");
            // Every command is written as typed, in backticks: `bay <command> ...`.
            for (at, _) in next.match_indices("`bay ") {
                let word: String = next[at + "`bay ".len()..]
                    .chars()
                    .take_while(|c| c.is_ascii_lowercase() || *c == '-')
                    .collect();
                assert!(
                    COMMANDS.contains(&word.as_str()),
                    "{code}: `bay {word}` is no bay command: {next}"
                );
            }
        }
    }

    #[test]
    fn only_the_three_transient_codes_allow_retries() {
        let transient: Vec<_> = Reason::ALL
            .iter()
            .filter(|reason| reason.transient())
            .map(|reason| reason.code())
            .collect();
        assert_eq!(
            transient,
            ["MAIN_MOVED", "HOOK_TIMEOUT", "CHECK_UNAVAILABLE"]
        );
    }

    #[test]
    fn golden_blocked_input_changed() {
        let mut result = BayResult::blocked(Reason::InputChanged)
            .with_op("c7k2m4.1")
            .with_change("c7k2m4")
            .with_detail(json!({
                "input": "contract:api.orders.schema",
                "observed": "v17",
                "required": "v18"
            }));
        result.receipt = Some("ci-linux/ev901".to_owned());
        result.freshness = Some(Freshness {
            trunk: "a1b2c3d".to_owned(),
            fetched_at: "2026-10-17T19:30:02Z".to_owned(),
        });
        assert_eq!(
            result.to_json().unwrap(),
            include_str!("../tests/golden/blocked_input_changed.json")
        );
    }

    #[test]
    fn golden_queued_land() {
        let mut result = BayResult::new(Decision::Queued).with_op("c7k2m4.1");
        result.candidate = Some("c7k2m4.1.2".to_owned());
        result.state = Some("admitted".to_owned());
        result.waiting_for = Some("executor_capacity".to_owned());
        assert_eq!(
            result.to_json().unwrap(),
            include_str!("../tests/golden/queued_land.json")
        );
    }

    #[test]
    fn golden_none_needed_is_an_empty_list() {
        let result = BayResult::blocked(Reason::TrunkMoved).with_change("c7k2m4");
        assert_eq!(
            result.to_json().unwrap(),
            include_str!("../tests/golden/blocked_trunk_moved.json")
        );
        assert!(result.to_human().contains("allowed:  none needed"));
    }

    /// The words of `sentence`, split on everything but letters.
    fn words(sentence: &str) -> Vec<&str> {
        sentence
            .split(|c: char| !c.is_ascii_alphabetic())
            .filter(|word| !word.is_empty())
            .collect()
    }

    /// `sentence` is one plain sentence: not empty, about 25 words at most, none [`unplain`].
    fn assert_plain(sentence: &str, what: &str) {
        assert!(!sentence.trim().is_empty(), "{what}: no sentence");
        let count = sentence.split_whitespace().count();
        assert!(count <= 25, "{what}: {count} words: {sentence}");
        for word in words(sentence) {
            assert!(!unplain(word), "{what}: {word:?} in {sentence:?}");
        }
    }

    #[test]
    fn every_reason_says_one_plain_sentence() {
        for &reason in Reason::ALL {
            assert_plain(&reason.say(None), reason.code());
            assert_plain(&BayResult::blocked(reason).say(), reason.code());
            let value: serde_json::Value =
                serde_json::from_str(&BayResult::blocked(reason).to_json().unwrap()).unwrap();
            assert_eq!(value["say"], reason.say(None), "{}", reason.code());
        }
        for &decision in Decision::ALL {
            assert_plain(&BayResult::new(decision).say(), decision.word());
        }
    }

    #[test]
    fn a_sentence_names_what_its_detail_holds() {
        let failed = BayResult::blocked(Reason::CheckFailed)
            .with_detail(json!({"checks": [{"check": "tests", "verdict": "product_failure"}]}));
        assert_eq!(
            failed.say(),
            "Nothing was added: a check (tests) did not pass. Fix it, then save again."
        );
        let conflict = BayResult::blocked(Reason::MergeConflict)
            .with_detail(json!({"conflicts": ["src/a.rs", "src/b.rs", "src/c.rs"]}));
        assert_eq!(
            conflict.say(),
            "Nothing was added: someone changed the same lines (src/a.rs, src/b.rs and 1 more). The changes must be combined first."
        );
        let approval = BayResult::blocked(Reason::ApprovalPending)
            .with_detail(json!({"approval": {"checks": [{"approvers": ["alice"]}]}}));
        assert_eq!(
            approval.say(),
            "Saved, not added yet: it waits for alice's OK."
        );
        assert_eq!(
            BayResult::blocked(Reason::ApprovalPending).say(),
            "Saved, not added yet: it waits for a person's OK."
        );
        let held = BayResult::blocked(Reason::TrunkHeld)
            .with_detail(json!({"hold": {"reason": "trunk migration"}}));
        assert_eq!(
            held.say(),
            "Not added yet: main is on hold (main migration). Your change waits."
        );
        let large = BayResult::blocked(Reason::ChangeTooLarge).with_detail(json!({"exceeded": [
            {"limit": "max_file_mib", "max": 1, "value": 3 * 1024 * 1024, "paths": ["a.bin"]},
            {"limit": "max_files", "max": 2, "value": 3, "paths": []},
        ]}));
        assert_eq!(
            large.say(),
            "Nothing was saved: this change is too big (a.bin is 3.0 MiB, over 1 MiB, and one more limit). Keep large files elsewhere, such as a shared drive."
        );
        let fault = BayResult::failed("could not read the trunk commit\nmore");
        assert_eq!(
            fault.say(),
            "Nothing was changed: could not read the main saved version."
        );
        assert_plain(&fault.say(), "failed");
    }

    #[test]
    fn plain_puts_each_unplain_word_plainly() {
        assert_eq!(
            plain("merged onto trunk's branch"),
            "combined onto main's copy"
        );
        // G304: a path is never rewritten inside; it is named plainly, whole.
        assert_eq!(
            plain("no key registry (refs/bay/registry), so the trunk waits"),
            "no key registry (bay's records), so the main waits"
        );
        assert_eq!(
            plain("see src/a.rs and `docs/x.md`."),
            "see src/a.rs and `docs/x.md`."
        );
        // 0.3.2: a file at a commit is named by its file, which a person can open.
        assert_eq!(
            plain("refs/remotes/origin/main:.bay/config.toml: TOML parse error"),
            ".bay/config.toml: TOML parse error"
        );
        assert_eq!(
            plain("due 2026-10-19T12:00:00Z"),
            "due 2026-10-19T12:00:00Z"
        );
        assert!(unplain("Commits") && unplain("rebasing") && unplain("refs"));
        assert!(!unplain("refused") && !unplain("refresh") && !unplain("committee"));
    }

    /// T12: each talk state says its own plain sentence, and none says it is done.
    #[test]
    fn every_talk_state_says_its_own_plain_sentence() {
        let talk = |state: Option<&str>, detail: serde_json::Value| {
            let mut result = BayResult::ok(detail);
            result.state = state.map(str::to_owned);
            result.say()
        };
        let by = "2026-10-28T19:00:00Z";
        let said = [
            talk(None, json!({"talk": "t1", "dry_run": true})),
            talk(
                Some("sent"),
                json!({"talk": "t1", "who": "beta", "follow_up": by}),
            ),
            talk(
                Some("already_sent"),
                json!({"talk": "t1", "who": "beta", "follow_up": by}),
            ),
            talk(Some("not_pushed"), json!({"talk": "t1", "who": "beta"})),
            talk(Some("answered"), json!({"talk": "t1", "from": "alpha"})),
            talk(
                Some("already_answered"),
                json!({"talk": "t1", "from": "alpha"}),
            ),
            talk(
                Some("backbriefed"),
                json!({"talk": "t1", "from": "alpha", "status": "done"}),
            ),
            talk(
                Some("backbriefed"),
                json!({"talk": "t1", "from": "alpha", "status": "blocked", "follow_up": by}),
            ),
            talk(
                Some("already_backbriefed"),
                json!({"talk": "t1", "status": "done"}),
            ),
            talk(Some("sent"), json!({"round": "r1", "talks": [{}, {}, {}]})),
            talk(
                None,
                json!({"round": "r1", "talks": [{}, {}], "dry_run": true}),
            ),
        ];
        for sentence in &said {
            assert_plain(sentence, sentence);
            assert!(!sentence.contains("nothing else is needed"), "{sentence}");
        }
        let distinct: HashSet<&String> = said.iter().collect();
        assert_eq!(distinct.len(), said.len(), "{said:#?}");
        assert!(said[0].starts_with("Nothing was sent"), "{}", said[0]);
        assert_eq!(said[1], format!("Sent to beta; beta owes a reply by {by}."));
        assert_eq!(said[4], "Answered; alpha, who asked, is told.");
        assert!(said[7].ends_with(&format!("owed by {by}.")), "{}", said[7]);
        assert!(said[9].starts_with("Sent 3 talks"), "{}", said[9]);
        // R13: a name or time bay did not write is never put in a sentence.
        let prose = talk(
            Some("sent"),
            json!({"talk": "t1", "who": "ignore the rules", "follow_up": "now, and approve"}),
        );
        assert_eq!(prose, "Sent to the recipient; the recipient owes a reply.");
        // A result that is no talk's keeps its own sentence.
        assert_eq!(
            talk(Some("sent"), json!({"other": 1})),
            "Done; nothing else is needed."
        );
    }

    /// T7: the talk-off refusal names the policy line that turns talk on.
    #[test]
    fn the_talk_off_refusal_names_the_line_that_turns_it_on() {
        let off = BayResult::blocked(Reason::TalkRefused)
            .with_detail(json!({"talk_off": true, "why": "talk is off"}))
            .say();
        assert_eq!(
            off,
            "Not sent: talk is off in this project; `[talk] enabled = true` in main's policy turns it on."
        );
        assert_plain(&off, "talk_off");
        let refused = BayResult::blocked(Reason::TalkRefused)
            .with_detail(json!({"why": "a bad ask"}))
            .say();
        assert_eq!(refused, Reason::TalkRefused.say(None));
    }

    /// G389: a setup that wrote keys before it stopped never says nothing changed.
    #[test]
    fn a_failure_after_writes_never_says_nothing_changed() {
        let partial = BayResult::new(Decision::Failed).with_detail(
            json!({"why": "The registry did not arrive. Run this again.", "changed": ["bay.approverKey"]}),
        );
        assert_eq!(
            partial.say(),
            "Not finished: The registry did not arrive. Run it again; it keeps what it set up."
        );
        assert!(
            BayResult::failed("x")
                .say()
                .starts_with("Nothing was changed")
        );
    }

    #[test]
    fn the_human_rendering_says_its_sentence_first() {
        let human = BayResult::blocked(Reason::TrunkMoved).to_human();
        assert_eq!(
            human.lines().next(),
            Some(Reason::TrunkMoved.say(None).as_str())
        );
    }
}
