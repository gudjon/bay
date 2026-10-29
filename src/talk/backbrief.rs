//! `bay talk --backbrief <id> <status>` (A6): the reply to a delivery or reliability talk.
//!
//! A backbrief is the recipient's report on a talk (`DESIGN` § 11c): the outcome it reads as
//! asked, its first step, what is still unclear, a status word (ready, blocked, validated or
//! done) and any evidence already in Git; a blocked one names the reason code it waits on and the
//! new time its next reply is owed. bay writes it as the host's judge takes it
//! ([`super::judge`]) and judges the same rules here first, so a backbrief under
//! `signing = "approvals"`, where no host judges, keeps them too: only `who` writes one, and a
//! closed talk takes none. The outcome is the talk's own `what` (the ask as written) unless
//! `--outcome` restates it, the first step is the status word unless `--first-step` names one,
//! and nothing is unclear unless `--unclear` says what is.
//!
//! A backbrief reports progress, so a talk takes several: the same one as the latest is
//! `already_backbriefed` and writes nothing (it pushes what is here, as a resend does), and any
//! other is the next entry. A discovery talk is
//! answered with `--answer`; while it is open it takes a backbrief too (blocked, to move its
//! `follow_up`), as the judge allows.

use std::path::Path;

use serde_json::{Map, Value, json};

use super::send::{Pushed, Stop, agent_key, find, finish, open, push, repeated, sent_result};
use super::{
    BAY_TALK_MAX_BYTES, Known, Refusal, STATUS_WORDS, TALK_SCHEMA, Talk, Trust, entry_commit_as,
    fold, plain,
};
use crate::canonical::canonical;
use crate::clock::unix_of;
use crate::result::{BayResult, Reason};

/// One backbrief, as the command line gives it. Every field is checked by bay, never by the
/// parser, so a missing or wrong field is a `bay.result/1` refusal.
#[derive(Clone, Debug, Default)]
pub struct Backbrief {
    pub talk: String,
    /// The status word, the argument after the id.
    pub status: Option<String>,
    pub outcome: Option<String>,
    pub first_step: Option<String>,
    pub unclear: Option<String>,
    pub evidence: Option<String>,
    /// When the next reply is owed; a blocked backbrief names a new one.
    pub follow_up: Option<String>,
    /// The reason code a blocked backbrief waits on (`INTERFACES` § 2).
    pub reason: Option<String>,
}

/// The fields a repeat of the latest backbrief matches: all but who wrote it, and when.
const REPEATED: [&str; 7] = [
    "status",
    "outcome",
    "first_step",
    "unclear",
    "evidence",
    "follow_up",
    "reason",
];

#[must_use]
pub fn backbrief(dir: &Path, request: &Backbrief) -> BayResult {
    finish(give(dir, request))
}

fn give(dir: &Path, request: &Backbrief) -> Result<BayResult, Stop> {
    let (repo, trust, me) = open(dir)?;
    let id = request.talk.as_str();
    let talk = find(&repo, id)?
        .ok_or_else(|| Refusal::invalid(format!("no talk {id} is here or on the remote")))?;
    let fields = checked(&talk, &me.name, request)?;
    let detail = json!({"talk": id, "ref": talk.name, "from": talk.from_name(),
        "status": fields["status"], "follow_up": fields.get("follow_up")});
    let known = Known::new();
    let latest = talk.last_entry(&repo.git, &known, "backbrief")?;
    if latest.is_some_and(|latest| {
        REPEATED
            .iter()
            .all(|field| latest.get(*field) == fields.get(*field))
    }) {
        return repeated(&repo, &talk, detail, "already_backbriefed");
    }
    blocked(talk.follow_up_known(&repo.git, &known)?.as_deref(), &fields)?;
    let key = match &trust {
        Trust::Keys(registry) => Some(agent_key(&repo.git, registry, &me.name)?),
        Trust::Names => None,
    };
    let mut entry = fields;
    for (field, value) in [
        ("schema", json!(TALK_SCHEMA)),
        ("entry", json!("backbrief")),
        ("talk", json!(id)),
        ("actor", json!({"name": me.name, "session": me.session})),
        ("at", json!(crate::clock::now_rfc3339())),
    ] {
        entry.insert(field.to_owned(), value);
    }
    let entry = Value::Object(entry);
    if canonical(&entry)?.len() > BAY_TALK_MAX_BYTES {
        return Err(Refusal::invalid(format!(
            "the backbrief is over BAY_TALK_MAX_BYTES ({BAY_TALK_MAX_BYTES} bytes), a bay limit: give a shorter --outcome"
        ))
        .into());
    }
    let commit = entry_commit_as(&repo.git, &talk.tip, &entry, key.as_deref(), &me.name)?;
    repo.git
        .run(&["update-ref", &talk.name, &commit, &talk.tip])
        .map_err(|error| error.message())?;
    let mut detail = detail;
    detail["backbrief"] = json!(commit);
    match push(&repo, &[format!("{commit}:{}", talk.name)]) {
        Pushed::Yes => Ok(sent_result(detail, "backbriefed", None)),
        Pushed::Refused(reason, why) => {
            let _ = repo
                .git
                .run(&["update-ref", &talk.name, &talk.tip, &commit]);
            Err(Refusal::new(
                reason.unwrap_or(Reason::TalkRefused),
                format!("the host refused the backbrief: {why}"),
            )
            .with("talk", json!(id))
            .into())
        }
        Pushed::NotNow(why) => Ok(sent_result(detail, "not_pushed", Some(why))),
    }
}

