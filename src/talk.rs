//! Talk (`DESIGN` § 11c; `NAMES` talk; rulings R9 and T1–T12 in `plans/wave2.md`).
//!
//! One talk is one append-only commit chain, `refs/bay/talk/<who>/<id>` (T6: sharded by the
//! recipient, whose name never changes on a talk). Its root commit is signed by the sender's
//! agent key and holds `talk.json` alone: the `bay.talk/1` body in RFC 8785 canonical form. Each
//! later commit adds one file, `entries/<n>-<kind>.json` (`n` from `0001`), and keeps every earlier
//! file byte for byte: an `ack`, a `backbrief`, an `answer` or a `close`. The id is the base32 of
//! 80 bits of the sha256 of the canonical JSON of the sender actor (name and session), `who`,
//! `kind`, the target, `what`, `follow_up` and `supersedes`, never `sent` (T7): a retry of one ask
//! is the same id, and the same body under it is a no-op. `sent` is the root commit's time.
//!
//! A talk carries one `what` and exactly one target field for its kind, with one value; bay judges
//! that by structure alone and never reads the prose (BACKLOG 9b). The sender's bay, the host
//! hook and the recipient's accept judge one body by the same rules ([`check_body`]).
//!
//! Who writes what (T1, T10): only an agent key sends, as the registry actor it is enrolled for;
//! only `who` acks, backbriefs and closes; only `who` or `decides` answers a discovery talk, and a
//! second, different answer names the standing one in `supersedes` or is `TALK_REFUSED`;
//! only the sender (or `decides`) supersedes a talk, and the recipient's accept closes the
//! superseded one as `superseded`, naming its successor. A delivery talk about a change closes as
//! `answered` once a landing that names the change (`Fixes:`, `Reverts:` or `Supersedes:`) is on
//! trunk, naming that landing (G148). One sender holds at most
//! [`BAY_TALK_OPEN_MAX`] open talks to one recipient, and a reviewer one open talk per change:
//! its later finding on that change supersedes the open one (G149b, [`open_findings`]). A talk
//! read after its `expires` (its ack's commit time is later) closes as `expired` and owes no
//! backbrief (G52, ruling T3). `land` and `undo` name the open talks on their change (G56,
//! [`open_on`]).
//!
//! Delivery is the recipient's fetch and accept ([`receive`]), which any fetching bay command on
//! the recipient's machine runs (`bay status`, `bay status --since`): it writes the ack on the same chain,
//! signed by the recipient's key. A push is not delivery, and a fetch without accept is not an
//! ack. The sender sees the ack after its own fetch.

mod accept;
mod judge;
mod lander;
mod open;
pub mod poll;
mod send;
pub mod standing;
pub mod wake;

pub use accept::{end_questions, receive};
pub use judge::{Push, judge};
pub use open::{name_open_talks, open_on};
pub use send::{Answer, Ask, answer, talk};

use std::fmt::Write as _;
use std::path::Path;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::canonical::canonical;
use crate::clock::unix_of;
use crate::git::Git;
use crate::registry::{KeyClass, REGISTRY_REF, Registry};
use crate::result::{BayResult, Reason};
use crate::signing::Signer;

pub const TALK_PREFIX: &str = crate::names::TALK;
pub const TALK_SCHEMA: &str = "bay.talk/1";
pub const TALK_FILE: &str = "talk.json";
pub const ENTRIES_DIR: &str = "entries";
/// The largest canonical body, and the largest entry (`DESIGN` § 11c). A bay limit, never a
/// command-line one.
pub const BAY_TALK_MAX_BYTES: usize = 8192;
/// The most open talks one sender holds to one recipient (ruling T10).
pub const BAY_TALK_OPEN_MAX: usize = 20;
/// Characters in a talk id: 80 bits of base32 (ruling T7).
pub const TALK_ID_LEN: usize = 16;
const ID_BYTES: usize = 10;
/// The most entries one chain takes. Bounds the host hook's work on one talk.
pub const ENTRIES_MAX: usize = 64;
/// The most talks one `git cat-file --batch` pair reads at once. A larger set is read in chunks of
/// this size, so a read's memory is bounded while its count is not: a count cap would lock a
/// recipient out of talk once its closed talks pass it (compaction of closed talks, ruling T6,
/// is what keeps the count small).
pub const TALKS_READ_CHUNK: usize = 512;
/// How far ahead of the reader's clock a `sent` or an entry's `at` may be: clocks differ, and a
/// time further ahead would postpone `stale` and `lost` and reorder the backlog.
pub const BAY_TALK_CLOCK_SKEW_S: u64 = 300;

/// Each kind and the target fields it may name.
/// The talk kinds by name, as `[talk] kinds` names them.
pub const KIND_NAMES: [&str; 3] = ["delivery", "discovery", "reliability"];

const KINDS: [(&str, &[&str]); 3] = [
    ("delivery", &["change", "artifact"]),
    ("discovery", &["decision"]),
    ("reliability", &["check"]),
];
const TARGETS: [&str; 4] = ["change", "artifact", "decision", "check"];
/// `target`, `forecast` and `commitment` are different words, and a talk names at most one.
const RESULT_WORDS: [&str; 3] = ["target", "forecast", "commitment"];
/// Fields on the same talk, not a second record (`DESIGN` § 11c).
const PROMISE_FIELDS: [&str; 4] = ["owns", "promises", "needs", "promise_evidence"];
/// The only status words (`DESIGN` § 11c).
pub const STATUS_WORDS: [&str; 4] = ["ready", "blocked", "validated", "done"];
const FIELDS: [&str; 28] = [
    "schema",
    "id",
    "from",
    "who",
    "kind",
    "what",
    "change",
    "artifact",
    "decision",
    "check",
    "follow_up",
    "sent",
    "decides",
    "context",
    "evidence",
    "supersedes",
    "expires",
    "urgent",
    "target",
    "forecast",
    "commitment",
    "owns",
    "promises",
    "needs",
    "promise_evidence",
    "options",
    "multi_select",
    "context_truncated",
];
/// The prefix of a question talk's `decision` (dossier 63): `question:<tool_use_id>:<n>`, the
/// n-th question of one question tool call. Structure bay judges, never the prose.
pub const QUESTION: &str = "question:";
/// The prefix of a plan talk's `decision`: `plan:<tool_use_id>:0`, a plan to approve.
pub const PLAN: &str = "plan:";

