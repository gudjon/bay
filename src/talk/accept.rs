//! Accept and ack (`DESIGN` § 11c): what a fetching bay command does on the recipient's machine.
//!
//! The recipient is this machine's actor, judged by its agent key: the registry must enrol the
//! key for the actor's name, or nothing is accepted here. Under `[policy] signing = "approvals"`
//! (G445) the recipient and the sender are their names: no key is asked, and the ack is
//! unsigned. Accept fetches the actor's talk refs
//! (`refs/bay/talk/<actor>/*`, ruling T6), checks each talk without an ack (the body's rules, the
//! registry actors, the root commit signed by the sender's agent key at `sent`), and writes the ack
//! on the same chain, signed by the recipient's key and naming its session. A talk that fails
//! accept is listed as refused and gets no ack. An accepted talk that supersedes another of this
//! recipient's talks closes that one as `superseded`, naming its successor (ruling T1). A
//! reviewer's talk about a change that stands beside another open talk of that reviewer about
//! that change, one this box acked or an earlier one, is refused (G149b). An acked
//! delivery talk about a change closes as `answered` once a landing on trunk names that change in
//! `Fixes:`, `Reverts:` or `Supersedes:`, naming the landing (G148). A talk read after its
//! `expires` (its ack's commit time is later) closes as `expired`: the ack keeps arrival a fact,
//! and no backbrief is owed (G52, ruling T3). Then every
//! chain this machine moved is pushed. A push the host refuses (another session acked first)
//! takes the host's chain instead; one the remote cannot take now stays for the next accept.
//!
//! The same pass fetches the talks this actor sent, so the sender sees acks after its own fetch.
//!
//! The outcome, `detail.talk_accept`, has one shape on every path: `state` (`accepted`,
//! `skipped` with `why`, `not_fetched`, `not_due` with `why` (a fetching command inside the
//! poll interval, G445), or `error` with `why`: never an empty inbox in disguise),
//! and the lists `acked`, `closed`, `refused` (a talk that fails accept) and `failed` (a talk
//! whose entry could not be written here), and `pushed`. One talk that fails never stops the
//! others.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde_json::{Value, json};

use super::send::{Pushed, agent_key, fetch_matching, push};
use super::{
    ANSWERS, Known, TALK_PREFIX, TALK_SCHEMA, Talk, Trust, check_body, check_parties,
    entry_commit_as, files, open_findings, split_values, talks_matching, talks_where,
};
use crate::change::{Actor, Chain, change_ref};
use crate::clock::{now_rfc3339, unix_of};
use crate::ids::is_change_id;
use crate::registry::{KeyClass, REGISTRY_REF, Registry};
use crate::repo::Repo;
use crate::result::Reason;
use crate::signing;

/// Accepts this actor's new talks and acks them. Under `[policy] signing = "approvals"` (N-21,
/// G445) no registry is read: talks are judged by name and acked unsigned. Under `all`, `None`
/// without a registry here, where no talk can be judged. The value is the outcome: see the
/// module doc.
#[must_use]
pub fn receive(repo: &Repo) -> Option<Value> {
    receive_with(repo, false)
}

/// [`receive`] for a session's poll ([`super::poll`]): under `all`, a clone that holds no
/// registry fetches it first, at most once per poll interval (F1). A command (`bay status`)
/// never does, so a repository with no registry pays nothing there.
#[must_use]
pub fn receive_polled(repo: &Repo) -> Option<Value> {
    receive_with(repo, true)
}

fn receive_with(repo: &Repo, discover: bool) -> Option<Value> {
    let trust = match Trust::current(&repo.git) {
        Ok(Some(trust)) => trust,
        Ok(None) if discover && super::poll::registry_due(repo, crate::live::unix_now()) => {
            repo.fetch_registry();
            match Trust::current(&repo.git) {
                Ok(Some(trust)) => trust,
                Ok(None) => return None,
                Err(why) => return Some(Outcome::new("error").why(why).value()),
            }
        }
        Ok(None) => return None,
        Err(why) => return Some(Outcome::new("error").why(why).value()),
    };
    Some(
        accept(repo, &trust)
            .unwrap_or_else(|why| Outcome::new("error").why(why))
            .value(),
    )
}

