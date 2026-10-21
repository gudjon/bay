//! Open talks named where a change lands (G56, ruling T9): `bay land` and `bay undo` fetch the
//! acting actor's talk refs (`refs/bay/talk/<actor>/*`, ruling T6) before they act, and their
//! `bay.result/1` lists under `detail.open_talks` every talk to that actor, unread or open, whose
//! target is the change: a delivery talk whose `change` names it. The fetch is not an accept: it
//! writes no ack, so an unread talk stays unread. The verbs never block on a talk: a talk cannot
//! widen or narrow a policy (`DESIGN` § 11c), so the list only tells the actor at that moment.

use serde_json::{Value, json};

use super::{TALK_PREFIX, Talk, talks_where};
use crate::registry::Registry;
use crate::repo::Repo;
use crate::result::BayResult;

/// Fetches the acting actor's talk refs, then lists the talks to it that are not closed and ask
/// about `change`, oldest first. Empty without a key registry (no talk exists then), for an actor
/// name no talk can reach, or when the talks cannot be read: the list only tells.
#[must_use]
pub fn open_on(repo: &Repo, change: &str) -> Vec<Value> {
    if !matches!(Registry::current(&repo.git), Ok(Some(registry)) if registry.signs_all()) {
        return Vec::new();
    }
    let me = crate::change::actor_name();
    if crate::names::operator_name(&me).is_err() {
        return Vec::new();
    }
    let prefix = format!("{TALK_PREFIX}{me}/");
    super::send::fetch_matching(repo, &format!("{prefix}*"));
    let Ok(mut talks) = talks_where(&repo.git, &prefix, |talk| {
        talk.who == me && !talk.closed() && talk.about_change() == Some(change)
    }) else {
        return Vec::new();
    };
    talks.sort_by(|a, b| (a.sent(), &a.id).cmp(&(b.sent(), &b.id)));
    talks.iter().map(row).collect()
}

fn row(talk: &Talk) -> Value {
    json!({
        "talk": talk.id,
        "kind": talk.body["kind"],
        "from": talk.from_name(),
        "what": talk.body["what"],
        "sent": talk.body["sent"],
        "follow_up": talk.body["follow_up"],
        "read": talk.has("ack"),
    })
}

/// Adds `open` to `result` as `detail.open_talks`, when it names any talk.
pub fn name_open_talks(result: &mut BayResult, open: Vec<Value>) {
    if open.is_empty() {
        return;
    }
    match result.detail.as_mut() {
        Some(Value::Object(detail)) => {
            detail.insert("open_talks".to_owned(), Value::Array(open));
        }
        Some(_) => {}
        None => result.detail = Some(json!({"open_talks": open})),
    }
}