/// Dossier 63 item 4: the value of `talk`'s standing answer when its commit is signed by a key
/// enrolled for the name the answer gives, as `registry` holds it: an approver or registry key
/// (a person), or the agent key of `who` or `decides`. A recipient judges it itself: on a remote
/// with no bay host, no hook judged the push. `None` with no answer, or one not so signed.
#[must_use]
pub fn verified_answer(git: &Git, registry: &Registry, talk: &Talk) -> Option<String> {
    let (commit, entry) = talk.standing_answer(git).ok()??;
    let name = entry["actor"]["name"].as_str()?;
    let value = entry["value"].as_str()?;
    let party = name == talk.who || talk.body["decides"].as_str() == Some(name);
    let classes: &[KeyClass] = if party {
        &[KeyClass::Agent, KeyClass::Approver, KeyClass::Registry]
    } else {
        &[KeyClass::Approver, KeyClass::Registry]
    };
    classes.iter().find_map(|class| {
        let allowed = registry.principal_file(git, *class, name).ok()?;
        (crate::signing::verify_commit(git, &commit, &allowed).ok()?? == name)
            .then(|| value.to_owned())
    })
}

/// Whether `body` asks a person (dossier 63): a discovery talk whose `decision` is a question
/// or a plan. Only such a talk takes the question fields, an `ended` close, and an answer
/// signed by a person (an approver or registry key) as well as by `who` or `decides`.
#[must_use]
pub fn asks_a_person(body: &Value) -> bool {
    body.get("kind").and_then(Value::as_str) == Some("discovery")
        && body
            .get("decision")
            .and_then(Value::as_str)
            .is_some_and(|decision| decision.starts_with(QUESTION) || decision.starts_with(PLAN))
}
/// The most options a question carries, as the question tool takes them.
const MAX_OPTIONS: usize = 4;
/// The fields the id is the digest of (ruling T7): not `sent`, so a retry is the same id.
const ID_FIELDS: [&str; 10] = [
    "from",
    "who",
    "kind",
    "what",
    "follow_up",
    "supersedes",
    "change",
    "artifact",
    "decision",
    "check",
];
/// Each entry kind and the fields it may carry. An ack carries no essay.
const ENTRY_FIELDS: [(&str, &[&str]); 4] = [
    ("ack", &["ack", "wake"]),
    (
        "backbrief",
        &[
            "outcome",
            "first_step",
            "unclear",
            "follow_up",
            "owes",
            "status",
            "reason",
            "evidence",
        ],
    ),
    ("answer", &["value", "supersedes"]),
    (
        "close",
        &[
            "state",
            "by",
            "fixed_by",
            "undone_by",
            "superseded_by",
            "landing",
        ],
    ),
];
/// How a landing answers a delivery talk about a change (G148): each close field and the
/// publication trailer that names the change.
pub(crate) const ANSWERS: [(&str, &str); 3] = [
    ("fixed_by", "Fixes"),
    ("undone_by", "Reverts"),
    ("superseded_by", "Supersedes"),
];
const ENTRY_COMMON: [&str; 5] = ["schema", "entry", "talk", "actor", "at"];

/// G52 (ruling T3): whether a talk read at `read` (Unix seconds) was read after its `expires`.
/// `read` is the time the read is on record at, the ack's commit time, never a later reader's
/// clock: a talk read in time owes its backbrief however late it is looked at.
#[must_use]
pub fn read_after_expiry(read: u64, expires: Option<u64>) -> bool {
    expires.is_some_and(|due| read > due)
}

/// The ref of the talk `id` to `who`.
#[must_use]
pub fn talk_ref(who: &str, id: &str) -> String {
    format!("{TALK_PREFIX}{who}/{id}")
}

/// Whether `id` is a talk id bay could have computed.
#[must_use]
pub fn is_talk_id(id: &str) -> bool {
    id.len() == TALK_ID_LEN
        && crate::ids::from_base32(id).is_some_and(|bytes| bytes.len() == ID_BYTES)
}

/// A talk refused, with its reason code.
#[derive(Debug)]
pub struct Refusal {
    pub reason: Reason,
    pub why: String,
    pub detail: Map<String, Value>,
}

impl Refusal {
    #[must_use]
    pub fn new(reason: Reason, why: impl Into<String>) -> Self {
        Self {
            reason,
            why: why.into(),
            detail: Map::new(),
        }
    }

    #[must_use]
    pub fn invalid(why: impl Into<String>) -> Self {
        Self::new(Reason::TalkRefused, why)
    }

    #[must_use]
    pub fn with(mut self, key: &str, value: Value) -> Self {
        self.detail.insert(key.to_owned(), value);
        self
    }

    #[must_use]
    pub fn result(self) -> BayResult {
        let mut detail = self.detail;
        detail.insert("why".to_owned(), Value::String(self.why));
        BayResult::blocked(self.reason).with_detail(Value::Object(detail))
    }

    /// The refusal as the host hook prints it: the code, then why.
    #[must_use]
    pub fn text(&self) -> String {
        format!("{}: {}", self.reason.code(), self.why)
    }
}

/// CRLF folded to LF (`DESIGN` § 11c), before any check.
#[must_use]
pub fn fold(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// Refuses a control character other than newline and tab, a bare CR among them.
fn plain(field: &str, text: &str) -> Result<(), Refusal> {
    match text
        .chars()
        .find(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    {
        Some(c) => Err(Refusal::invalid(format!(
            "`{field}` holds the control character U+{:04X}; a talk allows newline and tab and no other control character (CRLF folds to LF)",
            u32::from(c)
        ))),
        None => Ok(()),
    }
}

/// The time of a field in bay's form.
fn time_of(field: &str, text: &str) -> Result<u64, Refusal> {
    unix_of(text).ok_or_else(|| {
        Refusal::invalid(format!(
            "`{field}` {text:?} is not a UTC time in the form 2026-10-19T12:00:00Z"
        ))
    })
}

/// The talk id of a body: see the module doc.
///
/// # Errors
///
/// A number in the body that is not finite.
pub fn talk_id(body: &Value) -> Result<String, String> {
    let input: Map<String, Value> = ID_FIELDS
        .iter()
        .filter_map(|field| {
            body.get(*field)
                .filter(|value| !value.is_null())
                .map(|value| ((*field).to_owned(), value.clone()))
        })
        .collect();
    let text = canonical(&Value::Object(input))?;
    Ok(crate::ids::base32(
        &Sha256::digest(text.as_bytes())[..ID_BYTES],
    ))
}

/// A body field: absent, or one string with no control character but newline and tab. A list is
/// a bundled ask.
fn text_field<'a>(map: &'a Map<String, Value>, field: &str) -> Result<Option<&'a str>, Refusal> {
    match map.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            plain(field, text)?;
            Ok(Some(text))
        }
        Some(Value::Array(_)) => Err(Refusal::invalid(format!(
            "`{field}` holds a list: a list in a field is a bundled ask; send one talk per result"
        ))),
        Some(_) => Err(Refusal::invalid(format!("`{field}` is not a string"))),
    }
}

fn required<'a>(map: &'a Map<String, Value>, field: &str) -> Result<&'a str, Refusal> {
    text_field(map, field)?.ok_or_else(|| Refusal::invalid(format!("a talk needs `{field}`")))
}