/// A fetching command's outcome inside the poll interval (G445): nothing fetched, the last fetch
/// `now - last` seconds ago. `None` where no talk can exist here, as [`receive`] says.
pub(crate) fn not_due(repo: &Repo, last: u64, now: u64) -> Option<Value> {
    let due = last.saturating_add(super::poll::BAY_TALK_POLL_S);
    matches!(Trust::current(&repo.git), Ok(Some(_))).then(|| {
        Outcome::new("not_due")
            .why(format!(
                "this actor's talks were fetched {} s ago; the next fetch is due in {} s",
                now.saturating_sub(last),
                due.saturating_sub(now)
            ))
            .value()
    })
}

/// F1: `trust` with the registry fetched again when the remote's tip, `remote`, is not the one
/// this clone holds: a sender enrolled since the last fetch is judged on the remote's registry,
/// never refused. The tip comes from the listing accept makes anyway, so an unchanged registry
/// costs no round trip.
fn refreshed(repo: &Repo, trust: Trust, remote: Option<&str>) -> Result<Trust, String> {
    if !matches!(trust, Trust::Keys(_)) {
        return Ok(trust);
    }
    let local = repo
        .git
        .ref_target(REGISTRY_REF)
        .map_err(|error| error.message())?;
    if remote.is_none() || remote == local.as_deref() {
        return Ok(trust);
    }
    repo.fetch_registry();
    Ok(Trust::current(&repo.git)?.unwrap_or(trust))
}

/// What one accept did.
struct Outcome {
    state: &'static str,
    why: Option<String>,
    acked: Vec<String>,
    closed: Vec<Value>,
    refused: Vec<Value>,
    failed: Vec<Value>,
    pushed: bool,
}

impl Outcome {
    fn new(state: &'static str) -> Self {
        Self {
            state,
            why: None,
            acked: Vec::new(),
            closed: Vec::new(),
            refused: Vec::new(),
            failed: Vec::new(),
            pushed: false,
        }
    }

    fn why(mut self, why: String) -> Self {
        self.why = Some(why);
        self
    }

    fn value(self) -> Value {
        let mut value = json!({"state": self.state, "acked": self.acked, "closed": self.closed,
            "refused": self.refused, "failed": self.failed, "pushed": self.pushed});
        if let Some(why) = self.why {
            value["why"] = json!(why);
        }
        value
    }
}

/// The remote's tips under `prefix`, and its registry's tip, in one listing; `None` when the
/// remote did not answer.
fn remote_tips(repo: &Repo, prefix: &str) -> Option<(BTreeMap<String, String>, Option<String>)> {
    let listed = repo
        .git
        .run(&[
            "ls-remote",
            repo.remote(),
            &format!("{prefix}*"),
            REGISTRY_REF,
        ])
        .ok()?;
    let mut tips: BTreeMap<String, String> = listed
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(tip, name)| (name.to_owned(), tip.to_owned()))
        .collect();
    let registry = tips.remove(REGISTRY_REF);
    Some((tips, registry))
}

