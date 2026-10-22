//! Accept and ack (`DESIGN` § 11c): what a fetching bay command does on the recipient's machine.
//!
//! The recipient is this machine's actor, judged by its agent key: the registry must enrol the
//! key for the actor's name, or nothing is accepted here. Accept fetches the actor's talk refs
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
//! `skipped` with `why`, `not_fetched`, or `error` with `why`: never an empty inbox in disguise),
//! and the lists `acked`, `closed`, `refused` (a talk that fails accept) and `failed` (a talk
//! whose entry could not be written here), and `pushed`. One talk that fails never stops the
//! others.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde_json::{Value, json};

use super::send::{Pushed, agent_key, fetch_matching, push};
use super::{
    ANSWERS, Known, TALK_PREFIX, TALK_SCHEMA, Talk, check_actors, check_body, entry_commit, files,
    open_findings, split_values, talks_matching, talks_where,
};
use crate::change::{Actor, Chain, change_ref};
use crate::clock::{now_rfc3339, unix_of};
use crate::ids::is_change_id;
use crate::registry::{KeyClass, Registry};
use crate::repo::Repo;
use crate::result::Reason;
use crate::signing;

/// Accepts this actor's new talks and acks them; `None` without a key registry, where no talk
/// exists, or one under `[policy] signing = "approvals"` (N-21), where no agent signs a talk.
/// The value is the outcome: see the module doc.
#[must_use]
pub fn receive(repo: &Repo) -> Option<Value> {
    let registry = match Registry::current(&repo.git) {
        Ok(Some(registry)) if registry.signs_all() => registry,
        Ok(_) => return None,
        Err(why) => return Some(Outcome::new("error").why(why).value()),
    };
    Some(
        accept(repo, &registry)
            .unwrap_or_else(|why| Outcome::new("error").why(why))
            .value(),
    )
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

/// The remote's tips under `prefix`, or `None` when the remote did not answer.
fn remote_tips(repo: &Repo, prefix: &str) -> Option<BTreeMap<String, String>> {
    let listed = repo
        .git
        .run(&["ls-remote", repo.remote(), &format!("{prefix}*")])
        .ok()?;
    Some(
        listed
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(tip, name)| (name.to_owned(), tip.to_owned()))
            .collect(),
    )
}

fn accept(repo: &Repo, registry: &Registry) -> Result<Outcome, String> {
    let me = crate::change::current_actor()?;
    if let Err(why) = crate::names::operator_name(&me.name) {
        return Ok(
            Outcome::new("skipped").why(format!("the actor name is not a talk recipient: {why}"))
        );
    }
    let key = match agent_key(&repo.git, registry, &me.name) {
        Ok(key) => key,
        Err(refusal) => return Ok(Outcome::new("skipped").why(refusal.why)),
    };
    let prefix = format!("{TALK_PREFIX}{}/", me.name);
    fetch_sent(repo, &prefix, &me.name)?;
    let Some(remote) = remote_tips(repo, &prefix) else {
        return Ok(Outcome::new("not_fetched")
            .why("the remote did not list this actor's talks".to_owned()));
    };
    fetch_matching(repo, &format!("{prefix}*"));
    let mut outcome = Outcome::new("accepted");
    let talks = talks_matching(&repo.git, &prefix)?;
    for talk in &talks {
        if talk.has("ack") {
            continue;
        }
        match verify(repo, registry, talk, &me.name, &talks) {
            Ok(()) => {
                // G58, G53: the wake adapter is asked once, at accept, and its answer is on the
                // ack, so the sender sees whether its talk woke the recipient.
                let wake = super::wake::wake(repo, talk, &me);
                let ack = json!({"schema": TALK_SCHEMA, "entry": "ack", "talk": talk.id,
                    "ack": true, "actor": {"name": me.name, "session": me.session},
                    "at": now_rfc3339(), "wake": wake});
                // One talk whose ack cannot be written here (a sibling session moved its ref,
                // a lock) is that talk's failure, not the whole accept's.
                match append(repo, talk, &ack, &key, &me.name) {
                    Ok(()) => outcome.acked.push(talk.id.clone()),
                    Err(why) => outcome.failed.push(json!({"talk": talk.id, "why": why})),
                }
            }
            Err(why) => outcome.refused.push(json!({"talk": talk.id, "why": why})),
        }
    }
    close_superseded(repo, &prefix, &key, &me, &mut outcome)?;
    close_expired(repo, &prefix, &key, &me, &mut outcome)?;
    close_answered(repo, &prefix, &key, &me, &mut outcome)?;
    outcome.pushed = push_moved(repo, &prefix, &remote)?;
    Ok(outcome)
}

fn append(
    repo: &Repo,
    talk: &Talk,
    entry: &Value,
    key: &std::path::Path,
    name: &str,
) -> Result<(), String> {
    let commit = entry_commit(&repo.git, &talk.tip, entry, key, name)?;
    repo.git
        .run(&["update-ref", &talk.name, &commit, &talk.tip])
        .map(|_| ())
        .map_err(|error| error.message())
}

/// Accept's check of a talk to `me`: the body's rules, the registry actors, and the root commit:
/// `talk.json` alone, the same body, signed by the sender's agent key, committed at `sent`. A
/// reviewer's talk also stands beside no other open talk of that reviewer about its change, among
/// `talks` (G149b).
fn verify(
    repo: &Repo,
    registry: &Registry,
    talk: &Talk,
    me: &str,
    talks: &[Talk],
) -> Result<(), String> {
    check_body(&talk.body).map_err(|refusal| refusal.text())?;
    check_actors(registry, &talk.body).map_err(|refusal| refusal.text())?;
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
    let time = git
        .run(&["show", "-s", "--format=%ct", root])
        .map_err(|error| error.message())?;
    if time.parse::<u64>().ok() != talk.body["sent"].as_str().and_then(unix_of) {
        return Err(format!("{root} is not committed at the talk's `sent`"));
    }
    Ok(())
}

/// Closes each acked, open talk of this recipient that an acked talk of the same sender (or its
/// `decides`) supersedes.
fn close_superseded(
    repo: &Repo,
    prefix: &str,
    key: &std::path::Path,
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
    key: &std::path::Path,
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
    key: &std::path::Path,
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

/// Fetches, fast-forward only, the talks this actor sent to others, so it sees their acks.
fn fetch_sent(repo: &Repo, own: &str, me: &str) -> Result<(), String> {
    let refspecs: Vec<String> = talks_where(&repo.git, TALK_PREFIX, |talk| {
        !talk.name.starts_with(own) && talk.from_name() == me
    })?
    .into_iter()
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
