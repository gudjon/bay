//! 0.6.3 (a fleet finding, 2026-10-29: a red post check held every change on its paths, and
//! nothing told the session whose merge first failed it; the fix waited until someone read
//! `bay status`): the change that owns a red post check is told so, once.
//!
//! Each runner pass, after it reads the remote's post records, writes one `red_owner` event per
//! red check of main to its log, addressed to the change [`Red::fixes`] names: the culprit when
//! the evidence pins one, else the first failure, which stays the same while the red stands. The
//! event's id is a digest of the check, its environment and that change, so one red is one event
//! however many runners see it, and a log that holds it is never written again. A new event asks
//! the wake adapter once to wake that change's session (`cause: "red"`), and is told to the
//! notify adapter; the session's hook tells it as a notice line (`crate::notice`). The event's
//! `line` names the check, where it failed and the exact fix to run. A red a fix already answers
//! (its run not passed yet) tells no one, and a red whose failing landing names no change has no
//! one to tell.

use serde_json::{Value, json};

use crate::canonical::digest;
use crate::change::{Chain, ChangeRecord};
use crate::red::Red;
use crate::registry::Registry;
use crate::repo::Repo;
use crate::signing::Keys;

/// The event kind.
pub const KIND: &str = "red_owner";

/// Writes the `red_owner` event of each red check of `trunk` that no log holds yet. Returns one
/// note per event written, or one note naming the fault, for the pass's notes.
#[must_use]
pub fn tell(repo: &Repo, trunk: &str) -> Vec<Value> {
    told(repo, trunk).unwrap_or_else(|error| vec![json!({"red_owner": null, "error": error})])
}

fn told(repo: &Repo, trunk: &str) -> Result<Vec<Value>, String> {
    let git = &repo.git;
    let registry = Registry::current(git)?;
    let reds = crate::red::judged(git, trunk, registry.as_ref())?.red;
    if reds.is_empty() {
        return Ok(Vec::new());
    }
    crate::log::fetch(repo);
    let held = crate::log::held_events(git)?;
    let keys = Keys::load(git);
    // N-21: executor logs are signed only where the registry signs everything.
    let signer = match registry.filter(Registry::signs_all) {
        None => None,
        Some(_) => Some(keys.executor.clone().ok_or(
            "the repository has a key registry and this machine has no runner key (git config bay.runnerKey)",
        )?),
    };
    let chain = Chain::new(git);
    let mut notes = Vec::new();
    for red in &reds {
        let Some(owner) = red.fixes() else {
            continue;
        };
        let id = event_id(red, owner)?;
        if held.contains(&id) {
            continue;
        }
        let Ok((_, record)) = chain.read(owner) else {
            continue;
        };
        let mut event = event(&id, red, &record);
        if crate::talk::wake::configured(repo) {
            event["wake"] = crate::talk::wake::wake_red(
                repo,
                &crate::talk::wake::RedOwner {
                    change: &record.id,
                    check: &red.check,
                    event: &id,
                    actor: &record.actor,
                    line: event["line"].as_str().unwrap_or_default(),
                },
            );
        }
        crate::log::append(repo, &keys.executor_name, signer.as_ref(), &event)?;
        let _ = crate::notify::notify(repo, &event);
        notes.push(json!({"red_owner": owner, "check": red.check, "event": id}));
    }
    Ok(notes)
}

/// The id of the `red_owner` event of `red`, owned by `owner`: one red, one event.
fn event_id(red: &Red, owner: &str) -> Result<String, String> {
    let id = digest(&json!({
        "kind": KIND,
        "check": red.check,
        "environment": red.environment,
        "owner": owner,
    }))?;
    Ok(format!("evt_{}", &id.trim_start_matches("sha256:")[..20]))
}

/// The `red_owner` event `id` of `red`, for `record`, the change that owns it.
fn event(id: &str, red: &Red, record: &ChangeRecord) -> Value {
    json!({
        "schema": crate::log::EVENT_SCHEMA,
        "id": id,
        "at": crate::clock::now_rfc3339(),
        "kind": KIND,
        "change": record.id,
        "actor": record.actor,
        "check": red.check,
        "environment": red.environment,
        "landing": red.landing,
        "culprit": red.culprit().is_some(),
        "fix": fix_command(record),
        "line": line(red, record),
        "detail": red.to_json(),
        "recurrence_key": format!("{KIND}/{}/{}", red.check, red.environment),
    })
}

/// The command that opens the fix: a new change of the owner's intent that names it.
fn fix_command(record: &ChangeRecord) -> String {
    format!(
        "bay start {} --fixes {}",
        record.intent.reference, record.id
    )
}

/// The one sentence the owner's session is told: the check, whether the evidence proves the
/// change broke it or only saw it failing there, what the red holds, and the fix to run.
fn line(red: &Red, record: &ChangeRecord) -> String {
    let named = crate::change::shown(&record.intent.reference, &record.id);
    let how = if red.culprit().is_some() {
        "broke it: the check passed on the merge before it and failed on it"
    } else {
        "is where it was first seen failing, not a proven cause"
    };
    format!(
        "main is red on {}; your change {named} {how}. Changes on its paths wait. Fix it: {}",
        red.check,
        fix_command(record)
    )
}
