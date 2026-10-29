//! `bay talk`: send one talk (ruling R9), or answer a discovery talk (ruling T1).
//!
//! The simplest send is two words, `bay talk <who> "<what>"` (0.6.0, T1): a discovery talk whose
//! decision is its `what`, owed a reply `[talk] follow_up_s` after it is sent (one hour when the
//! policy names none). A target given alone names its kind ([`with_defaults`]).
//!
//! The sender's bay runs every refusal before it commits anything ([`plan`]), and `--dry-run`
//! stops there. The same ask as an open talk from the sender's name to the same `who` is that
//! talk, `already_sent` (T15, [`open_same`]); a closed talk does not hold it ([`past_closed`]).
//! It fetches the recipient's talk refs first, so a retry, a body conflict, a closed talk and the
//! open-talk cap are judged against what the host holds; the host hook judges the same rules
//! again on the push. A talk committed here that the remote cannot take now stays here, and the
//! result says `not_pushed`: it is not on the recipient. The same `bay talk` again pushes it.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::{
    Refusal, TALK_PREFIX, TALK_SCHEMA, Talk, Trust, backlog, check_body, check_parties,
    entry_commit_as, fold, is_talk_id, plain, root_commit_as, talk_id, talk_ref, talks_matching,
    talks_where,
};
use crate::change::Actor;
use crate::clock::rfc3339_of;
use crate::git::{Git, GitError};
use crate::policy::TalkPolicy;
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
    /// A change id or a trunk commit whose lander is the recipient (G77), in place of `who`.
    pub to_change: Option<String>,
    /// A question talk's options, `[{label, description}]` (dossier 63).
    pub options: Option<Value>,
    /// Whether a question takes several of its options.
    pub multi_select: Option<bool>,
    /// The question's `context` was cut to fit the talk.
    pub context_truncated: bool,
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

/// The repository, the trust its talks are judged by, and the acting actor. Under
/// `signing = "all"` the registry is fetched first; under `approvals` (G445) none is read.
fn open(dir: &Path) -> Result<(Repo, Trust, Actor), Stop> {
    let repo = Repo::open(dir)?;
    if crate::registry::signing_mode(&repo.git)? == crate::policy::Signing::All {
        repo.fetch_registry();
    }
    let trust = Trust::current(&repo.git)?.ok_or_else(|| {
        Refusal::invalid(format!(
            "under [policy] signing = \"all\" a talk needs the repository's key registry ({}): the sender and `who` are registry actors",
            crate::names::REGISTRY
        ))
    })?;
    let actor = crate::change::current_actor(&repo.common_dir)?;
    Ok((repo, trust, actor))
}

