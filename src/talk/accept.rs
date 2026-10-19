//! Accept and ack (`DESIGN` § 11c): what a fetching bay command does on the recipient's machine.
//!
//! The recipient is this machine's actor, judged by its agent key: the registry must enrol the
//! key for the actor's name, or nothing is accepted here. Accept fetches the actor's talk refs
//! (`refs/bay/talk/<actor>/*`, ruling T6), checks each talk without an ack (the body's rules, the
//! registry actors, the root commit signed by the sender's agent key at `sent`), and writes the ack
//! on the same chain, signed by the recipient's key and naming its session. A talk that fails
//! accept is listed as refused and gets no ack. An accepted talk that supersedes another of this
//! recipient's talks closes that one as `superseded`, naming its successor (ruling T1). Then every
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

use serde_json::{Value, json};

use super::send::{Pushed, agent_key, fetch_matching, push};
use super::{
    TALK_PREFIX, TALK_SCHEMA, Talk, check_actors, check_body, entry_commit, files, talks_matching,
    talks_where,
};
use crate::change::Actor;
use crate::clock::{now_rfc3339, unix_of};
use crate::registry::{KeyClass, Registry};
use crate::repo::Repo;
use crate::signing;

/// Accepts this actor's new talks and acks them; `None` without a key registry, where no talk
/// exists. The value is the outcome: see the module doc.
#[must_use]
pub fn receive(repo: &Repo) -> Option<Value> {
    let registry = match Registry::current(&repo.git) {
        Ok(Some(registry)) => registry,
        Ok(None) => return None,
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
    for talk in talks_matching(&repo.git, &prefix)? {
        if talk.has("ack") {
            continue;
        }
        match verify(repo, registry, &talk, &me.name) {
            Ok(()) => {
                let ack = json!({"schema": TALK_SCHEMA, "entry": "ack", "talk": talk.id,
                    "ack": true, "actor": {"name": me.name, "session": me.session},
                    "at": now_rfc3339()});
                // One talk whose ack cannot be written here (a sibling session moved its ref,
                // a lock) is that talk's failure, not the whole accept's.
                match append(repo, &talk, &ack, &key, &me.name) {
                    Ok(()) => outcome.acked.push(talk.id),
                    Err(why) => outcome.failed.push(json!({"talk": talk.id, "why": why})),
                }
            }
            Err(why) => outcome.refused.push(json!({"talk": talk.id, "why": why})),
        }
    }
    close_superseded(repo, &prefix, &key, &me, &mut outcome)?;
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
/// `talk.json` alone, the same body, signed by the sender's agent key, committed at `sent`.
fn verify(repo: &Repo, registry: &Registry, talk: &Talk, me: &str) -> Result<(), String> {
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
    let agents = registry.class_file(git, KeyClass::Agent)?;
    let from = talk.from_name();
    if signing::verify_commit(git, root, &agents)
        .map_err(|error| error.message())?
        .as_deref()
        != Some(from)
    {
        return Err(format!(
            "{root} is not signed by its sender {from}'s agent key"
        ));
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
