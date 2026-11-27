//! Item 98 (a fleet finding, 2026-11-25: checks moved from before the merge to after it, and a
//! red main then stood until the session whose change broke it came back, holding every change
//! on the check's paths): a runner undoes a red main's culprit on its own when no fix comes in
//! time. Opt-in: `[red] undo_after_s`, read at main; off when absent.
//!
//! Each runner pass, after it tells the owners of main's red post checks (`crate::red_owner`),
//! the runner that tells a red ([`crate::red_owner::teller`]) looks at each red whose evidence
//! pins the change that broke it: its own answer a product failure, never flaky or unknown, and
//! no fix landed for it (a red with a pending fix is not red). It undoes that change when all of
//! these hold:
//!
//! - its session was told it broke the check (its `red_owner` event) at least `undo_after_s`
//!   ago;
//! - no change in line answers the red (its `Fixes:`, `Reverts:` or `Supersedes:` names it,
//!   [`Red::answered_by`]): one with an operation that waits for a runner, submitted or
//!   queued; and no undo of it is claimed;
//! - it is not an undo itself: no undo is ever undone automatically;
//! - main has not moved since the pass read it: the remote's main, read once a pass before the
//!   first undo. A fix the pass itself merged, or another runner's, is not in the pass's read of
//!   main and answers the red; the decision waits for the next pass;
//! - its revert merges onto main's tip without a conflict, and still changes main.
//!
//! The undo is `bay undo <culprit>`, run as a child process under the runner's name
//! (`BAY_ACTOR`): an ordinary change, checked like any other and merged by the line, ahead of
//! the changes the red holds, as every undo of a red is. One decision per culprit, ever: its
//! `red_undo` event, whose id is a digest of the check, its environment and the culprit, names
//! the undo (`undo`) or why there is none (`reason`: `MERGE_CONFLICT` with the `conflicts`), and
//! a log that holds it is never written again. The event wakes the culprit's session once
//! (`cause: "red"`, with `undo`), is told to the notify adapter, and the session's hook tells it
//! as a notice line (`crate::notice`).
//!
//! A repository whose key registry signs every request opens no automatic undo: a runner key
//! cannot sign a change's requests, and the pass's note says so.

use std::collections::BTreeSet;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::canonical::digest;
use crate::change::{Chain, ChangeRecord};
use crate::merge::{Merge, revert_onto};
use crate::red::Red;
use crate::registry::Registry;
use crate::repo::Repo;
use crate::result::Reason;
use crate::signing::Keys;

/// The event kind.
pub const KIND: &str = "red_undo";

/// How long the child `bay undo` may run: it opens the undo's view and submits it to the line;
/// the line merges it.
const UNDO_BOUND: Duration = Duration::from_secs(300);

/// The verdict of a culprit's own run that an automatic undo needs.
const PRODUCT_FAILURE: &str = "product_failure";

/// Opens the undo of each red's culprit that is due, when `[red] undo_after_s` is set, and
/// writes one `red_undo` event per decision. `records` are the changes this pass read. Returns
/// one note per decision, or one note naming the fault, for the pass's notes.
#[must_use]
pub fn undo(repo: &Repo, trunk: &str, records: &[ChangeRecord]) -> Vec<Value> {
    let Some(after) = repo.config.red_undo_after() else {
        return Vec::new();
    };
    undone(repo, trunk, records, after)
        .unwrap_or_else(|error| vec![json!({"red_undo": null, "error": error})])
}

/// G930: whether the remote's logs hold event `id`, fetched into `fresh` once a pass.
fn held_now(repo: &Repo, fresh: &mut Option<BTreeSet<String>>, id: &str) -> Result<bool, String> {
    if fresh.is_none() {
        crate::log::fetch(repo);
        *fresh = Some(crate::log::held_events(&repo.git)?);
    }
    Ok(fresh.as_ref().is_some_and(|held| held.contains(id)))
}

/// Whether main, fetched again now, still stands at `trunk`.
fn main_stands(repo: &Repo, trunk: &str) -> Result<bool, String> {
    let now = repo.refetch_trunk().map_err(|error| error.message())?;
    Ok(now.trim() == trunk)
}