fn check_from(map: &Map<String, Value>) -> Result<(), Refusal> {
    let from = map
        .get("from")
        .and_then(Value::as_object)
        .ok_or_else(|| Refusal::invalid("a talk needs `from`, the sender actor"))?;
    actor_shape("from", from)
}

/// An actor object: `name` and `session`, non-empty strings, nothing else.
fn actor_shape(field: &str, actor: &Map<String, Value>) -> Result<(), Refusal> {
    let good = actor.len() == 2
        && ["name", "session"].iter().all(|key| {
            actor
                .get(*key)
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty() && plain(field, text).is_ok())
        });
    if good {
        Ok(())
    } else {
        Err(Refusal::invalid(format!(
            "`{field}` is an actor: a `name` and a `session`, nothing else"
        )))
    }
}

/// The one target field of the body's kind, refusing none, two, or one the kind does not own.
fn check_target(map: &Map<String, Value>, kind: &str) -> Result<(), Refusal> {
    let Some((_, owned)) = KINDS.iter().find(|(name, _)| *name == kind) else {
        return Err(Refusal::invalid(format!(
            "the kind {kind:?} is not delivery, discovery or reliability"
        )));
    };
    let given: Vec<&str> = TARGETS
        .iter()
        .copied()
        .filter(|field| map.contains_key(*field))
        .collect();
    for field in &given {
        if text_field(map, field)?.is_none_or(|value| value.trim().is_empty()) {
            return Err(Refusal::invalid(format!("the target `{field}` is empty")));
        }
    }
    let names = |fields: &[&str]| {
        fields
            .iter()
            .map(|field| format!("`{field}`"))
            .collect::<Vec<_>>()
            .join(" or ")
    };
    match given.as_slice() {
        [one] if owned.contains(one) => Ok(()),
        [one] => Err(Refusal::invalid(format!(
            "a {kind} talk names its target in {}, not `{one}`",
            names(owned)
        ))),
        [] => Err(Refusal::invalid(format!(
            "a {kind} talk needs one target: {}",
            names(owned)
        ))),
        _ => Err(Refusal::invalid(format!(
            "a talk has exactly one target field, and this one names {}: a bundled ask; send one talk per result",
            names(&given).replace(" or ", " and ")
        ))),
    }
}

/// `sent`, `follow_up` after it, and `expires` after `sent` and no later than `follow_up`.
fn check_times(map: &Map<String, Value>) -> Result<(), Refusal> {
    let sent = required(map, "sent")?;
    let sent_at = time_of("sent", sent)?;
    let follow_up = required(map, "follow_up")?;
    let follow_up_at = time_of("follow_up", follow_up)?;
    if follow_up_at <= sent_at {
        return Err(Refusal::invalid(format!(
            "`follow_up` {follow_up} is not after `sent` {sent}"
        )));
    }
    if let Some(expires) = text_field(map, "expires")? {
        let expires_at = time_of("expires", expires)?;
        if expires_at <= sent_at {
            return Err(Refusal::invalid(format!(
                "`expires` {expires} is not after `sent` {sent}"
            )));
        }
        if expires_at > follow_up_at {
            return Err(Refusal::invalid(format!(
                "`expires` {expires} is after `follow_up` {follow_up}"
            )));
        }
    }
    Ok(())
}

/// `target`, `forecast` and `commitment`: at most one, and `done` or `ready` labels neither a
/// target nor a forecast; `owns`, `promises`, `needs` and `promise_evidence` are one string each.
fn check_words(map: &Map<String, Value>) -> Result<(), Refusal> {
    let mut named = Vec::new();
    for field in RESULT_WORDS {
        if let Some(value) = text_field(map, field)? {
            if value.trim().is_empty() {
                return Err(Refusal::invalid(format!("`{field}` is empty")));
            }
            named.push(field);
            let word = value.trim().to_ascii_lowercase();
            if field != "commitment" && (word == "done" || word == "ready") {
                return Err(Refusal::invalid(format!(
                    "`{field}` is {value:?}: `done` and `ready` are status words, and a status word cannot label a target or a forecast"
                )));
            }
        }
    }
    if let [_, _, ..] = named.as_slice() {
        return Err(Refusal::invalid(format!(
            "a talk names at most one of `target`, `forecast` and `commitment`, and this one names `{}`",
            named.join("` and `")
        )));
    }
    for field in PROMISE_FIELDS {
        text_field(map, field)?;
    }
    Ok(())
}

/// The structural rules of a `bay.talk/1` body (`DESIGN` § 11c): what the sender's bay, the host
/// hook and the recipient's accept each check. The registry rules are [`check_actors`].
///
/// # Errors
///
/// The first rule the body breaks, as `TALK_REFUSED`.
pub fn check_body(body: &Value) -> Result<(), Refusal> {
    let Some(map) = body.as_object() else {
        return Err(Refusal::invalid("a talk is a JSON object"));
    };
    if map.get("schema").and_then(Value::as_str) != Some(TALK_SCHEMA) {
        return Err(Refusal::invalid(format!("not a {TALK_SCHEMA} talk")));
    }
    if let Some(key) = map.keys().find(|key| !FIELDS.contains(&key.as_str())) {
        return Err(Refusal::invalid(format!(
            "`{key}` is not a field of {TALK_SCHEMA}"
        )));
    }
    check_from(map)?;
    let who = required(map, "who")?;
    crate::names::operator_name(who).map_err(|why| Refusal::invalid(format!("`who`: {why}")))?;
    if required(map, "what")?.trim().is_empty() {
        return Err(Refusal::invalid(
            "`what` is empty: a talk starts with the action",
        ));
    }
    check_target(map, required(map, "kind")?)?;
    check_times(map)?;
    if text_field(map, "decides")?.is_some_and(|name| name.trim().is_empty()) {
        return Err(Refusal::invalid("`decides` is empty"));
    }
    text_field(map, "context")?;
    text_field(map, "evidence")?;
    check_words(map)?;
    if let Some(old) = text_field(map, "supersedes")?
        && !is_talk_id(old)
    {
        return Err(Refusal::invalid(format!(
            "`supersedes` {old:?} is not a talk id"
        )));
    }
    if map
        .get("urgent")
        .is_some_and(|urgent| *urgent != Value::Bool(true))
    {
        return Err(Refusal::invalid("`urgent` is true or absent"));
    }
    check_question(map)?;
    let id = required(map, "id")?;
    let computed = talk_id(body).map_err(Refusal::invalid)?;
    if id != computed {
        return Err(Refusal::invalid(format!(
            "`id` {id} is not the talk's content id {computed}"
        )));
    }
    let size = canonical(body).map_err(Refusal::invalid)?.len();
    if size > BAY_TALK_MAX_BYTES {
        return Err(Refusal::invalid(format!(
            "the talk is {size} bytes; a talk is at most BAY_TALK_MAX_BYTES ({BAY_TALK_MAX_BYTES} UTF-8 bytes of canonical JSON), a bay limit"
        ))
        .with("limit", json!("BAY_TALK_MAX_BYTES"))
        .with("max_bytes", json!(BAY_TALK_MAX_BYTES))
        .with("bytes", json!(size)));
    }
    Ok(())
}