fn accept(repo: &Repo, trust: &Trust) -> Result<Outcome, String> {
    let me = crate::change::current_actor(&repo.common_dir)?;
    if let Err(why) = crate::names::operator_name(&me.name) {
        return Ok(
            Outcome::new("skipped").why(format!("the actor name is not a talk recipient: {why}"))
        );
    }
    // G445: under approvals no key signs the ack; the recipient is its actor name.
    if let Trust::Keys(registry) = trust
        && let Err(refusal) = agent_key(&repo.git, registry, &me.name)
    {
        return Ok(Outcome::new("skipped").why(refusal.why));
    }
    let prefix = format!("{TALK_PREFIX}{}/", me.name);
    fetch_sent(repo, &prefix, &me.name)?;
    let Some((remote, registry_tip)) = remote_tips(repo, &prefix) else {
        return Ok(Outcome::new("not_fetched")
            .why("the remote did not list this actor's talks".to_owned()));
    };
    let trust = refreshed(repo, trust.clone(), registry_tip.as_deref())?;
    fetch_matching(repo, &format!("{prefix}*"));
    let mut outcome = Outcome::new("accepted");
    let talks = talks_matching(&repo.git, &prefix)?;
    let key = match &trust {
        Trust::Keys(registry) => match agent_key(&repo.git, registry, &me.name) {
            Ok(key) => Some(key),
            Err(refusal) => return Ok(Outcome::new("skipped").why(refusal.why)),
        },
        Trust::Names => None,
    };
    let key = key.as_deref();
    for talk in &talks {
        if talk.has("ack") {
            continue;
        }
        match verify(repo, &trust, talk, &me.name, &talks) {
            Ok(()) => {
                // G58, G53: the wake adapter is asked once, at accept, and its answer is on the
                // ack, so the sender sees whether its talk woke the recipient.
                let wake = super::wake::wake(repo, talk, &me);
                let ack = json!({"schema": TALK_SCHEMA, "entry": "ack", "talk": talk.id,
                    "ack": true, "actor": {"name": me.name, "session": me.session},
                    "at": now_rfc3339(), "wake": wake});
                // One talk whose ack cannot be written here (a sibling session moved its ref,
                // a lock) is that talk's failure, not the whole accept's.
                match append(repo, talk, &ack, key, &me.name) {
                    Ok(()) => outcome.acked.push(talk.id.clone()),
                    Err(why) => outcome.failed.push(json!({"talk": talk.id, "why": why})),
                }
            }
            Err(why) => outcome.refused.push(json!({"talk": talk.id, "why": why})),
        }
    }
    close_superseded(repo, &prefix, key, &me, &mut outcome)?;
    close_expired(repo, &prefix, key, &me, &mut outcome)?;
    close_answered(repo, &prefix, key, &me, &mut outcome)?;
    outcome.pushed = push_moved(repo, &prefix, &remote)?;
    Ok(outcome)
}

fn append(
    repo: &Repo,
    talk: &Talk,
    entry: &Value,
    key: Option<&std::path::Path>,
    name: &str,
) -> Result<(), String> {
    let commit = entry_commit_as(&repo.git, &talk.tip, entry, key, name)?;
    repo.git
        .run(&["update-ref", &talk.name, &commit, &talk.tip])
        .map(|_| ())
        .map_err(|error| error.message())
}

/// Dossier 63: closes the question talks `ids` to this actor as `ended`, once the question tool
/// that asked them ended here (answered at the terminal, or the session went on), and pushes
/// them. A talk already closed, or one this clone does not hold, is passed. Best effort: a close
/// that cannot be written now is written by no later run, and the talk stays open on the remote
/// until its `follow_up`.
pub fn end_questions(repo: &Repo, ids: &[String]) {
    let Ok(Some(trust)) = Trust::current(&repo.git) else {
        return;
    };
    let Ok(me) = crate::change::current_actor(&repo.common_dir) else {
        return;
    };
    let key = match &trust {
        Trust::Keys(registry) => match agent_key(&repo.git, registry, &me.name) {
            Ok(key) => Some(key),
            Err(_) => return,
        },
        Trust::Names => None,
    };
    let prefix = format!("{TALK_PREFIX}{}/", me.name);
    let Some((remote, _)) = remote_tips(repo, &prefix) else {
        return;
    };
    fetch_matching(repo, &format!("{prefix}*"));
    for id in ids {
        let name = crate::talk::talk_ref(&me.name, id);
        let Ok(Some(tip)) = repo.git.ref_target(&name) else {
            continue;
        };
        let Ok(talk) = Talk::read(&repo.git, &name, &tip) else {
            continue;
        };
        if talk.closed() {
            continue;
        }
        let close = json!({"schema": TALK_SCHEMA, "entry": "close", "talk": talk.id,
            "actor": {"name": me.name, "session": me.session}, "state": "ended",
            "at": now_rfc3339()});
        let _ = append(repo, &talk, &close, key.as_deref(), &me.name);
    }
    let _ = push_moved(repo, &prefix, &remote);
}

