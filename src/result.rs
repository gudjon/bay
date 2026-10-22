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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Ok,
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
            Self::Landed => "landed",
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
    InputChanged => "INPUT_CHANGED", ["read", "refresh", "repair"], "candidate_validated_against_required_versions", false;
    InputUnobserved => "INPUT_UNOBSERVED", ["read", "refresh"], "required_version_read", false;
    MergeConflict => "MERGE_CONFLICT", ["read", "refresh", "repair"], "conflict_paths_changed", false;
    TrunkMoved => "TRUNK_MOVED", [], "retried_once_then_queued", true;
    PublishRetryExhausted => "PUBLISH_RETRY_EXHAUSTED", ["read", "repair"], "cause_fixed", false;
    RegistryChanged => "REGISTRY_CHANGED", [], "resigned_by_valid_executor", false;
    CheckFlaky => "CHECK_FLAKY", ["repair"], "check_fixed_or_quarantined", false;
    ChangeNotLanded => "CHANGE_NOT_LANDED", ["read"], "change_landed", false;
    HookTimeout => "HOOK_TIMEOUT", [], "retried_after_short_wait", true;
    QueueHeld => "QUEUE_HELD", ["wait"], "reached_queue_head", false;
    QueueDeadlineExceeded => "QUEUE_DEADLINE_EXCEEDED", ["read", "repair"], "operation_changed_or_budget_raised", false;
    PublicationQueued => "PUBLICATION_QUEUED", ["wait"], "reached_queue_head", false;
    CheckFailed => "CHECK_FAILED", ["repair"], "candidate_changed", false;
    CheckUnavailable => "CHECK_UNAVAILABLE", ["wait"], "capable_worker_reachable", true;
    CheckUnknown => "CHECK_UNKNOWN", ["wait", "repair"], "check_produced_result", false;
    PolicyChanged => "POLICY_CHANGED", ["refresh"], "rechecked_under_current_policy", false;
    PolicyInvalid => "POLICY_INVALID", ["repair"], "policy_repaired", false;
    // The missing-policy ruling; not yet in INTERFACES § 2: no `.bay/policy.toml` at the base.
    PolicyMissing => "POLICY_MISSING", ["read"], "policy_added", false;
    WorkspaceIncomplete => "WORKSPACE_INCOMPLETE", ["refresh"], "journal_ready", false;
    PublishOutcomeUnknown => "PUBLISH_OUTCOME_UNKNOWN", [], "trunk_read_again", false;
    // G244: a post-publication check failed on a landing that touches the same paths.
    TrunkRed => "TRUNK_RED", ["read", "repair"], "trunk_green_or_change_names_the_owner", false;
    ChangeAlreadyLanded => "CHANGE_ALREADY_LANDED", ["read"], "never", false;
    ChangeSuperseded => "CHANGE_SUPERSEDED", ["read"], "existing_change_landed_abandoned_or_past_takeover", false;
    // Ruled 2026-10-19 (rulings-open-questions.md Q8): another live session of the same actor
    // name holds the change.
    ChangeHeld => "CHANGE_HELD", ["read", "wait"], "holder_session_ended_or_heartbeat_past_takeover_window", false;
    AbandonRefused => "ABANDON_REFUSED", ["read"], "current_actor_abandons_or_adopt_rules_apply", false;
    // G152; not yet in INTERFACES § 2: the policy at the base sets `[agent] require_actor` and
    // the actor's name came from a default, not `BAY_ACTOR`.
    ActorUnnamed => "ACTOR_UNNAMED", ["read"], "bay_actor_set", false;
    // G243b; not yet in INTERFACES § 2: the policy at the base sets `[agent]
    // require_actor_proof` and the agent key cannot prove which actor signs: several actors run
    // sessions under this OS account, which can read a file-based agent key.
    ActorUnproven => "ACTOR_UNPROVEN", ["read"], "actor_key_on_hardware_or_own_account", false;
    CoverageUnknown => "COVERAGE_UNKNOWN", ["wait"], "coordination_data_reachable", false;
    // Ruling R-H1 (AGENT-HOOKS-0.1 § 6.3, `context_invalid`); not yet in INTERFACES § 2: a
    // `pre-tool` payload bay cannot read is denied.
    HookInputInvalid => "HOOK_INPUT_INVALID", ["read"], "hook_payload_readable_by_this_bay", false;
    // Ruling R-H2 widened (H3, G60); not yet in INTERFACES § 2: a shell command on the deny list
    // (G39), as its `pre-tool` row's verdict reason. The same command is never allowed.
    ShellDenied => "SHELL_DENIED", ["read"], "never", false;
    // `DESIGN` § 11c, "Two sessions in one tree" (G101); not yet in INTERFACES § 2: a write of a
    // path another live session of the same tree holds, as its `pre-tool` deny.
    PathHeld => "PATH_HELD", ["read", "wait"], "holder_session_stopped_or_claim_past_deadline", false;
    RemoteNotFetched => "REMOTE_NOT_FETCHED", ["read"], "remote_fetched", false;
    // Ruling: config location (a repository with no `.bay/config.toml` anywhere bay looks);
    // not yet in INTERFACES § 2
    ConfigMissing => "CONFIG_MISSING", ["read"], "config_added", false;
    // INTERFACES § 8 (doctor)
    GitBelowFloor => "GIT_BELOW_FLOOR", ["read"], "git_upgraded_to_floor", false;
    ClaudeBelowFloor => "CLAUDE_BELOW_FLOOR", ["read"], "claude_upgraded_or_adapter_removed", false;
    ClaudeAbsent => "CLAUDE_ABSENT", ["read"], "claude_installed_or_adapter_removed", false;
    // G308; not yet in INTERFACES § 8: the Codex adapter's floor, as the Claude adapter's.
    AgentBelowFloor => "AGENT_BELOW_FLOOR", ["read"], "agent_upgraded_or_adapter_removed", false;
    AgentAbsent => "AGENT_ABSENT", ["read"], "agent_installed_or_adapter_removed", false;
    RemoteUnreachable => "REMOTE_UNREACHABLE", ["read"], "remote_answers_with_box_key", false;
    HooksMissing => "HOOKS_MISSING", ["read"], "install_wrote_hook_entries", false;
    SkillMismatch => "SKILL_MISMATCH", ["read"], "install_copied_skill_for_this_version", false;
    SandboxAbsent => "SANDBOX_ABSENT", ["read"], "sandbox_present_or_executor_sandbox_none", false;
    // `bay doctor --role executor` (G63); not yet in INTERFACES § 8
    CheckEnvUnset => "CHECK_ENV_UNSET", ["read"], "executor_sets_the_named_variables", false;
    ExecutorKeyMissing => "EXECUTOR_KEY_MISSING", ["read"], "executor_and_producer_keys_set_and_enrolled", false;
    // G145; not yet in INTERFACES § 8: a file-based release, producer, executor or registry key,
    // or a token beside it, that the account running agent sessions can read.
    KeyExposed => "KEY_EXPOSED", ["read"], "keys_on_hardware_or_outside_the_session_account", false;
    // INTERFACES § 2 (`bay update`, the binary's lifecycle in `DESIGN`)
    UpdateUnavailable => "UPDATE_UNAVAILABLE", ["read"], "release_reachable", false;
    UpdateUnverified => "UPDATE_UNVERIFIED", ["read"], "release_republished", false;
    UpdatePinned => "UPDATE_PINNED", ["read"], "pin_moved", false;
    // `DESIGN` § 11c and rulings R9, T1, T7 and T10 (`bay talk`); not yet in INTERFACES § 2
    TalkInvalid => "TALK_INVALID", ["repair"], "talk_corrected", false;
    TalkBodyConflict => "TALK_BODY_CONFLICT", ["read", "repair"], "ask_changed_or_superseded", false;
    TalkClosed => "TALK_CLOSED", ["read", "repair"], "follow_up_changed_or_superseded", false;
    TalkSupersedeRefused => "TALK_SUPERSEDE_REFUSED", ["read"], "sender_or_decides_supersedes", false;
    TalkAnswerConflict => "TALK_ANSWER_CONFLICT", ["read"], "answer_supersedes_the_standing_answer", false;
    TalkSenderNotActor => "TALK_SENDER_NOT_ACTOR", ["read"], "sent_with_the_senders_agent_key", false;
    TalkBacklog => "TALK_BACKLOG", ["read"], "an_open_talk_closed_or_superseded", false;
    // G77 (the review dossier's K6); not yet in INTERFACES § 2: `bay talk --to-change` names a
    // trunk commit that is no executor-signed bay publication, so no lander can be read from it.
    UnattributedLanding => "UNATTRIBUTED_LANDING", ["read"], "never", false;
    // G159 (owner ruling Q1); not yet in INTERFACES § 2: the host hook refuses a push of an
    // evidence notes ref whose note drops or changes a record the old note held.
    EvidenceWithdrawn => "EVIDENCE_WITHDRAWN", ["read"], "never", false;
    // NEW-5, NEW-9 (owner ruling 2026-10-20); not yet in INTERFACES § 2: the host hook refuses a
    // push that moves a promotion target to a commit that is not promotable (off its source's
    // first-parent line, not a fast-forward, a required check not passing, inside its soak, or
    // frozen without Fixes:/Reverts:). `bay status` names the promotable commit.
    PromotionRefused => "PROMOTION_REFUSED", ["read", "wait", "repair"], "pushed_commit_promotable", false;
    // NEW-4 (owner ruling 2026-10-20); not yet in INTERFACES § 2: a lane change (`bay start
    // --lane`) ran its checks against the current trunk, and never lands.
    LaneNeverLands => "LANE_NEVER_LANDS", ["read"], "never", false;
    // NEW-4; not yet in INTERFACES § 2: a change of no lane whose view reaches commits of a lane
    // ref that are not on trunk.
    LaneLeak => "LANE_LEAK", ["read", "repair"], "view_reaches_no_lane_commit", false;
    // G245 (owner ruling 2026-10-20); not yet in INTERFACES § 2: every other required check
    // passed, and an approval check the candidate's paths wake has no pass yet from an enrolled
    // approver's key for this exact tree.
    ApprovalPending => "APPROVAL_PENDING", ["wait"], "approved", false;
    // G264 (N-11); not yet in INTERFACES § 2: the policy at the base sets `[agent]
    // max_live_changes` and the actor already holds that many live changes; or (G310) it sets
    // `[limits] max_waiting_approvals` and that many live changes already wait for a yes.
    WipLimit => "WIP_LIMIT", ["read"], "a_live_change_lands_or_is_abandoned", false;
    // G265 (N-13); not yet in INTERFACES § 2: the box's `bay.minFreeMiB` floor is above the free
    // space of the filesystem a view or a materialized tree would be written to.
    DiskLow => "DISK_LOW", ["read"], "free_space_above_floor", false;
    // N-8; not yet in INTERFACES § 2: trunk is held, by a `[[freeze]]` of the trunk or the
    // hold adapter (`crate::hold`), and the change carries neither `Fixes:` nor `Reverts:`. The
    // operation stays queued, publishes nothing, and is tried again once the hold clears.
    TrunkHeld => "TRUNK_HELD", ["wait"], "hold_cleared", false;
    // N-5; not yet in INTERFACES § 2: `bay start --from <branch>` names a branch that changes
    // paths `--paths` does not declare. Nothing is created; a start whose paths cover them can.
    PathsUndeclared => "PATHS_UNDECLARED", ["read", "repair"], "paths_declared", false;
    // G273 (N-16); not yet in INTERFACES § 2: the change was started `--after` a change that has
    // not landed on trunk yet. The operation stays queued, publishes nothing, holds no queue
    // place, and is tried again once every change it names has landed.
    DependencyPending => "DEPENDENCY_PENDING", ["wait"], "dependencies_landed", false;
    // G273 (N-16); not yet in INTERFACES § 2: a change this one was started `--after` ended
    // without landing (abandoned, superseded or no effect), so this one can never publish.
    DependencyAbandoned => "DEPENDENCY_ABANDONED", ["read", "repair"], "dependency_dropped_or_restarted", false;
    // Owner ruling 2026-10-21; not yet in INTERFACES § 2: the change is over a `[limits]` size
    // limit of the policy at the base (`crate::limits`), refused at submit and at candidate
    // build, before any check runs.
    ChangeTooLarge => "CHANGE_TOO_LARGE", ["read", "repair"], "change_within_limits", false;
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
    pub published: Option<bool>,
    pub reason: Option<Reason>,
    pub change: Option<String>,
    pub detail: Option<Value>,
    pub allowed: Option<Vec<&'static str>>,
    pub retry_when: Option<&'static str>,
    pub waiting_for: Option<String>,
    /// Seconds since the operation's request, for an operation still under way (G80).
    pub elapsed_s: Option<u64>,
    /// The evidence records the operation has so far (G80).
    pub receipts: Option<Vec<String>>,
    /// The state the operation moves to next, when it is under way (G80).
    pub next: Option<&'static str>,
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
    published: Option<bool>,
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
    receipts: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    receipt: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    freshness: Option<&'a Freshness>,
}