fn undone(
    repo: &Repo,
    trunk: &str,
    records: &[ChangeRecord],
    after: Duration,
) -> Result<Vec<Value>, String> {
    let git = &repo.git;
    let registry = Registry::current(git)?;
    let reds = crate::red::judged(git, trunk, registry.as_ref())?.red;
    let due: Vec<(&Red, &str)> = reds
        .iter()
        .filter_map(|red| pinned(red).map(|culprit| (red, culprit)))
        .collect();
    if due.is_empty() {
        return Ok(Vec::new());
    }
    if registry.as_ref().is_some_and(Registry::signs_all) {
        return Ok(vec![json!({
            "red_undo": null,
            "error": "the key registry signs every request and a runner key cannot sign a \
                      change's requests, so bay opens no automatic undo",
        })]);
    }
    let held = crate::log::held_events(git)?;
    let keys = Keys::load(git);
    let present = crate::presence::members(git, repo.config.lease());
    let chain = Chain::new(git);
    let line = in_line(records);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let mut notes = Vec::new();
    let mut seen = BTreeSet::new();
    let mut read_main = false;
    // G930: the remote's logs, read once a pass before the first decision; the tell no longer
    // reads them on every pass while a red stands.
    let mut fresh: Option<BTreeSet<String>> = None;
    for (red, culprit) in due {
        if crate::red_owner::teller(red, &keys.executor_name, &present) != keys.executor_name
            || !seen.insert(culprit)
        {
            continue;
        }
        let id = event_id(red, culprit)?;
        if held.contains(&id) {
            continue;
        }
        let Ok((_, record)) = chain.read(culprit) else {
            continue;
        };
        // An undo is never undone automatically.
        if record.reverts.is_some() {
            continue;
        }
        let Some(told_at) = told_at(repo, red, culprit)? else {
            continue;
        };
        if now < told_at.saturating_add(after.as_secs())
            || line.iter().any(|names| red.answered_by(names))
            || crate::start::claimed(repo, &format!("undo of {culprit}"), "1")?
        {
            continue;
        }
        if held_now(repo, &mut fresh, &id)? {
            continue;
        }
        // Main where the remote holds it now, read once a pass before the first undo: a landing
        // this pass made, or another runner's, is not in the pass's read of main, and a fix among
        // them answers the red. While main moved, the decision waits for the next pass.
        if !read_main {
            read_main = true;
            if !main_stands(repo, trunk)? {
                break;
            }
        }
        let Some(commit) = crate::undo::landed_commit(repo, culprit, trunk)? else {
            continue;
        };
        let undone = match revert_onto(git, trunk, &commit).map_err(|error| error.message())? {
            Merge::Conflict(conflicts, _) => Undone::Refused {
                reason: Reason::MergeConflict.code().to_owned(),
                conflicts,
            },
            Merge::Clean(tree) if tree == crate::throughput::tree_of(git, trunk)? => continue,
            Merge::Clean(_) => match opened(repo, &keys.executor_name, culprit) {
                Ok(undone) => undone,
                Err(error) => {
                    notes.push(json!({"red_undo": culprit, "check": red.check, "error": error}));
                    continue;
                }
            },
        };
        let told = said(red, &record, &undone, after);
        let event = told_one(
            repo,
            &keys,
            red,
            &record,
            event(&id, red, &record, &undone, &told),
            &told,
        )?;
        notes.push(json!({
            "red_undo": culprit,
            "check": red.check,
            "event": id,
            "undo": event["undo"],
            "reason": event.get("reason"),
        }));
    }
    Ok(notes)
}

/// BACKLOG 322: names `red`'s automatic undo in its `bay status` row (`detail.red[]`). `undo`
/// holds the `culprit` and the undo `change` a runner opened, or the `reason` it opened none, or
/// `due_at`, when a runner undoes it unless a fix comes first (`null` until its owner is told);
/// the row's `say` adds the same in one sentence. A row whose red no automatic undo takes (no
/// `[red] undo_after_s`, no pinned culprit, or a culprit that is itself an undo) is unchanged.
pub fn name_in(repo: &Repo, red: &Red, row: &mut Value) {
    let named = match shown(repo, red) {
        Ok(Some(named)) => named,
        Ok(None) => return,
        Err(why) => {
            row["undo"] = json!({"unread": why});
            return;
        }
    };
    let sentence = match (
        named["change"].as_str(),
        named["reason"].as_str(),
        named["due_at"].as_str(),
    ) {
        (Some(undo), _, _) => {
            format!("bay opened its undo, change {undo}, which goes first in line.")
        }
        (None, Some(reason), _) => format!("bay did not undo it: {reason}."),
        (None, None, Some(due)) => {
            format!("Unless a fix comes first, a runner undoes it at {due}.")
        }
        (None, None, None) => format!(
            "Unless a fix comes first, a runner undoes it {} after its owner is told.",
            waited(Duration::from_secs(
                named["after_s"].as_u64().unwrap_or_default()
            ))
        ),
    };
    if let Some(say) = row["say"].as_str() {
        row["say"] = json!(format!("{say} {sentence}"));
    }
    row["undo"] = named;
}