/// Accept's check of a talk to `me`: the body's rules, its parties, and the root commit:
/// `talk.json` alone, the same body, committed at `sent`, and under `signing = "all"` signed by
/// the sender's agent key. A reviewer's talk also stands beside no other open talk of that
/// reviewer about its change, among `talks` (G149b). Under `approvals` (G445) the sender is its
/// name, and no key is asked.
fn verify(repo: &Repo, trust: &Trust, talk: &Talk, me: &str, talks: &[Talk]) -> Result<(), String> {
    check_body(&talk.body).map_err(|refusal| refusal.text())?;
    check_parties(trust, &talk.body).map_err(|refusal| refusal.text())?;
    if talk.body["who"] != me || talk.body["id"] != talk.id.as_str() || talk.who != me {
        return Err(format!(
            "{} does not hold a talk to {me} with its id",
            talk.name
        ));
    }
    let git = &repo.git;
    let roots = git
        .run(&["rev-list", "--max-parents=0", &talk.tip])
        .map_err(|error| error.message())?;
    let [root] = roots.lines().collect::<Vec<_>>()[..] else {
        return Err(format!("{} has more than one root", talk.name));
    };
    let root_files = files(git, root)?;
    if !matches!(root_files.as_slice(), [file] if file.oid == talk.body_blob) {
        return Err(format!("{root} does not hold the talk's body alone"));
    }
    if let Trust::Keys(registry) = trust {
        signed_by_sender(repo, registry, talk, root, talks)?;
    }
    let time = git
        .run(&["show", "-s", "--format=%ct", root])
        .map_err(|error| error.message())?;
    if time.parse::<u64>().ok() != talk.body["sent"].as_str().and_then(unix_of) {
        return Err(format!("{root} is not committed at the talk's `sent`"));
    }
    Ok(())
}

/// Under `signing = "all"`: `root` is signed by its sender's agent key, or a reviewer's delivery
/// talk by its reviewer key, standing on its review record of the change's landing and beside no
/// other open talk of that reviewer about that change (G149, G149b).
fn signed_by_sender(
    repo: &Repo,
    registry: &Registry,
    talk: &Talk,
    root: &str,
    talks: &[Talk],
) -> Result<(), String> {
    let git = &repo.git;
    let from = talk.from_name();
    // G149: a reviewer's delivery talk is signed by its reviewer key and stands on its review
    // record of the change's landing; every other talk by the sender's agent key.
    let class = super::sender_class(registry, &talk.body).unwrap_or(KeyClass::Agent);
    let signers = registry.class_file(git, class)?;
    if signing::verify_commit(git, root, &signers)
        .map_err(|error| error.message())?
        .as_deref()
        != Some(from)
    {
        return Err(format!(
            "{root} is not signed by its sender {from}'s {} key",
            class.file()
        ));
    }
    if class == KeyClass::Reviewer {
        // The reviewer's record is on its own notes ref, which this box may not hold yet. A
        // reviewer's name is never a producer's or an agent's (Q12), so this is never this
        // box's own ref.
        let _ = repo.fetch_refs(&[crate::evidence::notes_ref(from)]);
        super::reviewed(git, registry, &repo.trunk_tracking_ref(), &talk.body)
            .map_err(|refusal| refusal.text())?;
        // G149b: one open talk per reviewer and change. Of two that a host without bay's hook let
        // through, the one this box already acked, or else the earlier one, stands.
        let all: Vec<&Talk> = talks.iter().collect();
        if let Some(open) = open_findings(&all, &talk.body)
            .into_iter()
            .find(|open| open.has("ack") || (open.sent(), &open.id) < (talk.sent(), &talk.id))
        {
            return Err(format!(
                "{}: reviewer {from} holds open talk {} about change {}; a reviewer holds one open talk per change, and a later finding supersedes it",
                Reason::TalkRefused.code(),
                open.id,
                talk.body["change"].as_str().unwrap_or_default()
            ));
        }
    }
    Ok(())
}

/// Closes each acked, open talk of this recipient that an acked talk of the same sender (or its
/// `decides`) supersedes.
fn close_superseded(
    repo: &Repo,
    prefix: &str,
    key: Option<&std::path::Path>,
    me: &Actor,
    outcome: &mut Outcome,
) -> Result<(), String> {
    let talks = talks_matching(&repo.git, prefix)?;
    for successor in talks.iter().filter(|talk| talk.has("ack")) {
        let Some(old) = successor.body["supersedes"].as_str() else {
            continue;
        };
        let Some(talk) = talks.iter().find(|talk| talk.id == old) else {
            continue;
        };
        if talk.has("close")
            || !talk.has("ack")
            || !talk.superseded_by_allowed(successor.from_name())
        {
            continue;
        }
        let close = json!({"schema": TALK_SCHEMA, "entry": "close", "talk": old,
            "actor": {"name": me.name, "session": me.session}, "state": "superseded",
            "by": successor.id, "at": now_rfc3339()});
        match append(repo, talk, &close, key, &me.name) {
            Ok(()) => outcome
                .closed
                .push(json!({"talk": old, "state": "superseded", "by": successor.id})),
            Err(why) => outcome.failed.push(json!({"talk": old, "why": why})),
        }
    }
    Ok(())
}

