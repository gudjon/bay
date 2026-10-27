//! The wake adapter (G58, G53; `INTERFACES` § 4; ruling T4 and the wake contract in
//! `plans/drafts/talk-vs-oz-say.md` § 4).
//!
//! On the recipient's machine, when it accepts a talk, bay asks the configured `[plugin.wake] command`
//! once to wake the recipient's session: `bay.wake/1` on stdin, `bay.wake-result/1` on stdout,
//! within [`WAKE_TIMEOUT`]. The answer goes on the ack as `wake: {result, reason}`, so the sender
//! sees it after a fetch. The adapter's word is recorded as the adapter's, never as bay's proof.
//!
//! - No `[plugin.wake]` table: `not_attempted`, `NO_WAKE_ADAPTER`.
//! - An exit other than 0, no valid document, or no answer within the bound: `uncertain`,
//!   `WAKE_ADAPTER_FAILED`. bay never calls `wake` twice for one talk: an ack is written once.
//!
//! `[plugin.wake] command` is a program path relative to the work tree, then its arguments, run from the
//! work tree with only `PATH` and `HOME` from bay's environment (`crate::run`).
//!
//! G307: the executor asks the same adapter once for an outcome a session must act on (a
//! landing, or a block whose next step is the author's), through the same [`ask`]: the request
//! names `cause: "outcome"` ([`outcome_request`]) where a talk's names `cause: "talk"`, and every
//! field a talk's request had before stays as it was, so a connector that reads talks still works.
//!
//! `bay plugin test wake` (`crate::adapter`) runs these same parts against a connector.

use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::Talk;
use crate::change::Actor;
use crate::repo::Repo;
use crate::result::{Decision, Reason};
use crate::run::Fault;

/// How long the wake adapter has to answer.
pub const WAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// The results an ack's `wake` may carry.
pub const RESULTS: [&str; 4] = ["woken", "skipped", "not_attempted", "uncertain"];

/// The results the adapter itself may answer.
pub const ANSWERS: [&str; 3] = ["woken", "skipped", "uncertain"];

/// The schema of the adapter's answer.
pub const RESULT_SCHEMA: &str = "bay.wake-result/1";

/// What bay records when the adapter fails, answers late, or answers no valid document.
#[must_use]
pub fn failed() -> Value {
    json!({"result": "uncertain", "reason": "WAKE_ADAPTER_FAILED"})
}

/// Asks the wake adapter to wake `me`'s session for `talk`, and returns the ack's `wake` field.
#[must_use]
pub fn wake(repo: &Repo, talk: &Talk, me: &Actor) -> Value {
    let request = request(
        &talk.id,
        me,
        &crate::live::machine(&repo.git).unwrap_or_default(),
        talk.body["urgent"].as_bool().unwrap_or(false),
        talk.body["what"].as_str().unwrap_or_default(),
    );
    asked(repo, &request, false)
}

/// One outcome the executor reached that a session must act on (G307): operation `op_id` of
/// `change` ended in `decision`, for `reason` when it was refused, and `actor` is the session
/// the change record names as the one to wake.
pub struct Outcome<'a> {
    pub change: &'a str,
    pub op_id: &'a str,
    pub decision: Decision,
    pub reason: Option<Reason>,
    pub actor: &'a Actor,
}

/// Asks the wake adapter, once, to wake the session `outcome` names, and returns the outcome
/// event's `wake` field, as [`wake`] returns a talk ack's.
#[must_use]
pub fn wake_outcome(repo: &Repo, outcome: &Outcome) -> Value {
    let machine = crate::live::machine(&repo.git).unwrap_or_default();
    asked(repo, &outcome_request(outcome, &machine), true)
}

/// Whether a `[plugin.wake] command` is configured. Without one nothing is asked.
#[must_use]
pub fn configured(repo: &Repo) -> bool {
    repo.config
        .plugin
        .wake
        .as_ref()
        .is_some_and(|wake| wake.command.is_some())
}

/// Runs the configured adapter once with `request`, and returns what bay records of its
/// answer: the one path a talk's wake and an outcome's take. An outcome's record names why a
/// failed wake failed (`with_why`); a talk's ack keeps its two fields.
fn asked(repo: &Repo, request: &Value, with_why: bool) -> Value {
    let plugin = repo.config.plugin.wake.as_ref();
    let timeout = plugin.map_or(WAKE_TIMEOUT, |plugin| plugin.timeout(WAKE_TIMEOUT));
    let deadline = Instant::now() + timeout;
    let Some(made) = plugin
        .and_then(|wake| wake.command.as_deref())
        .and_then(|command| crate::run::at_trunk(repo, "wake adapter", command, "wake", deadline))
    else {
        return json!({"result": "not_attempted", "reason": "NO_WAKE_ADAPTER"});
    };
    // A script not on main, or one bay cannot start here (a `#!` script on Windows), fails as
    // an adapter that cannot start does.
    let made = match made {
        Ok(made) => made,
        Err(why) if with_why => return failed_because(&format!("it did not start: {why}"), &[]),
        Err(_) => return failed(),
    };
    let answer = ask(made.command, request, timeout);
    if with_why {
        recorded_with_why(&answer, timeout)
    } else {
        recorded(&answer, timeout)
    }
}

/// The `bay.wake/1` request for `talk` to `me` on `machine`; `line` is cut to 200 characters.
#[must_use]
pub fn request(talk: &str, me: &Actor, machine: &str, urgent: bool, line: &str) -> Value {
    let line: String = line.chars().take(200).collect();
    json!({
        "schema": "bay.wake/1", "op": "wake", "cause": "talk", "talk": talk,
        "actor": {"name": me.name, "session": me.session},
        "machine": machine,
        "urgent": urgent,
        "line": line,
    })
}