/// The backbrief's own fields, by the judge's rules: only `who` writes one, on an open talk; the
/// status is a status word; only a blocked one names a reason, and that a reason code; each text
/// is plain; a `follow_up` is a time after now. A blocked one's new `follow_up` is [`blocked`]'s.
fn checked(talk: &Talk, me: &str, request: &Backbrief) -> Result<Map<String, Value>, Refusal> {
    let id = &talk.id;
    if me != talk.who {
        return Err(Refusal::invalid(format!(
            "only {} backbriefs talk {id}; {me} is not its `who`",
            talk.who
        )));
    }
    if talk.closed() {
        return Err(Refusal::invalid(format!(
            "talk {id} is closed; it owes no reply and takes no backbrief"
        )));
    }
    let status = request.status.as_deref().ok_or_else(|| {
        Refusal::invalid(format!(
            "a backbrief needs its status: `bay talk --backbrief {id} <status>`, one of ready, blocked, validated or done"
        ))
    })?;
    if !STATUS_WORDS.contains(&status) {
        return Err(Refusal::invalid(format!(
            "{status:?} is not a status word: ready, blocked, validated or done"
        )));
    }
    let mut fields = Map::new();
    fields.insert("status".to_owned(), json!(status));
    let outcome = request
        .outcome
        .as_deref()
        .or_else(|| talk.body["what"].as_str())
        .unwrap_or_default();
    let first_step = request.first_step.as_deref().unwrap_or(status);
    for (field, text) in [("outcome", outcome), ("first_step", first_step)] {
        let text = fold(text);
        plain(field, &text)?;
        if text.trim().is_empty() {
            return Err(Refusal::invalid(format!(
                "the backbrief's `{field}` is empty"
            )));
        }
        fields.insert(field.to_owned(), json!(text));
    }
    let unclear = fold(request.unclear.as_deref().unwrap_or_default());
    plain("unclear", &unclear)?;
    fields.insert("unclear".to_owned(), json!(unclear));
    if let Some(evidence) = &request.evidence {
        let evidence = fold(evidence);
        plain("evidence", &evidence)?;
        fields.insert("evidence".to_owned(), json!(evidence));
    }
    if let Some(follow_up) = &request.follow_up {
        let due = unix_of(follow_up).ok_or_else(|| {
            Refusal::invalid(format!(
                "`--follow-up` {follow_up:?} is not a UTC time in the form 2026-10-19T12:00:00Z"
            ))
        })?;
        if due <= crate::live::unix_now().max(talk.sent()) {
            return Err(Refusal::invalid(format!(
                "`--follow-up` {follow_up} is not after now: it is when the next reply is owed"
            )));
        }
        fields.insert("follow_up".to_owned(), json!(follow_up));
    }
    match (&request.reason, status) {
        (Some(_), status) if status != "blocked" => Err(Refusal::invalid(
            "only a blocked backbrief names a reason: give --reason with blocked alone",
        )),
        (Some(code), _) if Reason::from_code(code).is_none() => Err(Refusal::invalid(format!(
            "{code:?} is not a reason code (INTERFACES § 2): a blocked backbrief names the code it waits on"
        ))),
        (Some(code), _) => {
            fields.insert("reason".to_owned(), json!(code));
            Ok(fields)
        }
        (None, _) => Ok(fields),
    }
}

/// A blocked backbrief does not close the talk: it names a new `follow_up`, other than the one
/// the talk owes now (`current`), and the reason code it waits on.
fn blocked(current: Option<&str>, fields: &Map<String, Value>) -> Result<(), Refusal> {
    if fields["status"] != "blocked" {
        return Ok(());
    }
    let follow_up = fields.get("follow_up").and_then(Value::as_str);
    if follow_up.is_none() || follow_up == current {
        return Err(Refusal::invalid(
            "a blocked backbrief states its new `follow_up` (--follow-up <time>): it does not close the talk",
        ));
    }
    if !fields.contains_key("reason") {
        return Err(Refusal::invalid(
            "a blocked backbrief names its reason code (INTERFACES § 2) in --reason, such as CHECK_FAILED",
        ));
    }
    Ok(())
}
