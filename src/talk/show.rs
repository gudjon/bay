//! `bay talk --show <id>` (G453, T6): one talk and every reply to it, read with no `git show`.
//!
//! It fetches as `bay status` does for talks ([`super::poll::command`]: this actor's talks, and
//! the ack of each that arrived), then that one talk wherever it lives, so a talk this actor
//! neither sent nor answers reads too. It writes no entry of its own. The result (`state:
//! "shown"`) holds the talk's row as `bay status --since` reads it, how it closed, its body, and
//! each entry in order: its number `n`, `kind`, who wrote it (`by`), when (`at`), and its own
//! fields (an answer's `value`, a backbrief's `status` and `evidence`, a close's `state`). Its
//! human text is the same, a line each. This is data the actor asked for, so it carries the
//! talk's text; a notice never does (ruling R13).

use std::fmt::Write as _;
use std::path::Path;

use serde_json::{Map, Value, json};

use super::standing::{follow, reader_now};
use super::{
    ENTRY_COMMON, Refusal, TALK_PREFIX, Talk, batch, entry_name, is_talk_id, talks_matching,
};
use crate::repo::Repo;
use crate::result::BayResult;

/// `bay talk --show <id>`.
#[must_use]
pub fn show(dir: &Path, id: &str) -> BayResult {
    match show_talk(dir, id) {
        Ok(result) => result,
        Err(Stop::Refused(refusal)) => refusal.result(),
        Err(Stop::Failed(why)) => BayResult::failed(why),
    }
}

/// Why a show stopped: a refusal, or a fault.
enum Stop {
    Refused(Refusal),
    Failed(String),
}

impl From<String> for Stop {
    fn from(why: String) -> Self {
        Self::Failed(why)
    }
}

fn show_talk(dir: &Path, id: &str) -> Result<BayResult, Stop> {
    let refused = |why: String| Stop::Refused(Refusal::invalid(why));
    if !is_talk_id(id) {
        return Err(refused(format!("{id:?} is not a talk id")));
    }
    let repo = Repo::open(dir)?;
    let accept = super::poll::command(&repo, crate::live::unix_now());
    let pattern = format!("{TALK_PREFIX}*/{id}");
    super::send::fetch_matching(&repo, &pattern);
    let talk = talks_matching(&repo.git, &pattern)?
        .into_iter()
        .next()
        .ok_or_else(|| refused(format!("no talk {id} is here or on the remote")))?;
    let followed = follow(&repo.git, &talk, reader_now())?;
    let mut detail = followed.row;
    detail["ref"] = json!(talk.name);
    detail["closed"] = json!(followed.closed);
    detail["body"] = talk.body.clone();
    detail["entries"] = Value::from(entries(&repo.git, &talk)?);
    if let Some(accept) = accept {
        detail["talk_accept"] = accept;
    }
    let human = text(&detail);
    let mut result = BayResult::ok(detail);
    result.state = Some("shown".to_owned());
    result.human = Some(human);
    Ok(result)
}

/// Each entry of `talk`, in order: its number, kind, writer and time, then its own fields.
fn entries(git: &crate::git::Git, talk: &Talk) -> Result<Vec<Value>, String> {
    let blobs: Vec<String> = talk
        .entries
        .iter()
        .map(|entry| entry.blob.clone())
        .collect();
    talk.entries
        .iter()
        .zip(batch(git, &blobs)?)
        .map(|(entry, object)| {
            let unread = |why: &str| format!("{}: {} {why}", talk.name, entry.path);
            let (_, data) = object.ok_or_else(|| unread("is missing"))?;
            let Ok(Value::Object(fields)) = serde_json::from_slice(&data) else {
                return Err(unread("is not a JSON object"));
            };
            let mut shown = Map::new();
            shown.insert(
                "n".to_owned(),
                json!(entry_name(&entry.path).map(|(n, _)| n)),
            );
            shown.insert("kind".to_owned(), json!(entry.kind));
            shown.insert(
                "by".to_owned(),
                fields
                    .get("actor")
                    .map_or(Value::Null, |actor| actor["name"].clone()),
            );
            shown.insert(
                "at".to_owned(),
                fields.get("at").cloned().unwrap_or(Value::Null),
            );
            for (key, field) in fields {
                if !ENTRY_COMMON.contains(&key.as_str()) {
                    shown.insert(key, field);
                }
            }
            Ok(Value::Object(shown))
        })
        .collect()
}

/// The shown talk as lines a person reads: the talk, its ask, then an entry a line.
fn text(detail: &Value) -> String {
    let word = |value: &Value| value.as_str().unwrap_or("-").to_owned();
    let mut out = format!(
        "talk {} ({}) from {} to {}, sent {}: {}",
        word(&detail["talk"]),
        word(&detail["kind"]),
        word(&detail["from"]),
        word(&detail["who"]),
        word(&detail["sent"]),
        word(&detail["standing"]),
    );
    if let Some(state) = detail["closed"].as_str() {
        let _ = write!(out, " ({state})");
    }
    let _ = write!(out, "\n  {}", word(&detail["what"]));
    for entry in detail["entries"].as_array().into_iter().flatten() {
        let _ = write!(
            out,
            "\n  {:04} {} by {} at {}",
            entry["n"].as_u64().unwrap_or(0),
            word(&entry["kind"]),
            word(&entry["by"]),
            word(&entry["at"]),
        );
        let said =
            match entry["kind"].as_str() {
                Some("answer") => entry["value"].as_str().map(str::to_owned),
                Some("backbrief") => entry["status"]
                    .as_str()
                    .map(|status| match entry["evidence"].as_str() {
                        Some(evidence) => format!("{status} (evidence {evidence})"),
                        None => status.to_owned(),
                    }),
                Some("close") => entry["state"].as_str().map(str::to_owned),
                _ => None,
            };
        if let Some(said) = said {
            let _ = write!(out, ": {said}");
        }
    }
    out
}