/// The `bay.wake/1` request for `outcome`, asked from `machine` (G307): `cause: "outcome"`, the
/// change, the operation, the decision and its reason code, and the actor and session to wake,
/// with the `actor`, `machine`, `urgent` and `line` fields a talk's request has, so a connector
/// that reads only those still wakes the right session. An outcome is never urgent.
#[must_use]
pub fn outcome_request(outcome: &Outcome, machine: &str) -> Value {
    let reason = outcome.reason.map(Reason::code);
    let decision = outcome.decision.word();
    let line = match reason {
        Some(code) => format!("change {} {decision}: {code}", outcome.change),
        None => format!("change {} {decision}", outcome.change),
    };
    json!({
        "schema": "bay.wake/1", "op": "wake", "cause": "outcome",
        "change": outcome.change,
        "op_id": outcome.op_id,
        "decision": decision,
        "reason": reason,
        "actor": {"name": outcome.actor.name, "session": outcome.actor.session},
        "machine": machine,
        "urgent": false,
        "line": line,
    })
}

/// What the adapter did with one request.
#[derive(Debug)]
pub struct Answer {
    /// Its exit status; `None` when it did not start or ran past the bound.
    pub status: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub elapsed: Duration,
    /// It ran past the bound and was killed.
    pub timed_out: bool,
    /// Why there is no status.
    pub error: Option<String>,
}

/// Runs `cmd` with `request` on stdin; once `bound` has passed, its whole process group is
/// killed (`crate::run::bounded`).
#[must_use]
pub fn ask(cmd: Command, request: &Value, bound: Duration) -> Answer {
    let input = format!("{request}\n");
    let started = Instant::now();
    let ran = crate::run::captured(cmd, Some(input.as_bytes()), bound);
    let elapsed = started.elapsed();
    let (timed_out, error) = match ran {
        Ok(ran) => {
            return Answer {
                status: Some(ran.status),
                stdout: ran.stdout,
                stderr: ran.stderr,
                elapsed,
                timed_out: false,
                error: None,
            };
        }
        // `Fault::Spawn` reads "it did not start: <why>".
        Err(Fault::Timeout(_)) => (
            true,
            format!("it did not answer within {} s", bound.as_secs_f64()),
        ),
        Err(other) => (false, other.to_string()),
    };
    Answer {
        status: None,
        stdout: Vec::new(),
        stderr: Vec::new(),
        elapsed,
        timed_out,
        error: Some(error),
    }
}

/// The ack's `wake` field from the adapter's answer, timing aside; or why bay records
/// [`failed`] instead.
///
/// # Errors
///
/// The adapter did not start, exited other than 0, or printed no `bay.wake-result/1` whose
/// `result` is one of [`ANSWERS`].
pub fn answered(answer: &Answer) -> Result<Value, String> {
    if let Some(error) = &answer.error {
        return Err(error.clone());
    }
    if let Some(status) = answer.status.filter(|status| !status.success()) {
        return Err(format!("it exited with {status}"));
    }
    let document: Value = serde_json::from_slice(&answer.stdout)
        .map_err(|error| format!("its stdout is not one JSON document: {error}"))?;
    let result = document["result"].as_str().unwrap_or_default();
    if document["schema"] != RESULT_SCHEMA || !ANSWERS.contains(&result) {
        return Err(format!(
            "its document is no {RESULT_SCHEMA} whose result is {}",
            ANSWERS.join(", ")
        ));
    }
    let reason = kept_reason(document["reason"].as_str().unwrap_or_default());
    Ok(
        json!({"result": result, "reason": if reason.is_empty() { Value::Null } else { json!(reason) }}),
    )
}

/// What bay keeps of the adapter's `reason`: its upper-case letters and `_`, at most 64.
#[must_use]
pub fn kept_reason(reason: &str) -> String {
    reason
        .chars()
        .filter(|c| c.is_ascii_uppercase() || *c == '_')
        .take(64)
        .collect()
}

/// What bay records on the ack for `answer`: the adapter's answer when it came within `limit`,
/// else [`failed`].
#[must_use]
pub fn recorded(answer: &Answer, limit: Duration) -> Value {
    if answer.elapsed > limit {
        return failed();
    }
    answered(answer).unwrap_or_else(|_| failed())
}

/// [`recorded`] for an outcome's wake on an executor's log, which names why a failed wake
/// failed ([`failed_because`]). A talk's ack keeps [`recorded`]'s two fields: its shape is
/// signed and judged by every reader (`crate::talk`).
#[must_use]
pub fn recorded_with_why(answer: &Answer, limit: Duration) -> Value {
    if answer.elapsed > limit && answer.error.is_none() {
        return failed_because(
            &format!("it answered after {} s", limit.as_secs_f64()),
            &answer.stderr,
        );
    }
    answered(answer).unwrap_or_else(|why| failed_because(&why, &answer.stderr))
}

/// [`failed`], naming why (a fleet finding, 2026-10-26: 67 wakes failed in a day, each recorded
/// only as `WAKE_ADAPTER_FAILED`, and nobody could tell why from the record): bay's cause and
/// the first line the adapter wrote on stderr, at most 200 characters in all.
#[must_use]
pub fn failed_because(why: &str, stderr: &[u8]) -> Value {
    let said = String::from_utf8_lossy(stderr);
    let first = said.lines().map(str::trim).find(|line| !line.is_empty());
    let why: String = match first {
        Some(line) => format!("{why}: {line}"),
        None => why.to_owned(),
    }
    .chars()
    .take(200)
    .collect();
    let mut value = failed();
    value["why"] = json!(why);
    value
}
