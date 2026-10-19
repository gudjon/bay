//! `bay.result/1` — the one result every command returns.
//!
//! Stdout is always this value as JSON. Human text is a rendering of it on stderr.
//! Exit codes follow `INTERFACES` § 1. Reason codes are the fixed table of
//! `INTERFACES` § 2 and § 8: every code carries `allowed` and `retry_when`.

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
    /// Process exit code for this decision. Usage errors are clap's exit 2.
    #[must_use]
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Ok | Self::Landed | Self::Queued => 0,
            Self::Blocked => 10,
            Self::Failed => 20,
        }
    }

    const fn word(self) -> &'static str {
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
    QueueHeld => "QUEUE_HELD", ["edit_locally"], "reached_queue_head", false;
    QueueDeadlineExceeded => "QUEUE_DEADLINE_EXCEEDED", ["read", "repair"], "operation_changed_or_budget_raised", false;
    PublicationQueued => "PUBLICATION_QUEUED", ["edit_locally"], "reached_queue_head", false;
    CheckFailed => "CHECK_FAILED", ["repair"], "candidate_changed", false;
    CheckUnavailable => "CHECK_UNAVAILABLE", ["wait"], "capable_worker_reachable", true;
    CheckUnknown => "CHECK_UNKNOWN", ["wait", "repair"], "check_produced_result", false;
    PolicyChanged => "POLICY_CHANGED", ["refresh"], "rechecked_under_current_policy", false;
    PolicyInvalid => "POLICY_INVALID", ["repair"], "policy_repaired", false;
    WorkspaceIncomplete => "WORKSPACE_INCOMPLETE", ["resume"], "journal_ready", false;
    PublishOutcomeUnknown => "PUBLISH_OUTCOME_UNKNOWN", [], "trunk_read_again", false;
    ChangeAlreadyLanded => "CHANGE_ALREADY_LANDED", ["read"], "never", false;
    ChangeSuperseded => "CHANGE_SUPERSEDED", ["read"], "existing_change_landed_abandoned_or_past_takeover", false;
    AbandonRefused => "ABANDON_REFUSED", ["read"], "current_actor_abandons_or_adopt_rules_apply", false;
    CoverageUnknown => "COVERAGE_UNKNOWN", ["edit_locally"], "coordination_data_reachable", false;
    RemoteNotFetched => "REMOTE_NOT_FETCHED", ["read"], "remote_fetched", false;
    // INTERFACES § 8 (doctor)
    GitBelowFloor => "GIT_BELOW_FLOOR", ["read"], "git_upgraded_to_floor", false;
    ClaudeBelowFloor => "CLAUDE_BELOW_FLOOR", ["read"], "claude_upgraded_or_adapter_removed", false;
    ClaudeAbsent => "CLAUDE_ABSENT", ["read"], "claude_installed_or_adapter_removed", false;
    RemoteUnreachable => "REMOTE_UNREACHABLE", ["read"], "remote_answers_with_box_key", false;
    HooksMissing => "HOOKS_MISSING", ["read"], "install_wrote_hook_entries", false;
    SkillMismatch => "SKILL_MISMATCH", ["read"], "install_copied_skill_for_this_version", false;
    // INTERFACES § 2 (`bay update`, the binary's lifecycle in `DESIGN`)
    UpdateUnavailable => "UPDATE_UNAVAILABLE", ["read"], "release_reachable", false;
    UpdateUnverified => "UPDATE_UNVERIFIED", ["read"], "release_republished", false;
    UpdatePinned => "UPDATE_PINNED", ["read"], "pin_moved", false;
    UpdateInUse => "UPDATE_IN_USE", ["wait"], "operation_finished", false;
}

impl Reason {
    #[must_use]
    pub const fn code(self) -> &'static str {
        self.row().code
    }

    /// What is still allowed. Empty means "none needed": bay acts on its own.
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

/// Fields appear in the order of `INTERFACES` § 1. Absent fields are omitted.
#[derive(Debug, Serialize)]
pub struct BayResult {
    pub schema: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
    pub decision: Decision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed: Option<Vec<&'static str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_when: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<Freshness>,
    /// A command's own human rendering, when it has one (doctor's status line).
    #[serde(skip)]
    pub human: Option<String>,
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
        let mut lines = vec![format!("decision: {}", self.decision.word())];
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

#[cfg(test)]
mod tests {
    use super::{BayResult, Decision, Freshness, Reason, SCHEMA};
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
        assert_eq!(Reason::ALL.len(), 34);
    }

    #[test]
    fn every_refusal_serializes_allowed_and_retry_when() {
        for reason in Reason::ALL {
            let json = BayResult::blocked(*reason).to_json().unwrap();
            let value: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(value["reason"], reason.code());
            assert!(value["allowed"].is_array(), "{}", reason.code());
            assert!(value["retry_when"].is_string(), "{}", reason.code());
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
}