/// Dossier 63: `options`, `multi_select` and `context_truncated` belong to a question talk only
/// (a discovery talk whose `decision` is `question:…`). `options` is at most four
/// `{label, description}` objects with a label each; `multi_select` is true or false;
/// `context_truncated` is true or absent.
fn check_question(map: &Map<String, Value>) -> Result<(), Refusal> {
    let own = ["options", "multi_select", "context_truncated"];
    if !asks_a_person(&Value::Object(map.clone())) {
        if let Some(field) = own.iter().find(|field| map.contains_key(**field)) {
            return Err(Refusal::invalid(format!(
                "`{field}` belongs to a question talk: a discovery talk whose decision is {QUESTION}… or {PLAN}…"
            )));
        }
        return Ok(());
    }
    if let Some(options) = map.get("options") {
        let Some(list) = options.as_array().filter(|list| list.len() <= MAX_OPTIONS) else {
            return Err(Refusal::invalid(format!(
                "`options` is a list of at most {MAX_OPTIONS} options"
            )));
        };
        for option in list {
            let fine = option.as_object().is_some_and(|option| {
                option
                    .keys()
                    .all(|key| key == "label" || key == "description")
                    && option
                        .get("label")
                        .and_then(Value::as_str)
                        .is_some_and(|label| !label.trim().is_empty())
                    && option.get("description").is_none_or(Value::is_string)
            });
            if !fine {
                return Err(Refusal::invalid(
                    "each of `options` is {label, description}, with a label",
                ));
            }
            for text in option.as_object().into_iter().flat_map(|o| o.values()) {
                if let Some(text) = text.as_str() {
                    plain("options", text)?;
                }
            }
        }
    }
    if map
        .get("multi_select")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err(Refusal::invalid("`multi_select` is true or false"));
    }
    if map
        .get("context_truncated")
        .is_some_and(|value| *value != Value::Bool(true))
    {
        return Err(Refusal::invalid("`context_truncated` is true or absent"));
    }
    Ok(())
}

/// `who`, `decides` and the sender are registry actors: names the registry's agent file
/// enrols (`DESIGN` § 11c).
///
/// # Errors
///
/// A name the registry does not enrol, as `TALK_REFUSED`; the sender, as
/// `TALK_REFUSED`.
pub fn check_actors(registry: &Registry, body: &Value) -> Result<(), Refusal> {
    for field in ["who", "decides"] {
        if let Some(name) = body[field].as_str()
            && !registry.enrols(KeyClass::Agent, name)
        {
            return Err(Refusal::invalid(format!(
                "`{field}` {name:?} is not a registry actor: {REGISTRY_REF} enrols no agent of that name"
            )));
        }
    }
    let from = body["from"]["name"].as_str().unwrap_or_default();
    match sender_class(registry, body) {
        Some(_) => Ok(()),
        None if registry.enrols(KeyClass::Reviewer, from) => Err(Refusal::new(
            Reason::TalkRefused,
            format!(
                "the sender {from:?} is a reviewer: a reviewer sends only a delivery talk about a change (--kind delivery --change)"
            ),
        )),
        None => Err(Refusal::new(
            Reason::TalkRefused,
            format!("the sender {from:?} is not a registry actor: only an agent sends a talk"),
        )),
    }
}

/// The key class that signs a talk with `body` (G149): an agent's, or, for a `delivery`
/// talk about a change only, a reviewer's. `None` when the sender is neither.
#[must_use]
pub fn sender_class(registry: &Registry, body: &Value) -> Option<KeyClass> {
    let from = body["from"]["name"].as_str().unwrap_or_default();
    if registry.enrols(KeyClass::Agent, from) {
        Some(KeyClass::Agent)
    } else if registry.enrols(KeyClass::Reviewer, from)
        && body["kind"] == "delivery"
        && body["change"].is_string()
    {
        Some(KeyClass::Reviewer)
    } else {
        None
    }
}

/// G149: a reviewer's talk about a change stands on the reviewer's own record: its notes ref
/// holds a `review` record, signed by its reviewer key, about the change's landing on `trunk`
/// (a verified bay publication carrying the change's `Change-Id`). A finding then reaches the
/// author with no agent key, and a reviewer can talk only about what it reviewed.
///
/// # Errors
///
/// No such record, as `TALK_REFUSED`; a git fault as a refusal too.
pub fn reviewed(git: &Git, registry: &Registry, trunk: &str, body: &Value) -> Result<(), Refusal> {
    let from = body["from"]["name"].as_str().unwrap_or_default();
    let change = body["change"].as_str().unwrap_or_default();
    let refuse = |why: String| Refusal::new(Reason::TalkRefused, why);
    let fault = |error: String| refuse(error);
    let commits = git
        .run(&[
            "log",
            "--format=%H%x1f%(trailers:key=Change-Id,valueonly,separator=%x2c)%x1f%(trailers:key=Registry-Id,valueonly,separator=%x2c)%x1f%cn",
            "--fixed-strings",
            &format!("--grep=Change-Id: {change}"),
            trunk,
        ])
        .map_err(|error| fault(error.message()))?;
    let landing = commits.lines().find_map(|line| {
        let fields: Vec<&str> = line.split('\u{1f}').map(str::trim).collect();
        match fields.as_slice() {
            [commit, id, registry_id, committer]
                if *id == change
                    && crate::publish::verify_publication(git, commit, registry_id, committer) =>
            {
                Some((*commit).to_owned())
            }
            _ => None,
        }
    });
    let Some(landing) = landing else {
        return Err(refuse(format!(
            "change {change} has no bay merge on main: a reviewer talks only about a merge it reviewed"
        )));
    };
    let tree = git
        .run(&["rev-parse", &format!("{landing}^{{tree}}")])
        .map_err(|error| fault(error.message()))?;
    let records = crate::evidence::producer_records(git, from, &tree).map_err(fault)?;
    for record in records {
        if record.kind == crate::evidence::KIND_REVIEW
            && record.producer.name == from
            && record.subject.commit == landing
            && record.verifies_in(git, registry).map_err(fault)?
        {
            return Ok(());
        }
    }
    Err(refuse(format!(
        "reviewer {from} holds no review record of its own on {landing}, change {change}'s merge: a reviewer talks only about a merge it reviewed (bay evidence record --kind review)"
    )))
}

