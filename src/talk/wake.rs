//! The wake adapter (G58, G53; `INTERFACES` § 4; ruling T4 and the wake contract in
//! `plans/drafts/talk-vs-oz-say.md` § 4).
//!
//! On the recipient's machine, when it accepts a talk, bay asks the configured `[wake] command`
//! once to wake the recipient's session: `bay.wake/1` on stdin, `bay.wake-result/1` on stdout,
//! within [`WAKE_TIMEOUT`]. The answer goes on the ack as `wake: {result, reason}`, so the sender
//! sees it after a fetch. The adapter's word is recorded as the adapter's, never as bay's proof.
//!
//! - No `[wake]` table: `not_attempted`, `NO_WAKE_ADAPTER`.
//! - An exit other than 0, no valid document, or no answer within the bound: `uncertain`,
//!   `WAKE_ADAPTER_FAILED`. bay never calls `wake` twice for one talk: an ack is written once.

use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

use super::Talk;
use crate::change::Actor;
use crate::repo::Repo;

/// How long the wake adapter has to answer.
pub const WAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// The results an ack's `wake` may carry.
pub const RESULTS: [&str; 4] = ["woken", "skipped", "not_attempted", "uncertain"];

fn failed() -> Value {
    json!({"result": "uncertain", "reason": "WAKE_ADAPTER_FAILED"})
}

/// Asks the wake adapter to wake `me`'s session for `talk`, and returns the ack's `wake` field.
#[must_use]
pub fn wake(repo: &Repo, talk: &Talk, me: &Actor) -> Value {
    let Some(command) = repo
        .config
        .wake
        .as_ref()
        .and_then(|wake| wake.command.as_deref())
    else {
        return json!({"result": "not_attempted", "reason": "NO_WAKE_ADAPTER"});
    };
    let mut words = command.split_whitespace();
    let Some(program) = words.next() else {
        return json!({"result": "not_attempted", "reason": "NO_WAKE_ADAPTER"});
    };
    let line: String = talk.body["what"]
        .as_str()
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect();
    let request = json!({
        "schema": "bay.wake/1", "op": "wake", "talk": talk.id,
        "actor": {"name": me.name, "session": me.session},
        "machine": crate::live::machine(&repo.git).unwrap_or_default(),
        "urgent": talk.body["urgent"].as_bool().unwrap_or(false),
        "line": line,
    });
    let mut cmd = Command::new(repo.work_root.join(program));
    cmd.args(words)
        .current_dir(&repo.work_root)
        .env_clear()
        .stderr(Stdio::null());
    for name in ["PATH", "HOME"] {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    let input = format!("{request}\n");
    let Ok(output) = crate::git::run_bounded_for(cmd, "wake", Some(input.as_bytes()), WAKE_TIMEOUT)
    else {
        return failed();
    };
    if !output.status.success() {
        return failed();
    }
    let Ok(answer) = serde_json::from_slice::<Value>(&output.stdout) else {
        return failed();
    };
    let result = answer["result"].as_str().unwrap_or_default();
    if answer["schema"] != "bay.wake-result/1"
        || !["woken", "skipped", "uncertain"].contains(&result)
    {
        return failed();
    }
    let reason: String = answer["reason"]
        .as_str()
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_uppercase() || *c == '_')
        .take(64)
        .collect();
    json!({"result": result, "reason": if reason.is_empty() { Value::Null } else { json!(reason) }})
}