/// G52 (ruling T3): closes each acked, open talk of this recipient that was read after its
/// `expires`, as `expired`. The read is the ack, judged by its commit time
/// ([`Talk::expired_known`]), never by this machine's clock now: a talk read in time still owes
/// its backbrief.
fn close_expired(
    repo: &Repo,
    prefix: &str,
    key: Option<&std::path::Path>,
    me: &Actor,
    outcome: &mut Outcome,
) -> Result<(), String> {
    let talks = talks_matching(&repo.git, prefix)?;
    for talk in talks
        .iter()
        .filter(|talk| talk.has("ack") && !talk.closed() && talk.expires().is_some())
    {
        match talk.expired_known(&repo.git, &Known::new()) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(why) => {
                outcome.failed.push(json!({"talk": talk.id, "why": why}));
                continue;
            }
        }
        let close = json!({"schema": TALK_SCHEMA, "entry": "close", "talk": talk.id,
            "actor": {"name": me.name, "session": me.session}, "state": "expired",
            "at": now_rfc3339()});
        match append(repo, talk, &close, key, &me.name) {
            Ok(()) => outcome.closed.push(
                json!({"talk": talk.id, "state": "expired", "expires": talk.body["expires"]}),
            ),
            Err(why) => outcome.failed.push(json!({"talk": talk.id, "why": why})),
        }
    }
    Ok(())
}

/// G148: closes each acked, open delivery talk of this recipient about a change that a landing on
/// trunk names (`Fixes:`, `Reverts:` or `Supersedes:`), as `answered`, naming the first such
/// landing and its relation. Only the trunk this command already fetched is read; a talk whose
/// change chain cannot be read here waits for a later accept.
fn close_answered(
    repo: &Repo,
    prefix: &str,
    key: Option<&std::path::Path>,
    me: &Actor,
    outcome: &mut Outcome,
) -> Result<(), String> {
    let talks = talks_matching(&repo.git, prefix)?;
    let open: Vec<(&Talk, &str)> = talks
        .iter()
        .filter(|talk| talk.has("ack") && !talk.closed())
        .filter_map(|talk| {
            talk.about_change()
                .filter(|change| is_change_id(change))
                .map(|change| (talk, change))
        })
        .collect();
    if open.is_empty() {
        return Ok(());
    }
    let Ok(trunk) = repo
        .git
        .run(&["rev-parse", "--verify", "-q", &repo.trunk_tracking_ref()])
    else {
        return Ok(());
    };
    let mut answers: BTreeMap<&str, Option<Answer>> = BTreeMap::new();
    for (talk, change) in open {
        if !answers.contains_key(change) {
            let found = answering_landing(repo, change, &trunk)?;
            answers.insert(change, found);
        }
        let Some(Some(answer)) = answers.get(change) else {
            continue;
        };
        let mut close = json!({"schema": TALK_SCHEMA, "entry": "close", "talk": talk.id,
            "actor": {"name": me.name, "session": me.session}, "state": "answered",
            "landing": answer.landing, "at": now_rfc3339()});
        close[answer.relation] = json!(answer.by);
        match append(repo, talk, &close, key, &me.name) {
            Ok(()) => {
                let mut closed = json!({"talk": talk.id, "state": "answered",
                    "landing": answer.landing});
                closed[answer.relation] = json!(answer.by);
                outcome.closed.push(closed);
            }
            Err(why) => outcome.failed.push(json!({"talk": talk.id, "why": why})),
        }
    }
    Ok(())
}

/// A landing that answers a change (G148).
struct Answer {
    /// The close field: `fixed_by`, `undone_by` or `superseded_by`.
    relation: &'static str,
    /// The answering change.
    by: String,
    /// Its publication commit.
    landing: String,
}

