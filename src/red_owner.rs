//! 0.6.3 (a fleet finding, 2026-10-29: a red post check held every change on its paths, and
//! nothing told the session whose merge first failed it; the fix waited until someone read
//! `bay status`): the change that owns a red post check is told so, once.
//!
//! Each runner pass, after it reads the remote's post records, writes one `red_owner` event per
//! red check of main and change that owns it to its log: the culprit when the evidence pins one,
//! else each merge on its paths since it last passed (G634, below), which stay the same while the
//! red stands. The event's id is a digest of the check, its environment and that change, so one red is one event
//! however many runners see it, and a log that holds it is never written again. A new event asks
//! the wake adapter once to wake that change's session (`cause: "red"`), and is told to the
//! notify adapter; the session's hook tells it as a notice line (`crate::notice`). The event's
//! `line` names the check, where it failed and the exact fix to run. A red a fix already answers
//! (its run not passed yet) tells no one, and a red whose failing landing names no change has no
//! one to tell.
//!
//! G630 (a fleet finding, 2026-11-02: a red came back on another unit after a fix landed, and the
//! session of the merge that broke the new unit was told nothing, because the red's owner stays
//! its first failure): a red that fails on units its first failure did not is told again, to the
//! merges on the check's paths between the run where those units first failed and the failing run
//! before it, or to that landing when none was on its paths. Each is told once per check, in an
//! event with the units; the evidence does not prove which of them broke the units, and the line
//! says so.
//!
//! G634 (a fleet finding, 2026-11-02: a red with no culprit was told to its first failure, a
//! merge of three documents no path of the check names, and not to the one merge on its paths
//! since it last passed): when the evidence pins no landing, the red is told to the merges on the
//! check's paths since it last passed, up to its first failure, once each, or to that failure
//! when none was on its paths. The line says how many there were, and that none is a proven
//! cause.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use crate::canonical::digest;
use crate::change::{Chain, ChangeRecord};
use crate::evidence::{self, CheckId, EvidenceRecord, Verdict};
use crate::git::Git;
use crate::red::{Mark, Red};
use crate::registry::Registry;
use crate::repo::Repo;
use crate::signing::{Keys, Signer};

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
    let signer = match registry.as_ref().filter(|registry| registry.signs_all()) {
        None => None,
        Some(_) => Some(keys.executor.clone().ok_or(
            "the repository has a key registry and this machine has no runner key (git config bay.runnerKey)",
        )?),
    };
    let chain = Chain::new(git);
    let mut notes = Vec::new();
    let mut written = BTreeSet::new();
    for red in &reds {
        // A fault reading the range leaves the red's own owner told.
        let mut owners = match range_owners(git, red) {
            Ok(Some(owners)) => owners,
            Ok(None) => red_owner(red),
            Err(error) => {
                notes.push(json!({"red_owner": null, "check": red.check, "error": error}));
                red_owner(red)
            }
        };
        // A fault reading the units leaves the red's own owner told.
        match unit_owners(git, registry.as_ref(), red) {
            Ok(more) => owners.extend(more),
            Err(error) => {
                notes.push(json!({"red_owner": null, "check": red.check, "error": error}));
            }
        }
        for (owner, owns) in &owners {
            let id = event_id(red, owner, owns)?;
            if held.contains(&id) || !written.insert(id.clone()) {
                continue;
            }
            let Ok((_, record)) = chain.read(owner) else {
                continue;
            };
            told_one(repo, (&keys, signer.as_ref()), red, (&id, owns, &record))?;
            notes.push(json!({"red_owner": owner, "check": red.check, "event": id}));
        }
    }
    Ok(notes)
}