/// The structural rules of one entry of kind `kind` on talk `id`.
///
/// # Errors
///
/// The first rule the entry breaks.
pub fn check_entry(kind: &str, id: &str, entry: &Value) -> Result<(), String> {
    let Some((_, own)) = ENTRY_FIELDS.iter().find(|(name, _)| *name == kind) else {
        return Err(format!("`{kind}` is not a talk entry"));
    };
    let Some(map) = entry.as_object() else {
        return Err("an entry is a JSON object".to_owned());
    };
    if let Some(key) = map
        .keys()
        .find(|key| !ENTRY_COMMON.contains(&key.as_str()) && !own.contains(&key.as_str()))
    {
        return Err(format!("`{key}` is not a field of a {kind} entry"));
    }
    if map.get("schema").and_then(Value::as_str) != Some(TALK_SCHEMA)
        || map.get("entry").and_then(Value::as_str) != Some(kind)
        || map.get("talk").and_then(Value::as_str) != Some(id)
    {
        return Err(format!(
            "a {kind} entry of talk {id} names {TALK_SCHEMA}, entry {kind:?} and talk {id}"
        ));
    }
    let actor = map
        .get("actor")
        .and_then(Value::as_object)
        .ok_or_else(|| "an entry names its actor".to_owned())?;
    actor_shape("actor", actor).map_err(|refusal| refusal.why)?;
    if map
        .get("at")
        .and_then(Value::as_str)
        .and_then(unix_of)
        .is_none()
    {
        return Err("an entry carries its time, `at`".to_owned());
    }
    for (key, value) in map {
        if let Value::String(text) = value {
            plain(key, text).map_err(|refusal| refusal.why)?;
        }
    }
    // G53: an ack's `wake` is the wake adapter's answer: a known result and a reason code.
    if let Some(wake) = map.get("wake") {
        let result = wake["result"].as_str().unwrap_or_default();
        let reason_ok = wake["reason"].is_null()
            || wake["reason"].as_str().is_some_and(|code| {
                !code.is_empty() && code.bytes().all(|c| c.is_ascii_uppercase() || c == b'_')
            });
        if !wake::RESULTS.contains(&result)
            || !reason_ok
            || wake.as_object().is_none_or(|w| w.len() > 2)
        {
            return Err("an ack's `wake` names a result (woken, skipped, not_attempted or uncertain) and a reason code".to_owned());
        }
    }
    Ok(())
}

/// One file of a tree, as `git ls-tree -r -l` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct File {
    pub mode: String,
    pub kind: String,
    pub oid: String,
    pub size: Option<u64>,
    pub path: String,
}

/// Every file of `rev`'s tree.
pub(crate) fn files(git: &Git, rev: &str) -> Result<Vec<File>, String> {
    let listing = git
        .run(&["ls-tree", "-r", "-l", rev])
        .map_err(|error| error.message())?;
    listing
        .lines()
        .map(|line| {
            let (entry, path) = line
                .split_once('\t')
                .ok_or_else(|| format!("unexpected ls-tree line {line:?}"))?;
            let words: Vec<&str> = entry.split_whitespace().collect();
            let [mode, kind, oid, size] = words.as_slice() else {
                return Err(format!("unexpected ls-tree line {line:?}"));
            };
            Ok(File {
                mode: (*mode).to_owned(),
                kind: (*kind).to_owned(),
                oid: (*oid).to_owned(),
                size: size.parse().ok(),
                path: path.to_owned(),
            })
        })
        .collect()
}

/// The entry file at `path`: its number and kind, when it is `entries/<nnnn>-<kind>.json`.
pub(crate) fn entry_name(path: &str) -> Option<(usize, &str)> {
    let rest = path
        .strip_prefix(ENTRIES_DIR)?
        .strip_prefix('/')?
        .strip_suffix(".json")?;
    let (number, kind) = rest.split_once('-')?;
    (number.len() == 4 && number.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| Some((number.parse().ok()?, kind)))
        .flatten()
}

/// Entries already read, by blob: the commit that added each, and its JSON.
pub(crate) type Known = std::collections::HashMap<String, (String, Value)>;

/// One entry of a chain.
#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub kind: String,
    pub blob: String,
}

/// One talk as a chain tip holds it.
#[derive(Clone, Debug)]
pub struct Talk {
    pub name: String,
    pub id: String,
    pub who: String,
    pub tip: String,
    pub body: Value,
    pub body_blob: String,
    pub entries: Vec<Entry>,
}

impl Talk {
    /// The talk on ref `name` at `tip`.
    ///
    /// # Errors
    ///
    /// `name` is not a talk ref, the tree is not a talk's, or a git fault.
    pub fn read(git: &Git, name: &str, tip: &str) -> Result<Self, String> {
        read_all(git, &[(name.to_owned(), tip.to_owned())])?
            .pop()
            .ok_or_else(|| format!("{name} could not be read"))
    }

    #[must_use]
    pub fn has(&self, kind: &str) -> bool {
        self.entries.iter().any(|entry| entry.kind == kind)
    }

    #[must_use]
    pub fn from_name(&self) -> &str {
        self.body["from"]["name"].as_str().unwrap_or_default()
    }

    #[must_use]
    pub fn sent(&self) -> u64 {
        self.body["sent"].as_str().and_then(unix_of).unwrap_or(0)
    }

    /// The talk's `expires`, in Unix seconds (G52).
    #[must_use]
    pub fn expires(&self) -> Option<u64> {
        self.body["expires"].as_str().and_then(unix_of)
    }

