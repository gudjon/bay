//! One ask to many (0.6.0, T2): `bay talk --to <a> --to <b> "<what>"`, or `--to-live` for every
//! actor with a live session in this repository but the sender, sends one talk to each recipient
//! under one result (`detail.talks`, one item per recipient), and `bay talk --round <id>` shows
//! the round: one row per recipient, with its talk, the talk's standing, and its answer or
//! backbrief status.
//!
//! Each talk of a round names it in `round`: 80 bits of the sha256 of the canonical JSON of the
//! sender's name, the ask (kind, `what`, target) and the sorted recipients ([`round_id`]). So the
//! same ask to the same recipients is the same round, and each of its talks is `already_sent`
//! while it is open (T15); asked again once a talk closed, the recipient's new talk is in the
//! same round, and the table shows each recipient's newest. A recipient that holds the same
//! open ask from a talk outside the round is that talk, in its own round.
//!
//! Every refusal is judged for every recipient before any talk is sent, so a refused round sends
//! nothing. More recipients than `[talk] max_round` is `TALK_REFUSED`, naming the cap and the
//! count. The live actors are the fleet map's sessions (`bay status --all`) after a fetch of
//! the remote's live refs.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::send::{Plan, Stop, carry_out, finish, open, plan, talk_policy, with_defaults};
use super::{
    ID_BYTES, Known, Refusal, TALK_PREFIX, Talk, Trust, is_talk_id, talks_where, verified_answer,
};
use crate::change::Actor;
use crate::repo::Repo;
use crate::result::{BayResult, Decision, Reason};

/// `bay talk --round <id>`: the round's table.
#[must_use]
pub fn round(dir: &Path, id: &str) -> BayResult {
    finish(table(dir, id))
}

/// Sends `ask` from `sender` to each of its recipients, as one round.
pub(super) fn fan_out(
    dir: &Path,
    repo: &Repo,
    trust: &Trust,
    sender: &Actor,
    ask: &super::Ask,
) -> Result<BayResult, Stop> {
    if ask.who.is_some() || ask.to_change.is_some() {
        return Err(Refusal::invalid(
            "`--to` and `--to-live` name the recipients: give the action alone, with no recipient or --to-change beside them",
        )
        .into());
    }
    let recipients = recipients(dir, repo, sender, ask)?;
    let policy = talk_policy(repo)?;
    if let Some(max) = policy.max_round
        && recipients.len() > usize::try_from(max).unwrap_or(usize::MAX)
    {
        return Err(Refusal::new(
            Reason::TalkRefused,
            format!(
                "the round has {} recipients and [talk] max_round is {max}: ask fewer at once",
                recipients.len()
            ),
        )
        .with("max_round", json!(max))
        .with("recipients", json!(recipients.len()))
        .into());
    }
    let sent = crate::live::unix_now();
    // One follow-up and one round for every recipient.
    let filled = with_defaults(ask, sent, policy.follow_up_s());
    let round = round_id(&sender.name, &filled, &recipients)?;
    let mut plans: Vec<(&String, Plan)> = Vec::with_capacity(recipients.len());
    for who in &recipients {
        let one = super::Ask {
            who: Some(who.clone()),
            round: Some(round.clone()),
            ..filled.clone()
        };
        let judged = plan(repo, trust, sender, &one, sent, ask.follow_up.is_some())
            .map_err(|stop| naming(stop, who))?;
        plans.push((who, judged));
    }
    let talks: Vec<Value> = plans
        .into_iter()
        .map(|(who, judged)| item(who, carry_out(repo, sender, judged, ask.dry_run)))
        .collect();
    let mut detail = json!({"round": round, "talks": talks});
    if ask.dry_run {
        detail["dry_run"] = json!(true);
    }
    Ok(BayResult::ok(detail))
}