/// Writes the `red_owner` event `id` of `red` to `record`'s change, wakes its session once and
/// tells the notify adapter.
fn told_one(
    repo: &Repo,
    (keys, signer): (&Keys, Option<&Signer>),
    red: &Red,
    (id, owns, record): (&str, &Owns, &ChangeRecord),
) -> Result<(), String> {
    let mut event = event(id, red, owns, record);
    if crate::talk::wake::configured(repo) {
        event["wake"] = crate::talk::wake::wake_red(
            repo,
            &crate::talk::wake::RedOwner {
                change: &record.id,
                check: &red.check,
                event: id,
                actor: &record.actor,
                said: &said(red, owns, record),
                fix: &fix_command(record),
            },
        );
    }
    crate::log::append(repo, &keys.executor_name, signer, &event)?;
    let _ = crate::notify::notify(repo, &event);
    Ok(())
}

/// What a `red_owner` event tells its change.
#[derive(Debug, PartialEq, Eq)]
enum Owns {
    /// The red: [`Red::fixes`] names the change.
    Red,
    /// G634: the red, which the evidence pins on no landing. The change is one of `among` merges
    /// on the check's paths since it last passed; `among` is 0 when none was, and the change is
    /// where it was first seen failing.
    Range { among: usize },
    /// G630: the units that failed first after the red's first failure. The change is one of
    /// `among` merges on the check's paths since the failing run before; `among` is 0 when none
    /// was, and the change is where they were first seen failing.
    Units { units: Vec<String>, among: usize },
}

/// The change [`Red::fixes`] names, told that it owns `red`.
fn red_owner(red: &Red) -> Vec<(String, Owns)> {
    red.fixes()
        .map(|owner| (owner.clone(), Owns::Red))
        .into_iter()
        .collect()
}

/// G634: the changes to tell of `red` when the evidence pins no landing: the merges on the
/// check's paths after the newest pass, up to the first failure, oldest first, or that failure
/// when none was on its paths. `None` when [`Red::fixes`] names the owner: the evidence pins a
/// culprit or the landing that added the check, or found no pass, and the range then runs to the
/// oldest landing read (B5).
fn range_owners(git: &Git, red: &Red) -> Result<Option<Vec<(String, Owns)>>, String> {
    let Some(suspect) = &red.suspect else {
        return Ok(None);
    };
    if suspect.culprit.is_some() || suspect.added.is_some() || suspect.passed.is_none() {
        return Ok(None);
    }
    let names: Vec<String> = suspect
        .landings
        .iter()
        .map(|mark| format!("{}^1", mark.landing))
        .collect();
    let parents = rev_parse(git, &names)?;
    let pairs: Vec<(&str, &str)> = suspect
        .landings
        .iter()
        .zip(&parents)
        .map(|(mark, parent)| (mark.landing.as_str(), parent.as_str()))
        .collect();
    let changed = crate::check::changed_paths_many(git, &pairs)?;
    let (to_tell, among) = on_paths(red, suspect.landings.iter().zip(&changed), &suspect.failed);
    Ok(Some(
        to_tell
            .into_iter()
            .filter_map(|mark| mark.change.clone())
            .map(|change| (change, Owns::Range { among }))
            .collect(),
    ))
}

/// The marks of `range` whose changed paths meet `red`'s check, oldest first, and how many;
/// `last` alone, and 0, when none does.
fn on_paths<'m, 'c>(
    red: &Red,
    range: impl Iterator<Item = (&'m Mark, &'c Vec<String>)>,
    last: &'m Mark,
) -> (Vec<&'m Mark>, usize) {
    let on: Vec<&Mark> = range
        .filter(|(_, changed)| red.covers(changed))
        .map(|(mark, _)| mark)
        .collect();
    match on.len() {
        0 => (vec![last], 0),
        among => (on, among),
    }
}

/// The object id of each of `names`, in their order, read by one `git rev-parse`.
fn rev_parse(git: &Git, names: &[String]) -> Result<Vec<String>, String> {
    let mut args = vec!["rev-parse"];
    args.extend(names.iter().map(String::as_str));
    let out = git.run(&args).map_err(|error| error.message())?;
    let ids: Vec<String> = out.lines().map(str::to_owned).collect();
    if ids.len() != names.len() {
        return Err(format!(
            "git rev-parse gave {} of {} names",
            ids.len(),
            names.len()
        ));
    }
    Ok(ids)
}