impl Serialize for BayResult {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let say = self.say();
        Wire {
            schema: self.schema,
            op_id: self.op_id.as_deref(),
            candidate: self.candidate.as_deref(),
            decision: self.decision,
            state: self.state.as_deref(),
            published: self.published,
            reason: self.reason,
            say: &say,
            change: self.change.as_deref(),
            detail: self.detail.as_ref(),
            allowed: self.allowed.as_deref(),
            retry_when: self.retry_when,
            next_step: self.reason.map(Reason::next),
            waiting_for: self.waiting_for.as_deref(),
            elapsed_s: self.elapsed_s,
            receipts: self.receipts.as_deref(),
            next: self.next,
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
            published: None,
            reason: None,
            change: None,
            detail: None,
            allowed: None,
            retry_when: None,
            waiting_for: None,
            elapsed_s: None,
            receipts: None,
            next: None,
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
pub const PLAIN_WORDS: &[(&str, &str)] = &[
    ("branch", "copy"),
    ("commit", "saved version"),
    ("merge", "combine"),
    ("trunk", "main"),
    ("rebase", "update"),
    ("ref", "name"),
    ("view", "folder"),
];

/// Whether `word` is one of [`PLAIN_WORDS`] or an inflection of one (`commits`, `merged`,
/// `committing`), compared without case.
#[must_use]
pub fn unplain(word: &str) -> bool {
    let word = word.to_ascii_lowercase();
    PLAIN_WORDS.iter().any(|&(stem, _)| {
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
    for c in text.chars() {
        if c.is_whitespace() {
            out.push_str(&plain_token(&token));
            token.clear();
            out.push(c);
        } else {
            token.push(c);
        }
    }
    out.push_str(&plain_token(&token));
    out
}

/// One whitespace-free token of [`plain`]: a path holding an [`unplain`] word as "bay's
/// records", with the punctuation around it kept; any other token word by word.
fn plain_token(token: &str) -> String {
    let core = token.trim_matches(|c: char| "()`'\".,;:".contains(c));
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
            .find(|(stem, _)| other.starts_with(stem))
            .map_or("that", |(_, plain)| plain),
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
    if sentence.split_whitespace().count() > SHORT_WORDS {
        return None;
    }
    let short = sentence.split_whitespace().collect::<Vec<_>>().join(" ");
    let short = plain(short.trim_end_matches(['.', ':', ';', ',']));
    (!short.is_empty()).then_some(short)
}

impl Reason {
    /// The agent's next step on this reason: one imperative sentence, at most 25 words, every
    /// command in it written as typed (`` `bay land` ``). Every refusal carries it as
    /// `next_step`, and `bay help --json` lists it with each code; it is the one home of what to
    /// do on a reason. Every reason is in this match, so a new one cannot compile without one.
    #[allow(clippy::too_many_lines)]
    #[must_use]
    pub const fn next(self) -> &'static str {
        match self {
            Self::InputChanged => {
                "Read the new version `detail` names, run `bay start <change>`, adjust the view, then `bay land` again."
            }
            Self::InputUnobserved => {
                "Read the required version `detail` names, then run `bay start <change>` and `bay land` again."
            }
            Self::MergeConflict => {
                "Run `bay start <change>`, resolve the listed paths in the view, then `bay land` again."
            }
            Self::TrunkMoved => {
                "Do nothing: bay retries once more, then queues the change. `bay land --wait` shows the outcome."
            }
            Self::PublishRetryExhausted => {
                "Read the cause in `detail` and tell the user; once it is fixed, `bay land` again."
            }
            Self::RegistryChanged => {
                "Do nothing: an executor with a valid key signs it again. `bay status <change>` shows the outcome."
            }
            Self::CheckFlaky => {
                "Tell the user the check gives two answers for one tree; once it is fixed, `bay land` again. Never quarantine it yourself."
            }
            Self::ChangeNotLanded => {
                "Wait until the named change lands; `bay status <change>` shows its state. Then run the command again."
            }
            Self::HookTimeout => {
                "Do nothing: bay retries after a short wait. If it keeps happening, run `bay doctor` and tell the user."
            }
            Self::QueueHeld => {
                "Keep working: the change waits its turn in the queue. `bay land --wait` or `bay status <change>` shows the outcome."
            }
            Self::QueueDeadlineExceeded => {
                "Read `detail`, shrink or split the change in the view, then `bay land` again; or ask the user to raise the budget."
            }
            Self::PublicationQueued => {
                "Wait: the change is queued to land. `bay land --wait` or `bay status <change>` shows the outcome."
            }
            Self::CheckFailed => {
                "Read the failing check in `detail`, repair the change in the view, then `bay land` again."
            }
            Self::CheckUnavailable => {
                "Wait: bay retries once a worker can run the check. If `bay status --fleet` shows no live executor, tell the user."
            }
            Self::CheckUnknown => {
                "Wait for the check's result with `bay land --wait`; never treat it as a pass. Repair the change if the check cannot answer."
            }
            Self::PolicyChanged => {
                "Run `bay start <change>`, then `bay land` again: the change is checked under the current policy."
            }
            Self::PolicyInvalid => {
                "Tell the user `.bay/policy.toml` on main is invalid, as `detail` says; once it is repaired, `bay land` again."
            }
            Self::PolicyMissing => {
                "Tell the user main has no `.bay/policy.toml`, so nothing can land; set it up only if they ask."
            }
            Self::WorkspaceIncomplete => {
                "Run `bay start <change>` to finish the interrupted step, then go on."
            }
            Self::PublishOutcomeUnknown => {
                "Do nothing yet: bay reads main again. Never report it landed; `bay status <change>` shows the outcome."
            }
            Self::TrunkRed => {
                "Wait for the owner's fix to the red paths, or fix it yourself with `bay start <intent> --fixes <change>`."
            }
            Self::ChangeAlreadyLanded => {
                "Do not retry: the change already landed. For more edits, `bay start <intent>` opens the next change."
            }
            Self::ChangeSuperseded => {
                "Read the change that took over in `detail`; work in it, or `bay start` again once it lands or is abandoned."
            }
            Self::ChangeHeld => {
                "Another live session holds this change: wait until it ends, or work on another change. `bay status --fleet` shows the holder."
            }
            Self::AbandonRefused => {
                "Only the change's own actor may abandon it: ask that actor, or leave the change as it is."
            }
            Self::ActorUnnamed => "Set `BAY_ACTOR` to your actor name, then run the command again.",
            Self::ActorUnproven => {
                "Tell the user policy needs proof of the actor: a hardware agent key, or one OS account per actor."
            }
            Self::CoverageUnknown => {
                "Keep editing; `bay land` again once coordination data is reachable."
            }
            Self::HookInputInvalid => {
                "Tell the user this bay cannot read the agent's hook payload; `bay doctor` names the floors, and `bay update` brings a newer bay."
            }
            Self::ShellDenied => {
                "Do not run that command again; it is never allowed. Ask the user if the work truly needs it."
            }
            Self::PathHeld => {
                "Another session in this tree holds the path: wait until it stops, or edit other paths."
            }
            Self::RemoteNotFetched => {
                "Check the network and the remote, then run the command again; `bay doctor` names what is wrong."
            }
            Self::ConfigMissing => {
                "Tell the user bay is not set up here; commit `.bay/config.toml` to main only if they ask."
            }
            Self::GitBelowFloor => {
                "Tell the user to upgrade Git to the floor `detail` names, then run `bay doctor` again."
            }
            Self::ClaudeBelowFloor => {
                "Tell the user to upgrade Claude Code, or remove its adapter, then run `bay doctor` again."
            }
            Self::ClaudeAbsent => {
                "Tell the user to install Claude Code, or remove its adapter, then run `bay doctor` again."
            }
            // An agent product other than Claude Code, named in `detail.rows` (`product`).
            Self::AgentBelowFloor => {
                "Tell the user to upgrade the agent product doctor names, or remove its adapter, then run `bay doctor` again."
            }
            Self::AgentAbsent => {
                "Tell the user to install the agent product doctor names, or remove its adapter, then run `bay doctor` again."
            }
            Self::RemoteUnreachable => {
                "Check the network and this box's key for the remote, then run `bay doctor` again."
            }
            Self::HooksMissing => {
                "Run `bay install` to write the hook entries, then `bay doctor` again."
            }
            Self::SkillMismatch => {
                "Run `bay install` to copy this version's skill, then `bay doctor` again."
            }
            Self::SandboxAbsent => {
                "Tell the user to install the sandbox `detail` names, or set the executor's sandbox to none."
            }
            Self::CheckEnvUnset => {
                "Tell the user to set the variables `detail` names for the executor, then run `bay doctor --role executor` again."
            }
            Self::ExecutorKeyMissing => {
                "Tell the user to set and enroll the executor and producer keys, then run `bay doctor --role executor` again."
            }
            Self::KeyExposed => {
                "Tell the user to move the key `detail` names onto hardware, or out of the account agent sessions run as."
            }
            Self::UpdateUnavailable => "Run `bay update` again later, once a release is reachable.",
            Self::UpdateUnverified => {
                "Keep the installed bay and tell the user; run `bay update` again once the release is republished."
            }
            Self::UpdatePinned => {
                "Keep this version: `bay update` moves only after the box's pin moves."
            }
            Self::TalkInvalid => {
                "Correct the talk as `detail` says, then `bay talk` again; `bay talk --help` lists each kind's fields."
            }
            Self::TalkBodyConflict => {
                "Read the talk already sent under this id; send a changed ask with `bay talk`, or supersede it."
            }
            Self::TalkClosed => {
                "Read the closed talk; if it is still needed, send a new `bay talk` with a changed follow-up."
            }
            Self::TalkSupersedeRefused => {
                "Only the sender or the one who decides may supersede that talk: ask them, or send a new `bay talk`."
            }
            Self::TalkAnswerConflict => {
                "Read the standing answer; answer again only to supersede it, with `bay talk --answer <id> --value <v>`."
            }
            Self::TalkSenderNotActor => {
                "Send the talk as its sender, signed with that actor's own agent key."
            }
            Self::TalkBacklog => {
                "Close or supersede one of your open talks, then `bay talk` again; `bay status --fleet` lists them."
            }
            Self::UnattributedLanding => {
                "Do not retry: that commit did not land through bay, so it names no lander. Ask its author directly."
            }
            Self::EvidenceWithdrawn => {
                "Do not retry: an evidence record is never removed or changed. Push notes that keep every record."
            }
            Self::PromotionRefused => {
                "Push the commit `bay status` names promotable for that target, or wait until one is."
            }
            Self::LaneNeverLands => {
                "Read the check verdicts in `detail`; a lane change never lands. To land the work, start a change without `--lane`."
            }
            Self::LaneLeak => {
                "Redo the work without the lane commits `detail` names, or start it as a lane change: `bay start <intent> --paths <paths> --lane <lane>`."
            }
            Self::ApprovalPending => {
                "Tell the user who can approve (`detail.approval`); the approver runs `bay approve <change>`. Then the executor lands it, or `bay land --wait`."
            }
            Self::WipLimit => {
                "Land or abandon one of the changes `detail` names (`live_changes`, or `review.oldest`), then run the command again."
            }
            Self::DiskLow => {
                "Tell the user to free disk space on this box, or lower `bay.minFreeMiB`, then run the command again."
            }
            Self::TrunkHeld => {
                "Wait: the change stays queued and lands once the hold clears; `bay status` shows the hold. Only fixes and reverts land now."
            }
            Self::PathsUndeclared => {
                "Run `bay start <intent> --from <branch>` again, with `--paths` covering every path in `detail.uncovered`."
            }
            Self::DependencyPending => {
                "Wait: the change lands after each change it names with `--after` lands; `bay status <change>` shows them."
            }
            Self::DependencyAbandoned => {
                "Run `bay land --abandon`, then `bay start <intent> --new` without that `--after`; or restart the dropped change."
            }
            Self::ChangeTooLarge => {
                "Keep large files elsewhere or split the change, then `bay land` again. Never raise the limits to get it through."
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
            | Self::ChangeTooLarge
            | Self::WipLimit => return None,
            Self::TrunkMoved => {
                "Main moved on while saving; bay tries once more on its own, nothing to do."
            }
            Self::PublishRetryExhausted => {
                "Adding the change kept failing, so nothing was added; ask whoever runs bay here to look."
            }
            Self::RegistryChanged => {
                "The list of trusted keys changed while saving; bay tries again once it is signed, nothing to do."
            }
            Self::ChangeNotLanded => "That change is not on main yet; wait until it is added.",
            Self::HookTimeout => {
                "bay took too long to answer and tries again shortly; nothing to do."
            }
            Self::QueueHeld => {
                "Saved; it waits in line behind other changes, so keep working meanwhile."
            }
            Self::QueueDeadlineExceeded => {
                "This change waited in line too long and was stopped; make it smaller or ask for more time."
            }
            Self::PublicationQueued => {
                "Saved; it is in line and will be checked and added shortly."
            }
            Self::CheckUnavailable => {
                "Saved; it waits for a machine that can run its checks, nothing to do."
            }
            Self::PolicyChanged => {
                "The rules changed while this was checked; it is checked again under the new rules."
            }
            Self::PolicyInvalid => {
                "The project's rules file (.bay/policy.toml) has a mistake; nothing is added until it is fixed."
            }
            Self::PolicyMissing => {
                "This project has no rules file (.bay/policy.toml) yet, so nothing can be added; ask whoever set it up."
            }
            Self::WorkspaceIncomplete => {
                "An earlier step was interrupted; resume it before going on."
            }
            Self::PublishOutcomeUnknown => {
                "bay cannot tell yet whether the change was added; it looks again, so do not report it as saved."
            }
            Self::ChangeAlreadyLanded => {
                "This change is already on main; start a new one for more edits."
            }
            Self::ChangeSuperseded => {
                "Someone else's change took over this work; read it before going on."
            }
            Self::ChangeHeld => {
                "Another session is working on this change right now; wait for it to finish."
            }
            Self::AbandonRefused => "Only the person who owns this change can give it up.",
            Self::ActorUnnamed => {
                "bay needs to know who you are; set your name in BAY_ACTOR, then try again."
            }
            Self::ActorUnproven => {
                "bay cannot prove who is saving on this computer; ask for a separate account or a hardware key."
            }
            Self::CoverageUnknown => {
                "bay cannot reach its shared notes right now; keep working and save again later."
            }
            Self::HookInputInvalid => {
                "bay could not read what the assistant asked to do, so it stopped it."
            }
            Self::ShellDenied => {
                "That command could lose work, so bay stopped it; it is never allowed."
            }
            Self::RemoteNotFetched => {
                "bay could not get the latest main; check the network, then try again."
            }
            Self::ConfigMissing => {
                "bay is not set up in this project yet; ask whoever runs it, or set it up."
            }
            Self::GitBelowFloor => {
                "The Git program on this computer is too old; update it, then try again."
            }
            Self::ClaudeBelowFloor => "Claude Code on this computer is too old for bay; update it.",
            Self::ClaudeAbsent => {
                "Claude Code is not installed here, but bay expects it; install it."
            }
            Self::AgentBelowFloor => {
                "The coding assistant on this computer is too old for bay; update it."
            }
            Self::AgentAbsent => {
                "The coding assistant bay expects is not installed here; install it."
            }
            Self::RemoteUnreachable => {
                "bay cannot reach main from here; check the network and the access key."
            }
            Self::HooksMissing => "bay's safety settings are missing here; run bay install.",
            Self::SkillMismatch => "The bay guide installed here is out of date; run bay install.",
            Self::SandboxAbsent => {
                "The safe space bay runs checks in is missing on this computer; ask whoever runs bay."
            }
            Self::CheckEnvUnset => {
                "This checking machine lacks settings its checks need; ask whoever runs it."
            }
            Self::ExecutorKeyMissing => {
                "This checking machine has no key to sign with; ask whoever runs it."
            }
            Self::KeyExposed => {
                "A secret key here can be read by the assistant's account; ask whoever runs bay to move it."
            }
            Self::UpdateUnavailable => "No bay update could be reached right now; try again later.",
            Self::UpdateUnverified => {
                "The bay update could not be verified, so it was not installed."
            }
            Self::UpdatePinned => {
                "This project keeps bay at a fixed version, so it was not updated."
            }
            Self::TalkInvalid => "That message is missing something; fix it and send it again.",
            Self::TalkBodyConflict => {
                "A different message with the same name was already sent; read it first."
            }
            Self::TalkClosed => "That conversation is already closed; start a new one if needed.",
            Self::TalkSupersedeRefused => "Only the sender can replace that message.",
            Self::TalkAnswerConflict => {
                "That question already has a different answer; read it first."
            }
            Self::TalkSenderNotActor => "That message must be sent with the sender's own key.",
            Self::TalkBacklog => {
                "Too many conversations are open; close one before starting another."
            }
            Self::UnattributedLanding => {
                "That change did not come through bay, so bay cannot say who made it."
            }
            Self::EvidenceWithdrawn => "A saved record of checks can never be removed or changed.",
            Self::PromotionRefused => {
                "That version is not ready for the next stage yet; use the one bay status names, or wait."
            }
            Self::LaneNeverLands => {
                "This was a try-out: its checks ran, and it is never added to main."
            }
            Self::DiskLow => {
                "This computer is almost out of disk space; free some, then try again."
            }
            Self::DependencyPending => "Saved; it waits until the change it depends on is added.",
            Self::DependencyAbandoned => {
                "A change this one depends on was dropped, so this one cannot be added; start it again without it."
            }
        })
    }

    /// One plain sentence for a person who does not know Git (owner ruling 2026-10-21): what
    /// happened and the one next step, with what `detail` names when it is there.
    #[must_use]
    pub fn say(self, detail: Option<&Value>) -> String {
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
                "Something this change relies on was updated{}; read the new version, adjust, then save again.",
                input()
            ),
            Self::InputUnobserved => format!(
                "This change must first read the latest version of something it relies on{}, then save again.",
                input()
            ),
            Self::MergeConflict => format!(
                "Someone changed the same lines{}; the changes must be combined first.",
                paths()
            ),
            Self::CheckFlaky => format!(
                "A check{} gave two answers for the same change; ask a developer to fix that check.",
                check()
            ),
            Self::CheckFailed => format!(
                "A check failed{}; nothing was added. Fix it and save again.",
                check()
            ),
            Self::CheckUnknown => format!(
                "A check gave no clear answer{}; nothing was added yet, so wait for it.",
                check()
            ),
            Self::TrunkRed => format!(
                "Main is broken on these files{}; wait for the fix, or help fix it.",
                paths()
            ),
            Self::PathHeld => format!(
                "Another session is editing these files{}; wait for it to finish.",
                paths()
            ),
            Self::LaneLeak => format!(
                "This change holds try-out work that must not be added{}; redo it without that work.",
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
                format!("Saved, but it waits for {who}{because}.")
            }
            Self::TrunkHeld => format!(
                "Main is on hold{}; your change waits.",
                named(text_at(detail, &["/hold/reason"]).map(plain))
            ),
            Self::PathsUndeclared => format!(
                "The work touches files it did not list{}; start again listing them.",
                paths()
            ),
            Self::ChangeTooLarge => format!(
                "This change is too big to save here{}; keep large files elsewhere, such as a shared drive.",
                named(too_large(detail))
            ),
            // G310: a full review queue is named apart from the actor's own live changes.
            Self::WipLimit if detail.is_some_and(|detail| detail.get("review").is_some()) => {
                "Too many changes already wait for a person's OK; answer or finish those first, then save again."
                    .to_owned()
            }
            Self::WipLimit => {
                "You already have the most open changes allowed; finish or give one up, then start again."
                    .to_owned()
            }
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
            Decision::Queued => {
                "Saved; it is in line and will be checked and added shortly.".to_owned()
            }
            Decision::Ok => ok_say(self).to_owned(),
            Decision::Blocked => {
                "Not done yet; read the detail for what is needed, then try again.".to_owned()
            }
            Decision::Failed => match short(detail) {
                Some(short) => format!("Something went wrong: {short}. Nothing was changed."),
                None => "Something went wrong. Nothing was changed.".to_owned(),
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
        assert_eq!(Reason::ALL.len(), 68);
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
        "start", "land", "abandon", "save", "waiting", "approve", "enrol", "undo", "status",
        "prune", "watch", "talk", "doctor", "install", "host", "executor", "live", "share",
        "evidence", "update", "adapter", "hook", "help",
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
            ["TRUNK_MOVED", "HOOK_TIMEOUT", "CHECK_UNAVAILABLE"]
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
        result.published = Some(false);
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
            "A check failed (tests); nothing was added. Fix it and save again."
        );
        let conflict = BayResult::blocked(Reason::MergeConflict)
            .with_detail(json!({"conflicts": ["src/a.rs", "src/b.rs", "src/c.rs"]}));
        assert_eq!(
            conflict.say(),
            "Someone changed the same lines (src/a.rs, src/b.rs and 1 more); the changes must be combined first."
        );
        let approval = BayResult::blocked(Reason::ApprovalPending)
            .with_detail(json!({"approval": {"checks": [{"approvers": ["alice"]}]}}));
        assert_eq!(approval.say(), "Saved, but it waits for alice's OK.");
        assert_eq!(
            BayResult::blocked(Reason::ApprovalPending).say(),
            "Saved, but it waits for a person's OK."
        );
        let held = BayResult::blocked(Reason::TrunkHeld)
            .with_detail(json!({"hold": {"reason": "trunk migration"}}));
        assert_eq!(
            held.say(),
            "Main is on hold (main migration); your change waits."
        );
        let large = BayResult::blocked(Reason::ChangeTooLarge).with_detail(json!({"exceeded": [
            {"limit": "max_file_mib", "max": 1, "value": 3 * 1024 * 1024, "paths": ["a.bin"]},
            {"limit": "max_files", "max": 2, "value": 3, "paths": []},
        ]}));
        assert_eq!(
            large.say(),
            "This change is too big to save here (a.bin is 3.0 MiB, over 1 MiB, and one more limit); keep large files elsewhere, such as a shared drive."
        );
        let fault = BayResult::failed("could not read the trunk commit\nmore");
        assert_eq!(
            fault.say(),
            "Something went wrong: could not read the main saved version. Nothing was changed."
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
        assert!(unplain("Commits") && unplain("rebasing") && unplain("refs"));
        assert!(!unplain("refused") && !unplain("refresh") && !unplain("committee"));
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