    /// The latest entry of `kind`, read from `known` (by blob) where it is there.
    pub(crate) fn last_entry(
        &self,
        git: &Git,
        known: &Known,
        kind: &str,
    ) -> Result<Option<Value>, String> {
        let Some(entry) = self.entries.iter().rev().find(|entry| entry.kind == kind) else {
            return Ok(None);
        };
        if let Some((_, value)) = known.get(&entry.blob) {
            return Ok(Some(value.clone()));
        }
        let text = git
            .run(&["cat-file", "blob", &entry.blob])
            .map_err(|error| error.message())?;
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| error.to_string())
    }

    /// G52: whether the talk was read after its `expires`. The read is its ack, judged by the
    /// ack's `at`, which the host holds to the ack's commit time ([`read_after_expiry`]).
    pub(crate) fn expired_known(&self, git: &Git, known: &Known) -> Result<bool, String> {
        let Some(expires) = self.expires() else {
            return Ok(false);
        };
        let read = self
            .last_entry(git, known, "ack")?
            .and_then(|ack| ack["at"].as_str().and_then(unix_of));
        Ok(read.is_some_and(|read| read_after_expiry(read, Some(expires))))
    }

    /// The state the talk's close names (`superseded`, `answered` or `expired`), if it is closed
    /// by a close entry.
    ///
    /// # Errors
    ///
    /// A git fault, or a close entry that is not JSON.
    pub fn closed_as(&self, git: &Git) -> Result<Option<String>, String> {
        Ok(self
            .last_entry(git, &Known::new(), "close")?
            .and_then(|close| close["state"].as_str().map(str::to_owned)))
    }

    /// Closed: a close entry (superseded, answered, or read after its `expires`, G52), or a
    /// discovery talk with an answer (ruling T2). Delivery and reliability talks close on
    /// evidence, which comes with backbriefs (G47).
    #[must_use]
    pub fn closed(&self) -> bool {
        self.has("close") || (self.body["kind"] == "discovery" && self.has("answer"))
    }

    /// Whether `actor` may supersede this talk: its sender's name, or its `decides`.
    #[must_use]
    pub fn superseded_by_allowed(&self, actor: &str) -> bool {
        self.from_name() == actor || self.body["decides"].as_str() == Some(actor)
    }

    /// The talk this one supersedes, if any.
    #[must_use]
    pub fn supersedes(&self) -> Option<&str> {
        self.body["supersedes"].as_str()
    }

    /// The change a delivery talk asks about, if it names one (G148).
    #[must_use]
    pub fn about_change(&self) -> Option<&str> {
        (self.body["kind"] == "delivery")
            .then(|| self.body["change"].as_str())
            .flatten()
    }

    /// The `follow_up` the talk owes now: the latest backbrief's, or the body's.
    ///
    /// # Errors
    ///
    /// A git fault, or a backbrief that is not JSON.
    pub fn follow_up(&self, git: &Git) -> Result<Option<String>, String> {
        self.follow_up_known(git, &Known::new())
    }

    /// [`Self::follow_up`], reading an entry from `known` (by blob) where it is there.
    pub(crate) fn follow_up_known(
        &self,
        git: &Git,
        known: &Known,
    ) -> Result<Option<String>, String> {
        for entry in self
            .entries
            .iter()
            .rev()
            .filter(|entry| entry.kind == "backbrief")
        {
            let value = if let Some((_, value)) = known.get(&entry.blob) {
                value.clone()
            } else {
                let text = git
                    .run(&["cat-file", "blob", &entry.blob])
                    .map_err(|error| error.message())?;
                serde_json::from_str(&text).map_err(|error| error.to_string())?
            };
            if let Some(follow_up) = value["follow_up"].as_str() {
                return Ok(Some(follow_up.to_owned()));
            }
        }
        Ok(self.body["follow_up"].as_str().map(str::to_owned))
    }

    /// The standing answer: the commit that added the latest answer entry, and that entry.
    ///
    /// # Errors
    ///
    /// A git fault, or an answer entry that is not JSON.
    pub fn standing_answer(&self, git: &Git) -> Result<Option<(String, Value)>, String> {
        self.standing_answer_known(git, &Known::new())
    }

    /// [`Self::standing_answer`], taking the entry and its commit from `known` (by blob) where
    /// it is there.
    pub(crate) fn standing_answer_known(
        &self,
        git: &Git,
        known: &Known,
    ) -> Result<Option<(String, Value)>, String> {
        let Some(entry) = self
            .entries
            .iter()
            .rev()
            .find(|entry| entry.kind == "answer")
        else {
            return Ok(None);
        };
        if let Some(found) = known.get(&entry.blob) {
            return Ok(Some(found.clone()));
        }
        let text = git
            .run(&["cat-file", "blob", &entry.blob])
            .map_err(|error| error.message())?;
        let value: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        let commit = git
            .run(&["log", "-1", "--format=%H", &self.tip, "--", &entry.path])
            .map_err(|error| error.message())?;
        Ok(Some((commit, value)))
    }
}

/// The values of trailer `key` on `commit`.
pub(crate) fn trailer_values(git: &Git, commit: &str, key: &str) -> Result<Vec<String>, String> {
    let text = git
        .run(&[
            "log",
            "-1",
            &format!("--format=%(trailers:key={key},valueonly,separator=%x2c)"),
            commit,
        ])
        .map_err(|error| error.message())?;
    Ok(split_values(&text))
}

/// A `separator=%x2c` trailer list, one value each.
pub(crate) fn split_values(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The talks whose refs match `pattern` (a `for-each-ref` pattern).
///
/// # Errors
///
/// A ref that is not a talk, or a git fault.
pub fn talks_matching(git: &Git, pattern: &str) -> Result<Vec<Talk>, String> {
    talks_where(git, pattern, |_| true)
}

/// The talks whose refs match `pattern` and that `keep` keeps, read [`TALKS_READ_CHUNK`] at a
/// time: only the kept ones are held.
///
/// # Errors
///
/// A ref that is not a talk, or a git fault.
pub fn talks_where(
    git: &Git,
    pattern: &str,
    keep: impl Fn(&Talk) -> bool,
) -> Result<Vec<Talk>, String> {
    let listed = git
        .run(&["for-each-ref", "--format=%(refname) %(objectname)", pattern])
        .map_err(|error| error.message())?;
    let refs: Vec<(String, String)> = listed
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(name, tip)| (name.to_owned(), tip.to_owned()))
        .collect();
    let mut kept = Vec::new();
    for chunk in refs.chunks(TALKS_READ_CHUNK) {
        kept.extend(read_all(git, chunk)?.into_iter().filter(|talk| keep(talk)));
    }
    Ok(kept)
}

/// The objects `objects` names, by one `git cat-file --batch`: each one's type and content, or
/// `None` where it is missing.
/// One object a batch read: its type and content, or `None` where it is missing.
pub(crate) type Object = Option<(String, Vec<u8>)>;

pub(crate) fn batch(git: &Git, objects: &[String]) -> Result<Vec<Object>, String> {
    if objects.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = objects.join("\n");
    input.push('\n');
    let out = git
        .run_bytes(&["cat-file", "--batch"], &[], Some(input.as_bytes()))
        .map_err(|error| error.message())?;
    let mut rest = out.as_slice();
    let mut found = Vec::with_capacity(objects.len());
    for object in objects {
        let end = rest
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(|| format!("git cat-file --batch ended before {object}"))?;
        let header = String::from_utf8_lossy(&rest[..end]).into_owned();
        rest = &rest[end + 1..];
        let words: Vec<&str> = header.split(' ').collect();
        let [_, kind, size] = words.as_slice() else {
            found.push(None);
            continue;
        };
        let size: usize = size
            .parse()
            .map_err(|_| format!("git cat-file --batch: {header:?}"))?;
        let content = rest
            .get(..size)
            .ok_or_else(|| format!("git cat-file --batch cut {object} short"))?;
        found.push(Some(((*kind).to_owned(), content.to_vec())));
        rest = rest.get(size + 1..).unwrap_or_default();
    }
    Ok(found)
}

/// The entries of a raw tree object: mode, name and hex object id each. `hash_len` is the object
/// format's raw id length.
pub(crate) fn tree_entries(data: &[u8], hash_len: usize) -> Option<Vec<(String, String, String)>> {
    let mut entries = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let space = rest.iter().position(|byte| *byte == b' ')?;
        let nul = rest.iter().position(|byte| *byte == 0)?;
        let mode = std::str::from_utf8(&rest[..space]).ok()?.to_owned();
        let name = std::str::from_utf8(rest.get(space + 1..nul)?)
            .ok()?
            .to_owned();
        let oid = crate::agent_settings::hex(rest.get(nul + 1..nul + 1 + hash_len)?);
        entries.push((mode, name, oid));
        rest = &rest[nul + 1 + hash_len..];
    }
    Some(entries)
}