/// G630: the changes to tell of `red`'s units that its first failure did not fail, each with
/// them, oldest first. The failing runs are read at the first failure and each landing after
/// it, in one read of their evidence ([`evidence::records_for_many`]) and one of their changed
/// paths ([`crate::check::changed_paths_many`]). The red's own owner is never one of them.
fn unit_owners(
    git: &Git,
    registry: Option<&Registry>,
    red: &Red,
) -> Result<Vec<(String, Owns)>, String> {
    let Some(suspect) = &red.suspect else {
        return Ok(Vec::new());
    };
    if suspect.failing.is_empty() {
        return Ok(Vec::new());
    }
    let marks: Vec<&Mark> = std::iter::once(&suspect.failed)
        .chain(&suspect.failing)
        .collect();
    // One `rev-parse`: each landing's tree, then the first parent of each landing after the
    // first failure.
    let mut names: Vec<String> = marks
        .iter()
        .map(|mark| format!("{}^{{tree}}", mark.landing))
        .collect();
    names.extend(marks[1..].iter().map(|mark| format!("{}^1", mark.landing)));
    let ids = rev_parse(git, &names)?;
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    let (trees, parents) = ids.split_at(marks.len());
    let id = CheckId::new(&red.check, &red.environment);
    let mut failed = Vec::with_capacity(marks.len());
    for records in evidence::records_for_many(git, trees)? {
        failed.push(failed_units(
            &evidence::counted(git, records, registry)?,
            &id,
        ));
    }
    let Some(Some(first)) = failed.first() else {
        return Ok(Vec::new());
    };
    let pairs: Vec<(&str, &str)> = marks[1..]
        .iter()
        .zip(parents)
        .map(|(mark, parent)| (mark.landing.as_str(), *parent))
        .collect();
    let changed = crate::check::changed_paths_many(git, &pairs)?;
    let mut known: BTreeSet<String> = first.iter().cloned().collect();
    let mut owners = Vec::new();
    let mut before = 0;
    for (at, units) in failed.iter().enumerate().skip(1) {
        let Some(units) = units else {
            continue;
        };
        let new: Vec<String> = units
            .iter()
            .filter(|unit| !known.contains(*unit))
            .cloned()
            .collect();
        let since = before;
        before = at;
        if new.is_empty() {
            continue;
        }
        // `changed[n]` is the paths of `marks[n + 1]`.
        let (to_tell, among) = on_paths(
            red,
            (since + 1..=at).map(|n| (marks[n], &changed[n - 1])),
            marks[at],
        );
        for mark in to_tell {
            if let Some(change) = &mark.change
                && Some(change) != red.fixes()
            {
                owners.push((
                    change.clone(),
                    Owns::Units {
                        units: new.clone(),
                        among,
                    },
                ));
            }
        }
        known.extend(new);
    }
    Ok(owners)
}

/// The units the newest `product_failure` record of `id` among `records` failed, sorted, once
/// each; `None` when no such record is there (the check did not run, or did not fail).
fn failed_units(records: &[EvidenceRecord], id: &CheckId) -> Option<Vec<String>> {
    let newest = records
        .iter()
        .filter(|record| {
            record.kind == evidence::KIND_CHECK
                && record.check == *id
                && record.verdict == Verdict::ProductFailure
        })
        .max_by(|one, other| one.at.cmp(&other.at))?;
    let mut units: Vec<String> = newest
        .units
        .iter()
        .filter(|unit| unit.verdict == Verdict::ProductFailure)
        .map(|unit| unit.id.clone())
        .collect();
    units.sort();
    units.dedup();
    Some(units)
}

/// The id of the `red_owner` event of `red`, owned by `owner`: one red, one event. A change told
/// of units (G630) is told once per check too; its id is not the one it would have as the red's
/// owner, so a later red that it owns is still told. A merge of a red's range (G634) owns the red:
/// its id is the one it has as the red's owner.
fn event_id(red: &Red, owner: &str, owns: &Owns) -> Result<String, String> {
    let mut fields = json!({
        "kind": KIND,
        "check": red.check,
        "environment": red.environment,
        "owner": owner,
    });
    if matches!(owns, Owns::Units { .. }) {
        fields["owns"] = json!("units");
    }
    let id = digest(&fields)?;
    Ok(format!("evt_{}", &id.trim_start_matches("sha256:")[..20]))
}