/// This machine's agent key, when the registry enrols it for `name`: the only key a talk or an
/// entry is signed with (ruling T10).
pub(crate) fn agent_key(git: &Git, registry: &Registry, name: &str) -> Result<PathBuf, Refusal> {
    let not_actor = |why: String| Refusal::new(Reason::TalkRefused, why);
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

/// This machine's approver key and name (`bay.approverKey`, `bay.approverName`), when the
/// registry enrols that key for that name as an approver: a person's key (dossier 63).
fn approver_key(git: &Git, registry: &Registry) -> Option<(PathBuf, String)> {
    let get = |key: &str| {
        git.config_get(key)
            .ok()
            .flatten()
            .filter(|value| !value.is_empty())
    };
    let name = get("bay.approverName")?;
    let key = PathBuf::from(get("bay.approverKey")?);
    let (key_type, blob) = signing::public_key(&key).ok()?;
    registry
        .holds(KeyClass::Approver, &name, &key_type, &blob)
        .then_some((key, name))
}

/// This machine's reviewer key (`bay.reviewerKey`), when the registry enrols it for `name`
/// (G149): the key a reviewer's delivery talk is signed with.
fn reviewer_key(git: &Git, registry: &Registry, name: &str) -> Result<PathBuf, Refusal> {
    let not_actor = |why: String| Refusal::new(Reason::TalkRefused, why);
    let key = git
        .config_get("bay.reviewerKey")
        .ok()
        .flatten()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            not_actor(
                "this machine has no reviewer key (git config bay.reviewerKey): a reviewer's talk is signed by it"
                    .to_owned(),
            )
        })?;
    let (key_type, blob) = signing::public_key(&key).map_err(not_actor)?;
    if registry.holds(KeyClass::Reviewer, name, &key_type, &blob) {
        Ok(key)
    } else {
        Err(not_actor(format!(
            "bay.reviewerKey is not {name}'s reviewer key in the registry"
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
        "who": need("who", &ask.who, "the actor who owes the reply, the first argument")?,
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
    if let Some(options) = &ask.options {
        body["options"] = options.clone();
    }
    if let Some(multi) = ask.multi_select {
        body["multi_select"] = json!(multi);
    }
    if ask.context_truncated {
        body["context_truncated"] = json!(true);
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
/// All or nothing (G328): a talk's refs never reach the host in part.
pub(crate) fn push(repo: &Repo, refspecs: &[String]) -> Pushed {
    let mut args = vec!["push", "-q", "--atomic", repo.remote()];
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
    let (repo, trust, sender) = open(dir)?;
    let Some(to_change) = &ask.to_change else {
        return deliver(&repo, &trust, &sender, ask);
    };
    if ask.who.is_some() {
        return Err(Refusal::invalid(
            "`--to-change` names the recipient (the actor that merged the change): give the action alone, not a recipient beside it",
        )
        .into());
    }
    let lander = super::lander::resolve(&repo, to_change)??;
    let addressed = Ask {
        who: Some(lander.who.clone()),
        ..ask.clone()
    };
    let mut result = deliver(&repo, &trust, &sender, &addressed)?;
    if let Some(detail) = result.detail.as_mut() {
        detail["to_change"] = lander.to_json();
    }
    Ok(result)
}

/// The `[talk]` table of the policy on main as this clone holds main: the default table with no
/// policy, or with one bay cannot read (where it would merge, it is `POLICY_INVALID`).
fn talk_policy(repo: &Repo) -> Result<TalkPolicy, Stop> {
    let Some(main) = repo
        .git
        .ref_target(&repo.trunk_tracking_ref())
        .map_err(|error| error.message())?
    else {
        return Ok(TalkPolicy::default());
    };
    match crate::policy::load_at(&repo.git, &main) {
        Ok(policy) => Ok(policy.map(|policy| policy.talk).unwrap_or_default()),
        Err(crate::policy::LoadError::Git(why)) => Err(why.into()),
        Err(crate::policy::LoadError::Invalid(_)) => Ok(TalkPolicy::default()),
    }
}

/// The two-word send (T1): `ask` with what it leaves out filled in. With no `--kind`, the kind
/// whose target is given (`--change` or `--artifact` delivery, `--check` reliability), else
/// discovery; a discovery talk with no target asks for the decision its `what` names; with no
/// `--follow-up`, the reply is owed `follow_up_s` after `sent`. A field given stays as given.
fn with_defaults(ask: &Ask, sent: u64, follow_up_s: u64) -> Ask {
    let targets = [
        ("change", &ask.change),
        ("artifact", &ask.artifact),
        ("decision", &ask.decision),
        ("check", &ask.check),
    ];
    let given = targets
        .iter()
        .find(|(_, values)| !values.is_empty())
        .map(|(field, _)| *field);
    let kind = ask.kind.clone().unwrap_or_else(|| {
        given
            .and_then(super::kind_of_target)
            .unwrap_or("discovery")
            .to_owned()
    });
    let decision = if kind == "discovery" && given.is_none() {
        ask.what.iter().cloned().collect()
    } else {
        ask.decision.clone()
    };
    Ask {
        kind: Some(kind),
        decision,
        follow_up: ask
            .follow_up
            .clone()
            .or_else(|| Some(rfc3339_of(sent + follow_up_s))),
        ..ask.clone()
    }
}

/// Sends `ask` from `sender`: every refusal first ([`plan`]), then the commit and its push.
fn deliver(repo: &Repo, trust: &Trust, sender: &Actor, ask: &Ask) -> Result<BayResult, Stop> {
    let sent = crate::live::unix_now();
    let filled = with_defaults(ask, sent, talk_policy(repo)?.follow_up_s());
    let plan = plan(repo, trust, sender, &filled, sent, ask.follow_up.is_some())?;
    carry_out(repo, sender, plan, ask.dry_run)
}

/// A send judged by every refusal: the talk the ask already is, or a new talk.
enum Plan {
    /// The open talk with the same ask (T15), or this very talk again: [`resend`] pushes it.
    Held { body: Value, talk: Box<Talk> },
    /// A new talk, signed by `key` (none under approvals).
    New { body: Value, key: Option<PathBuf> },
}

/// Judges `ask` from `sender`, sent at `sent`, by every refusal, committing nothing.
/// `follow_up_given`: the command line named the `follow_up`, so it is part of the body an
/// open talk with the same ask must hold.
fn plan(
    repo: &Repo,
    trust: &Trust,
    sender: &Actor,
    ask: &Ask,
    sent: u64,
    follow_up_given: bool,
) -> Result<Plan, Stop> {
    let mut body = build(ask, sender, sent)?;
    check_body(&body)?;
    check_parties(trust, &body)?;
    let (reviewer, key) = signer(repo, trust, sender, &body)?;
    let who = body["who"].as_str().unwrap_or_default().to_owned();
    fetch_matching(repo, &format!("{TALK_PREFIX}{who}/*"));
    if reviewer {
        body = later_finding(repo, ask, sender, sent, body)?;
    }
    let talks = talks_where(&repo.git, &format!("{TALK_PREFIX}{who}/"), |talk| {
        talk.from_name() == sender.name || talk.supersedes().is_some()
    })?;
    if let Some(open) = open_same(&talks, &body, &sender.name) {
        return held(open, &body, follow_up_given);
    }
    let body = past_closed(&talks, body)?;
    let id = body["id"].as_str().unwrap_or_default().to_owned();
    if let Some(talk) = talks.iter().find(|talk| talk.id == id) {
        // The same id, replaced by a successor whose close is not written yet.
        return Ok(Plan::Held {
            body,
            talk: Box::new(talk.clone()),
        });
    }
    let supersedes = body["supersedes"].as_str();
    if let Some(old) = supersedes {
        may_supersede(repo, old, &sender.name, &talk_ref(&who, &id))?;
    }
    if let Some((open, oldest)) = backlog(&talks, &[], &sender.name, supersedes) {
        return Err(Refusal::new(
            Reason::TalkRefused,
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
    Ok(Plan::New { body, key })
}

/// Whether `sender` talks as a reviewer, and the key that signs its talk: none under approvals
/// (G445), where the sender is its actor name.
fn signer(
    repo: &Repo,
    trust: &Trust,
    sender: &Actor,
    body: &Value,
) -> Result<(bool, Option<PathBuf>), Stop> {
    let Trust::Keys(registry) = trust else {
        return Ok((false, None));
    };
    if super::sender_class(registry, body) == Some(KeyClass::Reviewer) {
        // G149: a reviewer talks only about a landing it reviewed, under its reviewer key.
        let _ = repo.fetch_trunk();
        super::reviewed(&repo.git, registry, &repo.trunk_tracking_ref(), body)?;
        return Ok((true, Some(reviewer_key(&repo.git, registry, &sender.name)?)));
    }
    Ok((false, Some(agent_key(&repo.git, registry, &sender.name)?)))
}

/// T15: the open talk `sender` holds among `talks` (to one `who`) that asks what `body` asks
/// ([`super::same_ask`]), from any session and whatever its `follow_up`. A talk that another one
/// replaced, or that `body` supersedes, holds no ask.
fn open_same<'a>(talks: &'a [Talk], body: &Value, sender: &str) -> Option<&'a Talk> {
    let replaced: Vec<&str> = talks
        .iter()
        .filter_map(Talk::supersedes)
        .chain(body["supersedes"].as_str())
        .collect();
    talks
        .iter()
        .filter(|talk| talk.from_name() == sender && !talk.closed())
        .filter(|talk| !replaced.contains(&talk.id.as_str()))
        .filter(|talk| super::same_ask(&talk.body, body))
        .min_by_key(|talk| (talk.sent(), talk.id.clone()))
}

/// The fields bay writes itself, or that differ between two sends of one ask: no field of the
/// body an open talk with the same ask must hold.
const NOT_ASKED: [&str; 4] = ["id", "sent", "from", "repeats"];

/// T15: `open` asks what `body` asks. The same body is that talk, `already_sent`; a field beside
/// the ask that differs is refused, as a different body under one id is: a talk's body never
/// changes, and a successor changes it. A `follow_up` the command line did not give is never
/// compared: bay's default moves with the clock.
fn held(open: &Talk, body: &Value, follow_up_given: bool) -> Result<Plan, Stop> {
    let given = |body: &Value| {
        let mut given = body.clone();
        if let Some(map) = given.as_object_mut() {
            for field in NOT_ASKED {
                map.remove(field);
            }
            if !follow_up_given {
                map.remove("follow_up");
            }
        }
        given
    };
    if given(&open.body) != given(body) {
        let id = &open.id;
        return Err(Refusal::new(
            Reason::TalkRefused,
            format!(
                "talk {id} to {} asks the same and is open, with a different body: a talk's body never changes; to change it, send it as a successor (--supersedes {id})",
                open.who
            ),
        )
        .with("talk", json!(id))
        .into());
    }
    Ok(Plan::Held {
        body: open.body.clone(),
        talk: Box::new(open.clone()),
    })
}

/// T15: a closed talk does not hold its ask. While `body`'s id names a closed talk among
/// `talks`, the ask is asked again: the body names that talk in `repeats`, which gives it a new
/// id; a third ask names the second, and so on.
fn past_closed(talks: &[Talk], mut body: Value) -> Result<Value, Refusal> {
    let mut repeated = false;
    for _ in 0..=talks.len() {
        let id = body["id"].as_str().unwrap_or_default().to_owned();
        if !talks.iter().any(|talk| talk.id == id && talk.closed()) {
            break;
        }
        body["repeats"] = json!(id);
        body["id"] = json!(talk_id(&body).map_err(Refusal::invalid)?);
        repeated = true;
    }
    if repeated {
        check_body(&body)?;
    }
    Ok(body)
}

/// Carries out a judged send: [`resend`] for the talk the ask already is; else the new talk's
/// commit and push, or nothing on a dry run.
fn carry_out(repo: &Repo, sender: &Actor, plan: Plan, dry_run: bool) -> Result<BayResult, Stop> {
    let (body, key) = match plan {
        Plan::Held { body, talk } => return resend(repo, &body, &talk, dry_run),
        Plan::New { body, key } => (body, key),
    };
    commit_new(repo, sender, &body, key.as_deref(), dry_run)
}

/// The commit of a new talk `body` from `sender`, signed by `key`, and its push; a dry run
/// commits nothing.
fn commit_new(
    repo: &Repo,
    sender: &Actor,
    body: &Value,
    key: Option<&Path>,
    dry_run: bool,
) -> Result<BayResult, Stop> {
    let id = body["id"].as_str().unwrap_or_default().to_owned();
    let who = body["who"].as_str().unwrap_or_default().to_owned();
    let name = talk_ref(&who, &id);
    if dry_run {
        return Ok(BayResult::ok(
            json!({"dry_run": true, "talk": id, "ref": name, "body": body}),
        ));
    }
    let commit = root_commit_as(&repo.git, body, key, &sender.name)?;
    repo.git
        .run(&["update-ref", &name, &commit, ""])
        .map_err(|error| error.message())?;
    let detail = json!({"talk": id, "short": &id[..8], "ref": name, "who": who,
        "kind": body["kind"]});
    let refspecs = with_predecessor(repo, body, format!("{commit}:{name}"))?;
    match push(repo, &refspecs) {
        Pushed::Yes => Ok(sent_result(detail, "sent", None)),
        Pushed::Refused(reason, why) => {
            // The host's refusal is the answer: the talk does not exist, here either.
            let _ = repo.git.run(&["update-ref", "-d", &name, &commit]);
            Err(Refusal::new(
                reason.unwrap_or(Reason::TalkRefused),
                format!("the host refused the talk: {why}"),
            )
            .with("talk", json!(id))
            .into())
        }
        Pushed::NotNow(why) => Ok(sent_result(detail, "not_pushed", Some(why))),
    }
}

/// G149b: a reviewer holds one open talk per change, so its later finding on that change
/// supersedes the open one. With no `--supersedes`, the body names the reviewer's open talk about
/// the change; the same ask as that open talk is a retry of it, under its own `supersedes`. A
/// `--supersedes` that leaves the open talk standing is `TALK_REFUSED`, naming it.
fn later_finding(
    repo: &Repo,
    ask: &Ask,
    sender: &Actor,
    sent: u64,
    body: Value,
) -> Result<Value, Stop> {
    let who = body["who"].as_str().unwrap_or_default();
    let talks = talks_where(&repo.git, &format!("{TALK_PREFIX}{who}/"), |talk| {
        talk.from_name() == sender.name || talk.supersedes().is_some()
    })?;
    let all: Vec<&Talk> = talks.iter().collect();
    let Some(open) = super::open_findings(&all, &body).first().copied() else {
        return Ok(body);
    };
    let change = body["change"].as_str().unwrap_or_default();
    if ask.supersedes.is_some() {
        return Err(Refusal::new(
            Reason::TalkRefused,
            format!(
                "{} holds open talk {} to {who} about change {change}; a reviewer holds one open talk per change: supersede {} (or leave out --supersedes)",
                sender.name, open.id, open.id
            ),
        )
        .with("oldest", json!(open.id))
        .with("open", json!(1))
        .with("max", json!(1))
        .into());
    }
    let unchanged = |body: &Value| {
        let mut ask = ask_of(body);
        if let Some(map) = ask.as_object_mut() {
            map.remove("id");
            map.remove("supersedes");
        }
        ask
    };
    let supersedes = if unchanged(&open.body) == unchanged(&body) {
        open.supersedes().map(str::to_owned)
    } else {
        Some(open.id.clone())
    };
    let later = Ask {
        supersedes,
        ..ask.clone()
    };
    let body = build(&later, sender, sent)?;
    check_body(&body)?;
    Ok(body)
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

/// A talk not closed that `body` is again: the same body is a no-op that pushes what is here; a
/// different body under one id is refused. A closed talk is never asked again here: a new talk
/// repeats it ([`past_closed`]).
fn resend(repo: &Repo, body: &Value, existing: &Talk, dry_run: bool) -> Result<BayResult, Stop> {
    let id = existing.id.clone();
    if ask_of(&existing.body) != ask_of(body) {
        return Err(Refusal::new(
            Reason::TalkRefused,
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
                reason.unwrap_or(Reason::TalkRefused),
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
        Refusal::new(Reason::TalkRefused, why)
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

/// Who answers `talk` here, with the key that signs the answer, and whether it is a person.
/// Dossier 63: a person answers a talk that asks one with their approver key (as they approve),
/// under the name the registry enrols it for; anyone else answers as an agent, by its agent key.
/// G445: under approvals no agent key signs an answer, and `who` or `decides` answers by name; a
/// person still answers with their approver key, as an approval is signed.
fn answerer(
    repo: &Repo,
    trust: &Trust,
    talk: &Talk,
    me: Actor,
) -> Result<(Option<PathBuf>, Actor, bool), Stop> {
    let person = match trust {
        Trust::Keys(registry) => super::asks_a_person(&talk.body)
            .then(|| approver_key(&repo.git, registry))
            .flatten(),
        Trust::Names if super::asks_a_person(&talk.body) => {
            repo.fetch_registry();
            Registry::current(&repo.git)?.and_then(|registry| approver_key(&repo.git, &registry))
        }
        Trust::Names => None,
    };
    if let Some((key, name)) = person {
        let person = Actor {
            name,
            session: me.session,
        };
        return Ok((Some(key), person, true));
    }
    let key = match trust {
        Trust::Keys(registry) => Some(agent_key(&repo.git, registry, &me.name)?),
        Trust::Names => None,
    };
    Ok((key, me, false))
}

fn give_answer(dir: &Path, request: &Answer) -> Result<BayResult, Stop> {
    let (repo, trust, me) = open(dir)?;
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
    let id = request.talk.as_str();
    let talk = find(&repo, id)?
        .ok_or_else(|| Refusal::invalid(format!("no talk {id} is here or on the remote")))?;
    let (key, me, by_person) = answerer(&repo, &trust, &talk, me)?;
    if talk.body["kind"] != "discovery" {
        return Err(Refusal::invalid(format!(
            "only a discovery talk takes an answer; {id} is a {} talk",
            talk.body["kind"].as_str().unwrap_or_default()
        ))
        .into());
    }
    if !by_person && me.name != talk.who && talk.body["decides"].as_str() != Some(me.name.as_str())
    {
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
    let commit = entry_commit_as(&repo.git, &talk.tip, &entry, key.as_deref(), &me.name)?;
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
                reason.unwrap_or(Reason::TalkRefused),
                format!("the host refused the answer: {why}"),
            )
            .with("talk", json!(id))
            .into())
        }
        Pushed::NotNow(why) => Ok(sent_result(detail, "not_pushed", Some(why))),
    }
}

/// `TALK_REFUSED`, naming the standing answer.
pub(crate) fn conflict(id: &str, commit: &str, prior: &Value) -> Refusal {
    let value = prior["value"].as_str().unwrap_or_default();
    Refusal::new(
        Reason::TalkRefused,
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