/// What [`name_in`] names: `None` when no automatic undo takes `red`.
fn shown(repo: &Repo, red: &Red) -> Result<Option<Value>, String> {
    let Some(after) = repo.config.red_undo_after() else {
        return Ok(None);
    };
    let Some(culprit) = pinned(red) else {
        return Ok(None);
    };
    if let Some(event) = crate::log::event(&repo.git, &event_id(red, culprit)?)? {
        return Ok(Some(json!({
            "culprit": culprit,
            "change": event["undo"],
            "reason": event.get("reason"),
        })));
    }
    // An undo is never undone automatically.
    if Chain::new(&repo.git)
        .read(culprit)
        .is_ok_and(|(_, record)| record.reverts.is_some())
    {
        return Ok(None);
    }
    let due = told_at(repo, red, culprit)?
        .map(|at| crate::clock::rfc3339_of(at.saturating_add(after.as_secs())));
    Ok(Some(json!({
        "culprit": culprit,
        "change": null,
        "due_at": due,
        "after_s": after.as_secs(),
    })))
}

/// What the runner did about a culprit.
enum Undone {
    /// It opened the undo, this change, and the line has it.
    Opened(String),
    /// It opened no undo, for `reason`; `conflicts` are the paths a conflict names.
    Refused {
        reason: String,
        conflicts: Vec<String>,
    },
}

/// The change `red`'s evidence pins as the one that broke it, when its own runs answered only
/// `product_failure` and no fix landed for the red: never one a flaky or unknown run leaves, nor
/// a landing that names no change.
fn pinned(red: &Red) -> Option<&str> {
    if red.fix.is_some() {
        return None;
    }
    let culprit = red.culprit()?;
    let change = culprit.mark.change.as_deref()?;
    let mut own = culprit
        .tested
        .iter()
        .filter(|tested| tested.mark.landing == culprit.mark.landing)
        .peekable();
    (own.peek().is_some() && own.all(|tested| tested.verdict == PRODUCT_FAILURE)).then_some(change)
}

/// When `culprit`'s session was told it broke `red`'s check: its `red_owner` event's `at`, in
/// seconds since the epoch; `None` before it was told.
fn told_at(repo: &Repo, red: &Red, culprit: &str) -> Result<Option<u64>, String> {
    let told = crate::red_owner::culprit_event(red, culprit)?;
    Ok(crate::log::event(&repo.git, &told)?
        .and_then(|event| event["at"].as_str().and_then(crate::clock::unix_of)))
}

/// The names each change in line gives (`Fixes:`, `Reverts:`, `Supersedes:`): each of
/// `records` with an operation that waits for a runner, one submitted and not yet admitted to
/// the queue included.
fn in_line(records: &[ChangeRecord]) -> Vec<Vec<&String>> {
    records
        .iter()
        .filter(|record| record.operations.iter().any(|op| record.waits(op)))
        .map(|record| {
            [&record.fixes, &record.reverts, &record.supersedes]
                .into_iter()
                .flatten()
                .collect()
        })
        .collect()
}

/// The id of the `red_undo` event of `red`'s culprit `culprit`: one decision per culprit.
fn event_id(red: &Red, culprit: &str) -> Result<String, String> {
    let id = digest(&json!({
        "kind": KIND,
        "check": red.check,
        "environment": red.environment,
        "culprit": culprit,
    }))?;
    Ok(format!("evt_{}", &id.trim_start_matches("sha256:")[..20]))
}