/// The `red_owner` event `id` of `red`, for `record`, the change it tells.
fn event(id: &str, red: &Red, owns: &Owns, record: &ChangeRecord) -> Value {
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
        "culprit": *owns == Owns::Red && red.culprit().is_some(),
        "fix": fix_command(record),
        "line": with_fix(&said(red, owns, record), &fix_command(record)),
        "detail": red.to_json(),
        "recurrence_key": format!("{KIND}/{}/{}", red.check, red.environment),
    });
    match owns {
        Owns::Red => {}
        Owns::Range { among } => event["among"] = json!(among),
        Owns::Units { units, among } => {
            event["units"] = json!(units);
            event["among"] = json!(among);
        }
    }
    event
}

/// The command that opens the fix: a new change of the owner's intent that names it.
fn fix_command(record: &ChangeRecord) -> String {
    format!(
        "bay start {} --fixes {}",
        record.intent.reference, record.id
    )
}

/// The line the owner's session is told: what the red is ([`said`]), then the fix to run.
#[must_use]
pub fn with_fix(said: &str, fix: &str) -> String {
    format!("{said} Fix it: {fix}")
}

/// What the owner's session is told the red is: the check, whether the evidence proves the
/// change broke it or only saw it failing there, and what the red holds.
fn said(red: &Red, owns: &Owns, record: &ChangeRecord) -> String {
    let named = crate::change::shown(&record.intent.reference, &record.id);
    let how = match owns {
        Owns::Red if red.culprit().is_some() => {
            "broke it: the check passed on the merge before it and failed on it".to_owned()
        }
        Owns::Red | Owns::Range { among: 0 } => {
            "is where it was first seen failing, not a proven cause".to_owned()
        }
        Owns::Range { among: 1 } => {
            "is the one merge on its paths since it last passed, not a proven cause".to_owned()
        }
        Owns::Range { among } => {
            format!(
                "is one of {among} merges on its paths since it last passed, not a proven cause"
            )
        }
        Owns::Units { units, among } => {
            let more = match units.len() {
                1 => "1 more unit".to_owned(),
                n => format!("{n} more units"),
            };
            let place = match among {
                0 => "is where they were first seen failing".to_owned(),
                1 => "is the one merge on its paths since the failing run before".to_owned(),
                n => format!("is one of {n} merges on its paths since the failing run before"),
            };
            format!("{place}: it now fails on {more}, not a proven cause")
        }
    };
    format!(
        "main is red on {}; your change {named} {how}. Changes on its paths run it before they merge.",
        red.check
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(landing: &str) -> Mark {
        Mark {
            landing: landing.to_owned(),
            change: Some(landing.to_owned()),
        }
    }

    /// G634, G630: the marks whose paths meet the check are told, oldest first, with how many;
    /// with none on its paths, the last mark alone, and 0.
    #[test]
    fn the_merges_on_the_paths_are_told_else_the_last() {
        let red = Red {
            check: "suite".to_owned(),
            environment: "local".to_owned(),
            paths: Some(vec!["src/**".to_owned()]),
            landing: "c".to_owned(),
            change: None,
            suspect: None,
            fix: None,
            changed_check: false,
        };
        let marks = [mark("a"), mark("b"), mark("c")];
        let changed = [
            vec!["src/a.rs".to_owned()],
            vec!["conf/b".to_owned()],
            vec!["src/c.rs".to_owned()],
        ];
        let told = on_paths(&red, marks.iter().zip(&changed), &marks[2]);
        assert_eq!(told, (vec![&marks[0], &marks[2]], 2));
        let off = [
            vec!["conf/a".to_owned()],
            Vec::new(),
            vec!["docs/c.md".to_owned()],
        ];
        let told = on_paths(&red, marks.iter().zip(&off), &marks[2]);
        assert_eq!(told, (vec![&marks[2]], 0));
    }
}