/// The talks one push creates, from its `(name, new tip)` pairs, read once: those that do not
/// read as talks are left out here (each is refused where it is judged).
#[must_use]
pub fn read_created(git: &Git, creates: &[(String, String)]) -> Vec<Talk> {
    read_all(git, creates).unwrap_or_else(|_| {
        creates
            .iter()
            .filter_map(|(name, tip)| Talk::read(git, name, tip).ok())
            .collect()
    })
}

/// The talks at these refs and tips, by two `git cat-file --batch` calls whatever their count.
fn read_all(git: &Git, refs: &[(String, String)]) -> Result<Vec<Talk>, String> {
    let roots: Vec<String> = refs
        .iter()
        .map(|(_, tip)| format!("{tip}^{{tree}}"))
        .collect();
    let mut shapes = Vec::with_capacity(refs.len());
    let mut wanted = Vec::new();
    for ((name, tip), root) in refs.iter().zip(batch(git, &roots)?) {
        let not_a_talk = |why: &str| format!("{name} is not a talk: {why}");
        let hash_len = tip.len() / 2;
        let Some(("tree", data)) = root.as_ref().map(|(kind, data)| (kind.as_str(), data)) else {
            return Err(not_a_talk("its tip has no tree"));
        };
        let entries =
            tree_entries(data, hash_len).ok_or_else(|| not_a_talk("its tree does not parse"))?;
        let (mut body, mut list) = (None, None);
        for (mode, file, oid) in entries {
            match (mode.as_str(), file.as_str()) {
                ("100644", TALK_FILE) => body = Some(oid),
                ("40000", ENTRIES_DIR) => list = Some(oid),
                _ => return Err(not_a_talk(&format!("it holds {file}"))),
            }
        }
        let body = body.ok_or_else(|| not_a_talk(&format!("it holds no {TALK_FILE}")))?;
        wanted.push(body.clone());
        if let Some(list) = &list {
            wanted.push(list.clone());
        }
        shapes.push((body, list, hash_len));
    }
    let mut objects = batch(git, &wanted)?.into_iter();
    let mut talks = Vec::with_capacity(refs.len());
    for ((name, tip), (body_blob, list, hash_len)) in refs.iter().zip(shapes) {
        let (who, id) = name
            .strip_prefix(TALK_PREFIX)
            .and_then(|rest| rest.split_once('/'))
            .ok_or_else(|| format!("{name} is not a talk ref"))?;
        let text = objects
            .next()
            .flatten()
            .map(|(_, data)| data)
            .unwrap_or_default();
        let body = serde_json::from_slice(&text)
            .map_err(|error| format!("{name}: {TALK_FILE} is not JSON: {error}"))?;
        let mut entries = Vec::new();
        if list.is_some() {
            let data = objects
                .next()
                .flatten()
                .map(|(_, data)| data)
                .unwrap_or_default();
            for (_, file, blob) in tree_entries(&data, hash_len)
                .ok_or_else(|| format!("{name}: its {ENTRIES_DIR} tree does not parse"))?
            {
                let path = format!("{ENTRIES_DIR}/{file}");
                let (_, kind) = entry_name(&path)
                    .ok_or_else(|| format!("{name} holds {path}, which no talk holds"))?;
                entries.push(Entry {
                    kind: kind.to_owned(),
                    path,
                    blob,
                });
            }
        }
        talks.push(Talk {
            name: name.clone(),
            id: id.to_owned(),
            who: who.to_owned(),
            tip: tip.clone(),
            body,
            body_blob,
            entries,
        });
    }
    Ok(talks)
}

/// The open talks `sender` holds among `talks` (and `more`, the other talks one push brings), a
/// new talk that supersedes `replacing` aside, oldest first, and whether they reach
/// [`BAY_TALK_OPEN_MAX`]: the count and the oldest id. A talk that another one supersedes is
/// replaced on the record (ruling T1), so it is not open even before its recipient closes it.
#[must_use]
pub fn backlog(
    talks: &[Talk],
    more: &[Talk],
    sender: &str,
    replacing: Option<&str>,
) -> Option<(usize, String)> {
    let replaced: Vec<&str> = talks
        .iter()
        .chain(more)
        .filter_map(Talk::supersedes)
        .chain(replacing)
        .collect();
    let mut open: Vec<&Talk> = talks
        .iter()
        .chain(more)
        .filter(|talk| talk.from_name() == sender && !talk.closed())
        .filter(|talk| !replaced.contains(&talk.id.as_str()))
        .collect();
    open.sort_by(|a, b| (a.sent(), &a.id).cmp(&(b.sent(), &b.id)));
    let count = open.len();
    (count >= BAY_TALK_OPEN_MAX).then(|| {
        (
            count,
            open.first().map(|talk| talk.id.clone()).unwrap_or_default(),
        )
    })
}

/// G149b (the review dossier's K10 d): the open talks among `talks` that the reviewer's delivery
/// talk `body` would stand beside. These are its sender's other delivery talks about the same
/// change that are not closed and that no talk supersedes, the one `body` supersedes aside. A
/// reviewer holds at most one open talk per change, so its later finding supersedes the open one.
/// Empty when a talk among `talks` supersedes `body` itself: a replaced finding stands beside none.
#[must_use]
pub fn open_findings<'a>(talks: &[&'a Talk], body: &Value) -> Vec<&'a Talk> {
    let id = body["id"].as_str().unwrap_or_default();
    let from = body["from"]["name"].as_str().unwrap_or_default();
    let Some(change) = body["change"]
        .as_str()
        .filter(|_| body["kind"] == "delivery")
    else {
        return Vec::new();
    };
    let replaced: Vec<&str> = talks
        .iter()
        .copied()
        .filter_map(Talk::supersedes)
        .chain(body["supersedes"].as_str())
        .collect();
    if replaced.contains(&id) {
        return Vec::new();
    }
    talks
        .iter()
        .copied()
        .filter(|talk| talk.id != id && talk.from_name() == from && !talk.closed())
        .filter(|talk| talk.about_change() == Some(change))
        .filter(|talk| !replaced.contains(&talk.id.as_str()))
        .collect()
}

fn write_blob(git: &Git, text: &str) -> Result<String, String> {
    git.run_with(
        &["hash-object", "-w", "--stdin"],
        &[],
        Some(text.as_bytes()),
    )
    .map_err(|error| error.message())
}

fn make_tree(git: &Git, listing: &str) -> Result<String, String> {
    git.run_with(&["mktree"], &[], Some(listing.as_bytes()))
        .map_err(|error| error.message())
}