/// Runs `bay undo <culprit>` in the runner's checkout, under the runner's name, and reads what
/// it did. A change it opened and could not submit is given up, and named in the event.
fn opened(repo: &Repo, runner: &str, culprit: &str) -> Result<Undone, String> {
    let result = bay(repo, runner, &["undo", culprit])?;
    let change = result["change"]
        .as_str()
        .filter(|change| crate::ids::is_change_id(change))
        .map(str::to_owned);
    match (result["decision"].as_str(), change) {
        (Some("queued" | "merged"), Some(change)) => Ok(Undone::Opened(change)),
        (_, Some(change)) => {
            let reason = result["reason"].as_str().unwrap_or("unknown").to_owned();
            let why = format!("the automatic undo of {culprit} answered {reason}");
            let _ = bay(repo, runner, &["abandon", &change, "--why", &why]);
            let conflicts = result["detail"]["conflicts"]
                .as_array()
                .map(|paths| {
                    paths
                        .iter()
                        .filter_map(|path| path.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            Ok(Undone::Refused { reason, conflicts })
        }
        (decision, None) => Err(format!(
            "bay undo {culprit} opened no change: {}",
            decision.unwrap_or("no decision")
        )),
    }
}

/// Runs this bay with `args` in the runner's checkout as the runner (`BAY_ACTOR`), and reads its
/// `bay.result/1` document.
fn bay(repo: &Repo, runner: &str, args: &[&str]) -> Result<Value, String> {
    let program =
        std::env::current_exe().map_err(|error| format!("bay cannot find its program: {error}"))?;
    let mut cmd = Command::new(program);
    cmd.arg("-C")
        .arg(&repo.work_root)
        .arg("--json")
        .args(args)
        .env("BAY_ACTOR", runner)
        .env("BAY_SESSION", format!("{runner}-red-undo"));
    let ran = crate::run::captured(cmd, None, UNDO_BOUND)
        .map_err(|fault| format!("bay {}: {fault}", args.join(" ")))?;
    serde_json::from_slice(&ran.stdout)
        .map_err(|error| format!("bay {} answered no result: {error}", args.join(" ")))
}

/// The `red_undo` event `id` of `red`, for `record`, its culprit's change; `said` and `fix` are
/// what its session is told ([`said`]).
fn event(
    id: &str,
    red: &Red,
    record: &ChangeRecord,
    undone: &Undone,
    (said, fix): &(String, String),
) -> Value {
    let mut event = json!({
        "schema": crate::log::EVENT_SCHEMA,
        "id": id,
        "at": crate::clock::now_rfc3339(),
        "kind": KIND,
        "change": record.id,
        "actor": record.actor,
        "check": red.check,
        "environment": red.environment,
        "landing": red.landing,
        "culprit": true,
        "undo": null,
        "fix": fix,
        "line": crate::red_owner::with_fix(said, fix),
        "detail": red.to_json(),
        "recurrence_key": format!("{KIND}/{}/{}", red.check, red.environment),
    });
    match undone {
        Undone::Opened(undo) => event["undo"] = json!(undo),
        Undone::Refused { reason, conflicts } => {
            event["reason"] = json!(reason);
            if !conflicts.is_empty() {
                event["conflicts"] = json!(conflicts);
            }
        }
    }
    event
}

/// What the culprit's session is told, and the command it runs next.
fn said(red: &Red, record: &ChangeRecord, undone: &Undone, after: Duration) -> (String, String) {
    let named = crate::change::shown(&record.intent.reference, &record.id);
    let waited = waited(after);
    match undone {
        Undone::Opened(undo) => (
            format!(
                "main is red on {}; your change {named} broke it and no fix came in {waited}, so \
                 bay opened its undo, change {undo}, which goes first in line. Merge your work \
                 again with the fix in a new change.",
                red.check
            ),
            format!("bay start {}", record.intent.reference),
        ),
        Undone::Refused { reason, .. } => {
            let why = if reason == Reason::MergeConflict.code() {
                "its undo conflicts with later merges".to_owned()
            } else {
                format!("bay undo answered {reason}")
            };
            (
                format!(
                    "main is red on {}; your change {named} broke it and no fix came in {waited}, \
                     and bay did not undo it: {why}.",
                    red.check
                ),
                crate::red_owner::fix_command(record),
            )
        }
    }
}

/// `after` as a reader says it: whole hours, else whole minutes, else seconds.
fn waited(after: Duration) -> String {
    match after.as_secs() {
        secs if secs >= 3600 && secs % 3600 == 0 => format!("{} h", secs / 3600),
        secs if secs >= 60 && secs % 60 == 0 => format!("{} min", secs / 60),
        secs => format!("{secs} s"),
    }
}

/// Writes `event` to this runner's log, after it wakes the culprit's session once with `said`
/// and `fix`, and tells the notify adapter, as a `red_owner` event is told. Returns the event
/// as written.
fn told_one(
    repo: &Repo,
    keys: &Keys,
    red: &Red,
    record: &ChangeRecord,
    mut event: Value,
    (said, fix): &(String, String),
) -> Result<Value, String> {
    if crate::talk::wake::configured(repo) {
        let id = event["id"].as_str().unwrap_or_default().to_owned();
        let undo = event["undo"].as_str().map(str::to_owned);
        event["wake"] = crate::talk::wake::wake_red(
            repo,
            &crate::talk::wake::RedOwner {
                change: &record.id,
                check: &red.check,
                event: &id,
                actor: &record.actor,
                said,
                fix,
                undo: undo.as_deref(),
            },
        );
    }
    crate::log::append(repo, &keys.executor_name, None, &event)?;
    let _ = crate::notify::notify(repo, &event);
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::red::{Culprit, Mark, Suspect, Tested};

    fn mark(landing: &str) -> Mark {
        Mark {
            landing: landing.to_owned(),
            change: Some(format!("c{landing}")),
        }
    }

    fn red(tested: &[(&str, &str)], fix: bool) -> Red {
        Red {
            check: "suite".to_owned(),
            environment: "local".to_owned(),
            paths: None,
            landing: "l4".to_owned(),
            change: Some("cl4".to_owned()),
            suspect: Some(Suspect {
                passed: Some(mark("l2")),
                passed_units: None,
                failed: mark("l3"),
                landings: vec![mark("l3")],
                failing: vec![mark("l4")],
                newest_failed: mark("l4"),
                culprit: Some(Culprit {
                    mark: mark("l3"),
                    tested: tested
                        .iter()
                        .map(|(landing, verdict)| Tested {
                            mark: mark(landing),
                            verdict: (*verdict).to_owned(),
                        })
                        .collect(),
                }),
                searched: None,
                added: None,
                judged_old: None,
                same_read: None,
                base_failed: None,
            }),
            fix: fix.then(|| mark("l5")),
            changed_check: false,
        }
    }

    /// Item 98: only a culprit whose own runs all failed is undone; a flaky one, one with a
    /// fix landed, or one no run of its own answered is not.
    #[test]
    fn only_a_culprit_whose_own_runs_failed_is_due() {
        let failed = [
            ("l2", "pass"),
            ("l3", PRODUCT_FAILURE),
            ("l4", PRODUCT_FAILURE),
        ];
        assert_eq!(pinned(&red(&failed, false)), Some("cl3"));
        assert_eq!(pinned(&red(&failed, true)), None);
        let flaky = [("l2", "pass"), ("l3", "flaky"), ("l4", PRODUCT_FAILURE)];
        assert_eq!(pinned(&red(&flaky, false)), None);
        let twice = [("l2", "pass"), ("l3", PRODUCT_FAILURE), ("l3", "pass")];
        assert_eq!(pinned(&red(&twice, false)), None);
        assert_eq!(pinned(&red(&[("l2", "pass")], false)), None);
    }

    /// Item 98: one decision per culprit, whatever runner sees it; another culprit or check is
    /// another decision, and none is a `red_owner` event's id.
    #[test]
    fn one_culprit_is_one_event() {
        let failed = [("l3", PRODUCT_FAILURE)];
        let one = event_id(&red(&failed, false), "cl3").unwrap();
        assert_eq!(one, event_id(&red(&failed, false), "cl3").unwrap());
        assert!(one.starts_with("evt_") && one.len() == 24, "{one}");
        assert_ne!(one, event_id(&red(&failed, false), "cl2").unwrap());
        let mut other = red(&failed, false);
        other.check = "lint".to_owned();
        assert_ne!(one, event_id(&other, "cl3").unwrap());
        assert_ne!(
            one,
            crate::red_owner::culprit_event(&red(&failed, false), "cl3").unwrap()
        );
    }

    #[test]
    fn the_wait_reads_in_hours_minutes_or_seconds() {
        assert_eq!(waited(Duration::from_hours(2)), "2 h");
        assert_eq!(waited(Duration::from_mins(30)), "30 min");
        assert_eq!(waited(Duration::from_secs(90)), "90 s");
        assert_eq!(waited(Duration::ZERO), "0 s");
    }
}