/// The first executor-verified publication in `base_first..trunk` (first parent, oldest first)
/// whose `Fixes:`, `Reverts:` or `Supersedes:` names `change`. The walk starts at the change's
/// own `base_first`, since anything that names it landed after it did. `None` when none does, or
/// when the change's chain cannot be read here.
fn answering_landing(repo: &Repo, change: &str, trunk: &str) -> Result<Option<Answer>, String> {
    let name = change_ref(change);
    if repo.git.ref_target(&name).ok().flatten().is_none()
        && repo.fetch_refs(std::slice::from_ref(&name)).is_err()
    {
        return Ok(None);
    }
    let Ok((_, record)) = Chain::new(&repo.git).read(change) else {
        return Ok(None);
    };
    let mut shape = "--format=%H%x1f%(trailers:key=Change-Id,valueonly,separator=%x2c)%x1f%(trailers:key=Registry-Id,valueonly,separator=%x2c)%x1f%cn".to_owned();
    for (_, trailer) in ANSWERS {
        let _ = write!(
            shape,
            "%x1f%(trailers:key={trailer},valueonly,separator=%x2c)"
        );
    }
    shape.push_str("%x1e");
    let log = repo
        .git
        .run(&[
            "log",
            "--first-parent",
            "--reverse",
            &shape,
            &format!("{}..{trunk}", record.base_first),
        ])
        .map_err(|error| error.message())?;
    for entry in log.split('\u{1e}') {
        let fields: Vec<&str> = entry.trim().split('\u{1f}').map(str::trim).collect();
        let [commit, by, registry_id, committer, named @ ..] = fields.as_slice() else {
            continue;
        };
        let Some((relation, _)) = ANSWERS
            .iter()
            .zip(named)
            .find(|(_, values)| split_values(values).iter().any(|value| value == change))
            .map(|(answer, _)| *answer)
        else {
            continue;
        };
        if !is_change_id(by)
            || !crate::publish::verify_publication(&repo.git, commit, registry_id, committer)
        {
            continue;
        }
        return Ok(Some(Answer {
            relation,
            by: (*by).to_owned(),
            landing: (*commit).to_owned(),
        }));
    }
    Ok(None)
}

/// Pushes every chain under `prefix` whose local tip is not the remote's. Whether all went.
fn push_moved(
    repo: &Repo,
    prefix: &str,
    remote: &BTreeMap<String, String>,
) -> Result<bool, String> {
    let listed = repo
        .git
        .run(&["for-each-ref", "--format=%(refname) %(objectname)", prefix])
        .map_err(|error| error.message())?;
    let moved: Vec<&str> = listed
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(name, tip)| remote.get(*name).is_none_or(|held| held != tip))
        .map(|(name, _)| name)
        .collect();
    if moved.is_empty() {
        return Ok(true);
    }
    let refspecs: Vec<String> = moved.iter().map(|name| format!("{name}:{name}")).collect();
    match push(repo, &refspecs) {
        Pushed::Yes => Ok(true),
        Pushed::Refused(..) => {
            // The host holds another chain (a sibling session acked first): take it.
            let force: Vec<String> = moved.iter().map(|name| format!("+{name}:{name}")).collect();
            let mut args = vec!["fetch", "-q", "--no-tags", repo.remote()];
            args.extend(force.iter().map(String::as_str));
            let _ = repo.git.run(&args);
            Ok(false)
        }
        Pushed::NotNow(_) => Ok(false),
    }
}

/// The most sent talks one accept fetches: the newest open ones.
const SENT_FETCH_MAX: usize = 64;

/// Fetches, fast-forward only, the talks this actor sent to others that hold no close here, the
/// newest [`SENT_FETCH_MAX`], so it sees their acks and answers. A chain takes no entry after its
/// close, so there is nothing more to fetch; the list stays flat however many talks the actor has
/// sent. An answered discovery talk is still fetched: a later answer may supersede its answer.
fn fetch_sent(repo: &Repo, own: &str, me: &str) -> Result<(), String> {
    let mut sent = talks_where(&repo.git, TALK_PREFIX, |talk| {
        !talk.name.starts_with(own) && talk.from_name() == me && !talk.has("close")
    })?;
    sent.sort_by(|a, b| (b.sent(), &b.id).cmp(&(a.sent(), &a.id)));
    let refspecs: Vec<String> = sent
        .iter()
        .take(SENT_FETCH_MAX)
        .map(|talk| format!("{0}:{0}", talk.name))
        .collect();
    if refspecs.is_empty() {
        return Ok(());
    }
    let mut args = vec!["fetch", "-q", "--no-tags", repo.remote()];
    args.extend(refspecs.iter().map(String::as_str));
    let _ = repo.git.run(&args);
    Ok(())
}
