//! `bay talk`: send one talk (ruling R9), or answer a discovery talk (ruling T1).
//!
//! The sender's bay runs every refusal before it commits anything, and `--dry-run` stops there.
//! It fetches the recipient's talk refs first, so a retry, a body conflict, a closed talk and the
//! open-talk cap are judged against what the host holds; the host hook judges the same rules
//! again on the push. A talk committed here that the remote cannot take now stays here, and the
//! result says `not_pushed`: it is not on the recipient. The same `bay talk` again pushes it.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::{
    Refusal, TALK_PREFIX, TALK_SCHEMA, Talk, backlog, check_actors, check_body, entry_commit, fold,
    is_talk_id, plain, root_commit, talk_id, talk_ref, talks_matching, talks_where,
};
use crate::change::Actor;
use crate::clock::rfc3339_of;
use crate::git::{Git, GitError};
use crate::registry::{KeyClass, Registry};
use crate::repo::Repo;
use crate::result::{BayResult, Reason};
use crate::signing::{self, Keys};

/// One ask, as the command line gives it. Every field is checked by bay, never by the parser, so
/// a missing or repeated field is a `bay.result/1` refusal.
#[derive(Clone, Debug, Default)]
pub struct Ask {
    pub who: Option<String>,
    pub what: Option<String>,
    pub kind: Option<String>,
    pub change: Vec<String>,
    pub artifact: Vec<String>,
    pub decision: Vec<String>,
    pub check: Vec<String>,
    pub follow_up: Option<String>,
    pub decides: Option<String>,
    pub context: Option<String>,
    pub evidence: Option<String>,
    pub supersedes: Option<String>,
    pub expires: Option<String>,
    pub target: Option<String>,
    pub forecast: Option<String>,
    pub commitment: Option<String>,
    pub owns: Option<String>,
    pub promises: Option<String>,
    pub needs: Option<String>,
    pub promise_evidence: Option<String>,
    pub urgent: bool,
    pub dry_run: bool,
}

/// An answer to a discovery talk: its id, the decision value, and the standing answer's commit
/// it supersedes, if any.
#[derive(Clone, Debug, Default)]
pub struct Answer {
    pub talk: String,
    pub value: Option<String>,
    pub supersedes: Option<String>,
}

/// Why a send stopped: a refusal, or a fault.
enum Stop {
    Refused(Refusal),
    Failed(String),
}