fn signed_commit(
    git: &Git,
    tree: &str,
    parent: Option<&str>,
    signer: &Signer,
    at: u64,
    message: &str,
) -> Result<String, String> {
    let mut args = signer.config_args();
    args.extend(["commit-tree".to_owned(), tree.to_owned(), "-S".to_owned()]);
    if let Some(parent) = parent {
        args.extend(["-p".to_owned(), parent.to_owned()]);
    }
    args.extend(["-m".to_owned(), message.to_owned()]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let date = format!("{at} +0000");
    let mut envs = signer.identity();
    envs.push(("GIT_AUTHOR_DATE".to_owned(), date.clone()));
    envs.push(("GIT_COMMITTER_DATE".to_owned(), date));
    let envs: Vec<(&str, &str)> = envs
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    git.run_with(&args, &envs, None)
        .map_err(|error| error.message())
}

/// A talk's root commit: `talk.json` alone, the body in canonical form, committed at `sent` and
/// signed by `key` as `name`.
///
/// # Errors
///
/// A body with no `sent`, or a git or signing fault.
pub fn root_commit(git: &Git, body: &Value, key: &Path, name: &str) -> Result<String, String> {
    let blob = write_blob(git, &canonical(body)?)?;
    let tree = make_tree(git, &format!("100644 blob {blob}\t{TALK_FILE}\n"))?;
    let sent = body["sent"]
        .as_str()
        .and_then(unix_of)
        .ok_or("the talk has no `sent` time")?;
    let signer = Signer {
        key: key.to_path_buf(),
        name: name.to_owned(),
    };
    let message = format!(
        "talk {} to {}",
        body["id"].as_str().unwrap_or_default(),
        body["who"].as_str().unwrap_or_default()
    );
    signed_commit(git, &tree, None, &signer, sent, &message)
}

/// The commit that appends `entry` (its `entry` field names its kind) to the chain at `tip`,
/// signed by `key` as `name`: every earlier file kept, one new `entries/<n>-<kind>.json`.
///
/// # Errors
///
/// An entry with no kind, a tip that is not a talk, or a git or signing fault.
pub fn entry_commit(
    git: &Git,
    tip: &str,
    entry: &Value,
    key: &Path,
    name: &str,
) -> Result<String, String> {
    let kind = entry["entry"]
        .as_str()
        .filter(|kind| ENTRY_FIELDS.iter().any(|(own, _)| own == kind))
        .ok_or("an entry names its kind")?;
    let mut body = None;
    let mut listing = String::new();
    let mut count = 0;
    for file in files(git, tip)? {
        if file.path == TALK_FILE {
            body = Some(file.oid);
        } else if let Some(base) = file.path.strip_prefix("entries/") {
            count += 1;
            let _ = writeln!(listing, "100644 blob {}\t{base}", file.oid);
        }
    }
    let body = body.ok_or_else(|| format!("{tip} holds no {TALK_FILE}"))?;
    let blob = write_blob(git, &canonical(entry)?)?;
    let _ = writeln!(listing, "100644 blob {blob}\t{:04}-{kind}.json", count + 1);
    let entries = make_tree(git, &listing)?;
    let tree = make_tree(
        git,
        &format!("100644 blob {body}\t{TALK_FILE}\n040000 tree {entries}\t{ENTRIES_DIR}\n"),
    )?;
    let at = entry["at"]
        .as_str()
        .and_then(unix_of)
        .unwrap_or_else(crate::live::unix_now);
    let signer = Signer {
        key: key.to_path_buf(),
        name: name.to_owned(),
    };
    let talk = entry["talk"].as_str().unwrap_or_default();
    signed_commit(
        git,
        &tree,
        Some(tip),
        &signer,
        at,
        &format!("{kind} {talk}"),
    )
}

#[cfg(test)]
mod tests {
    use super::{BAY_TALK_MAX_BYTES, check_body, entry_name, fold, is_talk_id, talk_id};
    use serde_json::{Value, json};

    fn body() -> Value {
        let mut body = json!({"schema": "bay.talk/1", "from": {"name": "a", "session": "s"},
            "who": "b", "kind": "delivery", "what": "Ship it", "change": "abcd2345",
            "follow_up": "2026-10-19T13:00:00Z", "sent": "2026-10-19T12:00:00Z"});
        body["id"] = json!(talk_id(&body).unwrap());
        body
    }

    #[test]
    fn the_id_is_80_bits_of_the_ask_and_never_the_sent_time() {
        let first = body();
        let id = first["id"].as_str().unwrap();
        assert!(is_talk_id(id), "{id}");
        let mut retry = first.clone();
        retry["sent"] = json!("2026-10-19T12:00:30Z");
        assert_eq!(talk_id(&retry).unwrap(), id);
        let mut context = first.clone();
        context["context"] = json!("more");
        assert_eq!(talk_id(&context).unwrap(), id, "context is not the ask");
        for (field, value) in [
            ("what", json!("Ship it now")),
            ("who", json!("c")),
            ("follow_up", json!("2026-10-19T14:00:00Z")),
            ("from", json!({"name": "a", "session": "t"})),
        ] {
            let mut other = first.clone();
            other[field] = value;
            assert_ne!(talk_id(&other).unwrap(), id, "{field}");
        }
        assert!(!is_talk_id("abcd2345"));
        assert!(!is_talk_id("ABCDEFGHIJKLMNOP"));
    }

    #[test]
    fn a_good_body_passes_and_each_rule_refuses_its_break() {
        check_body(&body()).unwrap();
        let broken = |edit: &dyn Fn(&mut Value)| {
            let mut bad = body();
            edit(&mut bad);
            bad["id"] = json!(talk_id(&bad).unwrap());
            check_body(&bad).unwrap_err().why
        };
        assert!(broken(&|b| b["what"] = json!(" \n")).contains("empty"));
        assert!(broken(&|b| b["artifact"] = json!("x")).contains("exactly one target"));
        assert!(broken(&|b| b["change"] = json!(["x", "y"])).contains("list"));
        assert!(broken(&|b| b["kind"] = json!("chat")).contains("kind"));
        assert!(broken(&|b| b["what"] = json!("a\rb")).contains("control character"));
        assert!(broken(&|b| b["follow_up"] = b["sent"].clone()).contains("after"));
        assert!(broken(&|b| b["extra"] = json!(1)).contains("not a field"));
        assert!(
            broken(&|b| b["context"] = json!("x".repeat(BAY_TALK_MAX_BYTES)))
                .contains("BAY_TALK_MAX_BYTES (8192")
        );
        let mut forged = body();
        forged["id"] = json!("aaaaaaaaaaaaaaaa");
        assert!(check_body(&forged).unwrap_err().why.contains("content id"));
        assert_eq!(fold("a\r\nb"), "a\nb");
    }

    #[test]
    fn entry_files_are_numbered_and_named_by_kind() {
        assert_eq!(entry_name("entries/0001-ack.json"), Some((1, "ack")));
        assert_eq!(entry_name("entries/12-ack.json"), None);
        assert_eq!(entry_name("talk.json"), None);
    }
}