/// The recipients of `ask`, each once, in the order given: every `--to`, then with `--to-live`
/// every live actor but the sender.
fn recipients(
    dir: &Path,
    repo: &Repo,
    sender: &Actor,
    ask: &super::Ask,
) -> Result<Vec<String>, Stop> {
    let live = if ask.to_live {
        live_actors(dir, repo, &sender.name)?
    } else {
        Vec::new()
    };
    let mut names: Vec<String> = Vec::new();
    for name in ask.to.iter().chain(&live) {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    if names.is_empty() {
        return Err(Refusal::invalid(
            "--to-live found no live session of another actor in this repository: name the recipients with --to",
        )
        .into());
    }
    Ok(names)
}

/// The actors of the fresh sessions the fleet map names (`bay status --all`) after a fetch of
/// the remote's live refs, `sender` aside, sorted.
fn live_actors(dir: &Path, repo: &Repo, sender: &str) -> Result<Vec<String>, Stop> {
    crate::fleet::fetch(repo);
    let map = crate::fleet::fleet(dir);
    if map.decision != Decision::Ok {
        return Err(Refusal::invalid(format!(
            "--to-live reads the live sessions from the fleet map, which could not be read: {}",
            map.say()
        ))
        .into());
    }
    let sessions = map
        .detail
        .as_ref()
        .and_then(|detail| detail["sessions"].as_array().cloned())
        .unwrap_or_default();
    let mut names: Vec<String> = sessions
        .iter()
        .filter(|session| session["tracked"] == true)
        .filter_map(|session| session["actor"]["name"].as_str())
        .filter(|name| *name != sender)
        .map(str::to_owned)
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

/// The round of `ask` from `sender` to `recipients`: see the module doc.
fn round_id(sender: &str, ask: &super::Ask, recipients: &[String]) -> Result<String, Stop> {
    let mut to = recipients.to_vec();
    to.sort();
    let one = |values: &[String]| values.first().cloned();
    let input = json!({"from": sender, "to": to, "kind": ask.kind, "what": ask.what,
        "change": one(&ask.change), "artifact": one(&ask.artifact),
        "decision": one(&ask.decision), "check": one(&ask.check)});
    let text = crate::canonical::canonical(&input)?;
    Ok(crate::ids::base32(
        &Sha256::digest(text.as_bytes())[..ID_BYTES],
    ))
}

/// A refusal for one recipient, naming it.
fn naming(stop: Stop, who: &str) -> Stop {
    match stop {
        Stop::Refused(refusal) => Stop::Refused(refusal.with("who", json!(who))),
        Stop::Failed(why) => Stop::Failed(format!("the talk to {who}: {why}")),
    }
}

/// One recipient's item of a round's result: its talk's detail and state, or why the host
/// refused it after every recipient was judged.
fn item(who: &str, outcome: Result<BayResult, Stop>) -> Value {
    match outcome {
        Ok(result) => {
            let mut item = result.detail.unwrap_or_else(|| json!({}));
            item["who"] = json!(who);
            item["state"] = json!(result.state);
            item
        }
        Err(Stop::Refused(refusal)) => json!({"who": who, "state": "refused",
            "reason": refusal.reason.code(), "why": refusal.why}),
        Err(Stop::Failed(why)) => json!({"who": who, "state": "failed", "why": why}),
    }
}

/// The round `id`'s table: each recipient's newest talk of the round, as the remote holds it.
fn table(dir: &Path, id: &str) -> Result<BayResult, Stop> {
    let (repo, trust, _) = open(dir)?;
    if !is_talk_id(id) {
        return Err(Refusal::invalid(format!("{id:?} is not a round id")).into());
    }
    let of_round = |talk: &Talk| talk.body["round"].as_str() == Some(id);
    let mut who: Vec<String> = talks_where(&repo.git, TALK_PREFIX, of_round)?
        .into_iter()
        .map(|talk| talk.who)
        .collect();
    who.dedup();
    // Each recipient's talks as the remote holds them: its replies are written there.
    if who.is_empty() {
        super::send::fetch_matching(&repo, &format!("{TALK_PREFIX}*"));
    }
    for name in &who {
        super::send::fetch_matching(&repo, &format!("{TALK_PREFIX}{name}/*"));
    }
    let rows = rows(
        &repo,
        &trust,
        talks_where(&repo.git, TALK_PREFIX, of_round)?,
    )?;
    if rows.is_empty() {
        return Err(
            Refusal::invalid(format!("no talk here or on the remote is of round {id}")).into(),
        );
    }
    let mut result = BayResult::ok(json!({"round": id, "talks": rows}));
    // A read: its sentence never says anything was sent (`result.rs` `talk_say`).
    result.state = Some("shown".to_owned());
    result.human = Some(human(id, &rows));
    Ok(result)
}

/// One row per recipient of `talks`: its newest talk, the one no other talk to it repeats.
fn rows(repo: &Repo, trust: &Trust, talks: Vec<Talk>) -> Result<Vec<Value>, String> {
    let repeated: Vec<String> = talks
        .iter()
        .filter_map(|talk| talk.body["repeats"].as_str().map(str::to_owned))
        .collect();
    let mut newest: BTreeMap<String, Talk> = BTreeMap::new();
    for talk in talks
        .into_iter()
        .filter(|talk| !repeated.contains(&talk.id))
    {
        let newer = newest
            .get(&talk.who)
            .is_none_or(|held| (talk.sent(), &talk.id) > (held.sent(), &held.id));
        if newer {
            newest.insert(talk.who.clone(), talk);
        }
    }
    let now = super::standing::reader_now();
    newest
        .values()
        .map(|talk| {
            let mut row = super::standing::row(&repo.git, talk, now)?;
            row["answer"] = json!(verified_answer(&repo.git, trust, talk));
            row["reply"] = talk
                .last_entry(&repo.git, &Known::new(), "backbrief")?
                .map_or(Value::Null, |backbrief| backbrief["status"].clone());
            Ok(row)
        })
        .collect()
}

/// The table as a person reads it: one line per recipient.
fn human(id: &str, rows: &[Value]) -> String {
    let mut text = format!("round {id}: {} recipients", rows.len());
    for row in rows {
        let answered = row["answer"]
            .as_str()
            .map(|answer| format!("answered {answer}"))
            .or_else(|| {
                row["reply"]
                    .as_str()
                    .map(|status| format!("backbrief {status}"))
            })
            .unwrap_or_else(|| "no reply yet".to_owned());
        let _ = write!(
            text,
            "\n{:<20} {} {:<16} {answered}",
            row["who"].as_str().unwrap_or_default(),
            row["talk"].as_str().unwrap_or_default(),
            row["standing"].as_str().unwrap_or_default(),
        );
    }
    text
}