impl From<Refusal> for Stop {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<String> for Stop {
    fn from(why: String) -> Self {
        Self::Failed(why)
    }
}

fn finish(outcome: Result<BayResult, Stop>) -> BayResult {
    match outcome {
        Ok(result) => result,
        Err(Stop::Refused(refusal)) => refusal.result(),
        Err(Stop::Failed(why)) => BayResult::failed(why),
    }
}

#[must_use]
pub fn talk(dir: &Path, ask: &Ask) -> BayResult {
    finish(send(dir, ask))
}

#[must_use]
pub fn answer(dir: &Path, answer: &Answer) -> BayResult {
    finish(give_answer(dir, answer))
}

/// The repository, its registry (fetched first) and the acting actor.
fn open(dir: &Path) -> Result<(Repo, Registry, Actor), Stop> {
    let repo = Repo::open(dir)?;
    repo.fetch_registry();
    let registry = Registry::current(&repo.git)?.ok_or_else(|| {
        Refusal::invalid(
            "a talk needs the repository's key registry (refs/bay/registry): the sender and `who` are registry actors",
        )
    })?;
    let actor = crate::change::current_actor()?;
    Ok((repo, registry, actor))
}

/// This machine's agent key, when the registry enrols it for `name`: the only key a talk or an
/// entry is signed with (ruling T10).
pub(crate) fn agent_key(git: &Git, registry: &Registry, name: &str) -> Result<PathBuf, Refusal> {
    let not_actor = |why: String| Refusal::new(Reason::TalkSenderNotActor, why);
    let key = Keys::load(git).agent.ok_or_else(|| {
        not_actor(
            "this machine has no agent key (git config bay.agentKey): a talk is signed by its sender's agent key"
                .to_owned(),
        )
    })?;
    let (key_type, blob) = signing::public_key(&key).map_err(not_actor)?;
    if registry.holds(KeyClass::Agent, name, &key_type, &blob) {
        Ok(key)
    } else {
        Err(not_actor(format!(
            "bay.agentKey is not {name}'s agent key in the registry: only an agent key sends a talk, as the actor it is enrolled for"
        )))
    }
}

/// The body of `ask` from `from`, sent at `sent`. The field rules are [`check_body`]'s; this
/// refuses only what it cannot write: a missing field, or a field given twice.
fn build(ask: &Ask, from: &Actor, sent: u64) -> Result<Value, Refusal> {
    let need = |field: &str, value: &Option<String>, hint: &str| {
        value
            .as_deref()
            .map(fold)
            .ok_or_else(|| Refusal::invalid(format!("a talk needs `{field}`: {hint}")))
    };
    let mut body = json!({
        "schema": TALK_SCHEMA,
        "from": {"name": from.name, "session": from.session},
        "who": need("who", &ask.who, "the registry actor who owes the reply, the first argument")?,
        "what": need("what", &ask.what, "the action, the second argument")?,
        "kind": need("kind", &ask.kind, "--kind delivery, discovery or reliability")?,
        "follow_up": need("follow_up", &ask.follow_up, "--follow-up <time>, when the reply is owed")?,
        "sent": rfc3339_of(sent),
    });
    for (field, values) in [
        ("change", &ask.change),
        ("artifact", &ask.artifact),
        ("decision", &ask.decision),
        ("check", &ask.check),
    ] {
        match values.as_slice() {
            [] => {}
            [one] => body[field] = json!(fold(one)),
            _ => {
                return Err(Refusal::invalid(format!(
                    "`--{field}` is given {} times: a list in a field is a bundled ask; send one talk per result",
                    values.len()
                )));
            }
        }
    }
    for (field, value) in [
        ("decides", &ask.decides),
        ("context", &ask.context),
        ("evidence", &ask.evidence),
        ("supersedes", &ask.supersedes),
        ("expires", &ask.expires),
        ("target", &ask.target),
        ("forecast", &ask.forecast),
        ("commitment", &ask.commitment),
        ("owns", &ask.owns),
        ("promises", &ask.promises),
        ("needs", &ask.needs),
        ("promise_evidence", &ask.promise_evidence),
    ] {
        if let Some(value) = value {
            body[field] = json!(fold(value));
        }
    }
    if ask.urgent {
        body["urgent"] = json!(true);
    }
    body["id"] = json!(talk_id(&body).map_err(Refusal::invalid)?);
    Ok(body)
}

/// Fetches the remote's refs matching `pattern` (a refspec source with one `*`) into the same
/// names, fast-forward only: a local entry the remote lacks is never overwritten. Best effort.
pub(crate) fn fetch_matching(repo: &Repo, pattern: &str) {
    let _ = repo.git.run(&[
        "fetch",
        "-q",
        "--no-tags",
        repo.remote(),
        &format!("{pattern}:{pattern}"),
    ]);
}

/// How a push went.
pub(crate) enum Pushed {
    Yes,
    /// The host refused it, with the talk code its hook named.
    Refused(Option<Reason>, String),
    /// The remote did not take it now; the reason.
    NotNow(String),
}

/// Pushes `refspecs` to the configured remote.
pub(crate) fn push(repo: &Repo, refspecs: &[String]) -> Pushed {
    let mut args = vec!["push", "-q", repo.remote()];
    args.extend(refspecs.iter().map(String::as_str));
    match repo.git.run(&args) {
        Ok(_) => Pushed::Yes,
        Err(GitError::Failed { stderr, .. })
            if stderr.contains("[remote rejected]") || stderr.contains("[rejected]") =>
        {
            let reason = Reason::ALL.iter().copied().find(|reason| {
                reason.code().starts_with("TALK_") && stderr.contains(reason.code())
            });
            Pushed::Refused(reason, stderr)
        }
        Err(error) => Pushed::NotNow(error.message()),
    }
}

/// The body without `sent`: what a retry of the same ask must match.
fn ask_of(body: &Value) -> Value {
    let mut ask = body.clone();
    if let Some(map) = ask.as_object_mut() {
        map.remove("sent");
    }
    ask
}

fn send(dir: &Path, ask: &Ask) -> Result<BayResult, Stop> {
    let (repo, registry, sender) = open(dir)?;
    let body = build(ask, &sender, crate::live::unix_now())?;
    check_body(&body)?;
    check_actors(&registry, &body)?;
    let key = agent_key(&repo.git, &registry, &sender.name)?;
    let id = body["id"].as_str().unwrap_or_default().to_owned();
    let who = body["who"].as_str().unwrap_or_default().to_owned();
    let name = talk_ref(&who, &id);
    fetch_matching(&repo, &format!("{TALK_PREFIX}{who}/*"));
    if let Some(tip) = repo
        .git
        .ref_target(&name)
        .map_err(|error| error.message())?
    {
        return resend(
            &repo,
            &body,
            &Talk::read(&repo.git, &name, &tip)?,
            ask.dry_run,
        );
    }
    let supersedes = body["supersedes"].as_str();
    if let Some(old) = supersedes {
        may_supersede(&repo, old, &sender.name, &name)?;
    }
    let talks = talks_where(&repo.git, &format!("{TALK_PREFIX}{who}/"), |talk| {
        talk.from_name() == sender.name || talk.supersedes().is_some()
    })?;
    if let Some((open, oldest)) = backlog(&talks, &[], &sender.name, supersedes) {
        return Err(Refusal::new(
            Reason::TalkBacklog,
            format!(
                "{} holds {open} open talks to {who} (BAY_TALK_OPEN_MAX {}); the oldest open one is {oldest}: close or supersede one first",
                sender.name,
                super::BAY_TALK_OPEN_MAX
            ),
        )
        .with("oldest", json!(oldest))
        .with("open", json!(open))
        .with("max", json!(super::BAY_TALK_OPEN_MAX))
        .into());
    }
    if ask.dry_run {
        return Ok(BayResult::ok(
            json!({"dry_run": true, "talk": id, "ref": name, "body": body}),
        ));
    }
    let commit = root_commit(&repo.git, &body, &key, &sender.name)?;
    repo.git
        .run(&["update-ref", &name, &commit, ""])
        .map_err(|error| error.message())?;
    let detail = json!({"talk": id, "short": &id[..8], "ref": name, "who": who,
        "kind": body["kind"]});
    let refspecs = with_predecessor(&repo, &body, format!("{commit}:{name}"))?;
    match push(&repo, &refspecs) {
        Pushed::Yes => Ok(sent_result(detail, "sent", None)),
        Pushed::Refused(reason, why) => {
            // The host's refusal is the answer: the talk does not exist, here either.
            let _ = repo.git.run(&["update-ref", "-d", &name, &commit]);
            Err(Refusal::new(
                reason.unwrap_or(Reason::TalkInvalid),
                format!("the host refused the talk: {why}"),
            )
            .with("talk", json!(id))
            .into())
        }
        Pushed::NotNow(why) => Ok(sent_result(detail, "not_pushed", Some(why))),
    }
}

fn sent_result(mut detail: Value, state: &str, not_pushed: Option<String>) -> BayResult {
    detail["pushed"] = json!(not_pushed.is_none());
    if let Some(why) = not_pushed {
        detail["why"] = json!(format!(
            "the talk is committed here and is not on the recipient: {why}; the same bay talk pushes it"
        ));
    }
    let mut result = BayResult::ok(detail);
    result.state = Some(state.to_owned());
    result
}

/// The same id again: the same ask is a no-op that pushes what is here; a different body is
/// refused; a closed talk is refused (ruling T7).
fn resend(repo: &Repo, body: &Value, existing: &Talk, dry_run: bool) -> Result<BayResult, Stop> {
    let id = existing.id.clone();
    // A closed talk is closed whatever else differs: the id is the ask (ruling T7).
    if existing.closed() {
        return Err(Refusal::new(
            Reason::TalkClosed,
            format!(
                "talk {id} is closed; a repeat of it is refused: change follow_up for a new talk, or supersede it"
            ),
        )
        .with("talk", json!(id))
        .into());
    }
    if ask_of(&existing.body) != ask_of(body) {
        return Err(Refusal::new(
            Reason::TalkBodyConflict,
            format!(
                "talk {id} already holds a different body; a different body under one id is refused: change the action, the target or follow_up for a new talk, or supersede {id}"
            ),
        )
        .with("talk", json!(id))
        .into());
    }
    let detail = json!({"talk": id, "short": &id[..8], "ref": existing.name, "who": existing.who,
        "kind": existing.body["kind"]});
    if dry_run {
        let mut detail = detail;
        detail["dry_run"] = json!(true);
        return Ok(BayResult::ok(detail));
    }
    // It may not be on the host yet; a host that holds more of the chain holds the talk.
    let refspecs = with_predecessor(
        repo,
        &existing.body,
        format!("{}:{}", existing.tip, existing.name),
    )?;
    match push(repo, &refspecs) {
        Pushed::Yes => Ok(sent_result(detail, "already_sent", None)),
        Pushed::Refused(_, why)
            if why.contains("fetch first") || why.contains("non-fast-forward") =>
        {
            Ok(sent_result(detail, "already_sent", None))
        }
        Pushed::Refused(reason, why) => {
            // The host's refusal of a talk it does not hold is the answer, as on a first send:
            // a copy only this machine holds is not a talk, and is not counted here.
            if existing.entries.is_empty() {
                let _ = repo
                    .git
                    .run(&["update-ref", "-d", &existing.name, &existing.tip]);
            }
            Err(Refusal::new(
                reason.unwrap_or(Reason::TalkInvalid),
                format!("the host refused the talk: {why}"),
            )
            .with("talk", json!(id))
            .into())
        }
        Pushed::NotNow(why) => Ok(sent_result(detail, "not_pushed", Some(why))),
    }
}

/// The talk `id`, from any recipient's refs, here or fetched from the remote.
fn find(repo: &Repo, id: &str) -> Result<Option<Talk>, Stop> {
    if !is_talk_id(id) {
        return Err(Refusal::invalid(format!("{id:?} is not a talk id")).into());
    }
    let pattern = format!("{TALK_PREFIX}*/{id}");
    fetch_matching(repo, &pattern);
    Ok(talks_matching(&repo.git, &pattern)?.into_iter().next())
}

/// Only the sender (by name) or `decides` of the talk `old` supersedes it, once, and not once it
/// is closed (ruling T1). `name` is the successor's ref.
fn may_supersede(repo: &Repo, old: &str, sender: &str, name: &str) -> Result<(), Stop> {
    let talk = find(repo, old)?.ok_or_else(|| {
        Refusal::invalid(format!(
            "`supersedes` names talk {old}, which neither this repository nor the remote holds"
        ))
    })?;
    let refused = |why: String| -> Stop {
        Refusal::new(Reason::TalkSupersedeRefused, why)
            .with("talk", json!(old))
            .into()
    };
    if !talk.superseded_by_allowed(sender) {
        return Err(refused(format!(
            "talk {old} was sent by {}; only its sender or its `decides` supersedes it",
            talk.from_name()
        )));
    }
    if talk.has("close") {
        return Err(refused(format!(
            "talk {old} is closed; a closed talk takes no successor"
        )));
    }
    let prefix = format!("{TALK_PREFIX}{}/", talk.who);
    fetch_matching(repo, &format!("{prefix}*"));
    let rivals = talks_where(&repo.git, &prefix, |other| {
        other.supersedes() == Some(old) && other.name != name
    })?;
    match rivals.first() {
        Some(rival) => Err(refused(format!(
            "talk {old} is already superseded by {}; a talk takes one successor: supersede {} instead",
            rival.id, rival.id
        ))),
        None => Ok(()),
    }
}

/// `refspec`, and the talk `body` supersedes when it is this machine's and the remote does not
/// hold it yet: the host judges a successor against what the same push brings.
fn with_predecessor(repo: &Repo, body: &Value, refspec: String) -> Result<Vec<String>, Stop> {
    let mut refspecs = vec![refspec];
    let Some(old) = body["supersedes"].as_str() else {
        return Ok(refspecs);
    };
    let local = talks_matching(&repo.git, &format!("{TALK_PREFIX}*/{old}"))?;
    let Some(talk) = local.first() else {
        return Ok(refspecs);
    };
    let held = repo
        .git
        .run(&["ls-remote", repo.remote(), &talk.name])
        .map(|listed| !listed.trim().is_empty());
    if matches!(held, Ok(false)) {
        refspecs.push(format!("{}:{}", talk.tip, talk.name));
    }
    Ok(refspecs)
}

fn give_answer(dir: &Path, request: &Answer) -> Result<BayResult, Stop> {
    let (repo, registry, me) = open(dir)?;
    let value = fold(
        request
            .value
            .as_deref()
            .ok_or_else(|| Refusal::invalid("an answer needs `--value`, the decision"))?,
    );
    plain("value", &value)?;
    if value.trim().is_empty() {
        return Err(Refusal::invalid("the answer's `--value` is empty").into());
    }
    let key = agent_key(&repo.git, &registry, &me.name)?;
    let id = request.talk.as_str();
    let talk = find(&repo, id)?
        .ok_or_else(|| Refusal::invalid(format!("no talk {id} is here or on the remote")))?;
    if talk.body["kind"] != "discovery" {
        return Err(Refusal::invalid(format!(
            "only a discovery talk takes an answer; {id} is a {} talk",
            talk.body["kind"].as_str().unwrap_or_default()
        ))
        .into());
    }
    if me.name != talk.who && talk.body["decides"].as_str() != Some(me.name.as_str()) {
        return Err(Refusal::invalid(format!(
            "only `who` ({}) or `decides` answers talk {id}",
            talk.who
        ))
        .into());
    }
    let detail = json!({"talk": id, "ref": talk.name});
    let standing = talk.standing_answer(&repo.git)?;
    if let Some((commit, prior)) = &standing {
        if prior["value"].as_str() == Some(value.as_str()) {
            let mut detail = detail;
            detail["answer"] = json!(commit);
            let mut result = BayResult::ok(detail);
            result.state = Some("already_answered".to_owned());
            return Ok(result);
        }
        if request.supersedes.as_deref() != Some(commit.as_str()) {
            return Err(conflict(id, commit, prior).into());
        }
    } else if request.supersedes.is_some() {
        return Err(Refusal::invalid(format!(
            "`--supersedes` names an answer, and talk {id} has none"
        ))
        .into());
    }
    let mut entry = json!({"schema": TALK_SCHEMA, "entry": "answer", "talk": id,
        "actor": {"name": me.name, "session": me.session}, "value": value,
        "at": crate::clock::now_rfc3339()});
    if let Some(commit) = &request.supersedes {
        entry["supersedes"] = json!(commit);
    }
    let commit = entry_commit(&repo.git, &talk.tip, &entry, &key, &me.name)?;
    repo.git
        .run(&["update-ref", &talk.name, &commit, &talk.tip])
        .map_err(|error| error.message())?;
    let mut detail = detail;
    detail["answer"] = json!(commit);
    match push(&repo, &[format!("{commit}:{}", talk.name)]) {
        Pushed::Yes => Ok(sent_result(detail, "answered", None)),
        Pushed::Refused(reason, why) => {
            let _ = repo
                .git
                .run(&["update-ref", &talk.name, &talk.tip, &commit]);
            Err(Refusal::new(
                reason.unwrap_or(Reason::TalkInvalid),
                format!("the host refused the answer: {why}"),
            )
            .with("talk", json!(id))
            .into())
        }
        Pushed::NotNow(why) => Ok(sent_result(detail, "not_pushed", Some(why))),
    }
}

/// `TALK_ANSWER_CONFLICT`, naming the standing answer.
pub(crate) fn conflict(id: &str, commit: &str, prior: &Value) -> Refusal {
    let value = prior["value"].as_str().unwrap_or_default();
    Refusal::new(
        Reason::TalkAnswerConflict,
        format!(
            "talk {id} already has the answer {value:?} ({commit}); a different answer supersedes it (--supersedes {commit})"
        ),
    )
    .with("talk", json!(id))
    .with(
        "standing",
        json!({"commit": commit, "value": value, "actor": prior["actor"]["name"]}),
    )
}
