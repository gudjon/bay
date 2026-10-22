//! The host hook's judge of `refs/bay/talk/<who>/<id>` (`DESIGN` § 10a, § 11c; rulings T1, T7,
//! T10). A talk is created by its sender and only appended to after; nothing deletes it.
//!
//! A create is one parentless commit holding `talk.json` alone, in canonical form, with a body
//! that passes [`check_body`] and [`check_actors`] and names this ref's `who` and id, signed by the
//! agent key of its sender (an executor, producer or registry key is `TALK_REFUSED`), and
//! committed at `sent`, which is no more than [`BAY_TALK_CLOCK_SKEW_S`] ahead of this host's clock.
//! A `supersedes` names a talk this host holds, or this push brings, whose sender, or `decides`,
//! is this sender, that is not closed and has no other successor (`TALK_REFUSED`). The
//! sender's open talks to `who`, the superseded one aside, number fewer than `BAY_TALK_OPEN_MAX`,
//! counting this push's other creates by the same sender (`TALK_REFUSED`, naming the oldest). A
//! reviewer's delivery talk about a change stands beside no other open talk of that reviewer about
//! that change, the one it supersedes aside (G149b, `TALK_REFUSED` naming the open one).
//!
//! An append is one or more commits, each on the one before it, from the old tip; each keeps every
//! file byte for byte (so a different body under the same id is refused) and adds the next
//! `entries/<n>-<kind>.json`, committed at its `at` and signed by the agent key of the actor the
//! entry names: an ack, backbrief or close by `who` alone, one ack per talk; a backbrief names its
//! outcome, first step and what is unclear, any `owes` is a registry actor, any `follow_up` is
//! after `sent`, any `status` is a status word, and a blocked one names a reason code and a new
//! `follow_up`; an answer by `who` or `decides` on a discovery talk, where a different value from
//! the standing answer names that answer's commit in `supersedes` (`TALK_REFUSED`); a
//! close as `superseded` by a talk under the same `who` that supersedes it, or as `answered` by
//! the landing of a change whose `Fixes:`, `Reverts:` or `Supersedes:` names the delivery talk's
//! change (G148), or as `expired` when its ack's commit time is later than its `expires` (G52).
//! A closed talk takes nothing. A chain holds at most [`ENTRIES_MAX`] entries.

use serde_json::Value;

use super::{
    ANSWERS, BAY_TALK_CLOCK_SKEW_S, BAY_TALK_MAX_BYTES, BAY_TALK_OPEN_MAX, ENTRIES_MAX, File,
    Known, STATUS_WORDS, TALK_FILE, TALK_PREFIX, Talk, backlog, check_actors, check_body,
    check_entry, entry_name, files, is_talk_id, open_findings, talk_ref, talks_matching,
    talks_where, trailer_values,
};
use crate::canonical::canonical;
use crate::clock::unix_of;
use crate::git::Git;
use crate::ids::is_change_id;
use crate::registry::{KeyClass, Registry};
use crate::result::Reason;
use crate::signing;

/// What the hook knows of the push a talk ref is judged in.
pub struct Push<'a> {
    pub git: &'a Git,
    pub registry: &'a Registry,
    /// A ref's value after this push.
    pub after: &'a dyn Fn(&str) -> Result<Option<String>, String>,
    /// A marker's value after this push: its ref, or the shard index record that stands in for
    /// it (§ 10a).
    pub marker: &'a dyn Fn(&str) -> Result<Option<String>, String>,
    /// The talks this push creates, those that read as talks ([`super::read_created`]).
    pub creates: &'a [Talk],
}

impl Push<'_> {
    /// The talks this push creates besides the one at `except`. Each is judged on its own.
    fn created(&self, except: &str) -> impl Iterator<Item = &Talk> {
        self.creates.iter().filter(move |talk| talk.name != except)
    }
}

/// Judges one update of `refs/bay/talk/<who>/<id>`: `old` and `new` are `None` for the zero id.
///
/// # Errors
///
/// The refusal, naming the rule (and its talk code, where it has one).
pub fn judge(
    push: &Push,
    old: Option<&str>,
    new: Option<&str>,
    who: &str,
    id: &str,
) -> Result<(), String> {
    let Some(new) = new else {
        return Err("a talk is never deleted".to_owned());
    };
    match old {
        None => create(push, new, who, id),
        Some(old) => appends(push, old, new, who, id),
    }
}

/// Each commit from `old` to `new`, judged as one append on the one before it. [`append`]'s
/// [`ENTRIES_MAX`] bounds how many there are.
///
/// Only the commits this push adds are judged: `old` is this host's own ref, whose entries were
/// judged when they were pushed. Their objects are read by a fixed number of `git cat-file
/// --batch` calls whatever their count, and each new entry's signature is one `ssh-keygen -Y
/// verify` ([`Chain::signed_by`]), so the cost of a push grows with the entries it adds and not
/// with the talk's history.
fn appends(push: &Push, old: &str, new: &str, who: &str, id: &str) -> Result<(), String> {
    let walk = push
        .git
        .run(&[
            "rev-list",
            "--reverse",
            "--parents",
            &format!("{old}..{new}"),
        ])
        .map_err(|error| error.message())?;
    let mut commits: Vec<String> = Vec::new();
    let mut prev = old.to_owned();
    for line in walk.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let [commit, parent] = words.as_slice() else {
            return Err(
                "a talk only appends: each new commit has the one before it as its one parent"
                    .to_owned(),
            );
        };
        if *parent != prev {
            return Err(format!(
                "a talk only appends: {commit} has {prev} as its one parent, or it is refused"
            ));
        }
        commits.push((*commit).to_owned());
        (*commit).clone_into(&mut prev);
    }
    if prev != new {
        return Err(format!(
            "a talk only appends: {new} does not descend from {old}"
        ));
    }
    let talk = Talk::read(push.git, &talk_ref(who, id), old)?;
    // One commit past what the talk still takes is read, so it is refused as the one too many.
    commits.truncate((ENTRIES_MAX + 1).saturating_sub(talk.entries.len()));
    let mut chain = Chain::read(push, talk, old, &commits)?;
    for (index, commit) in commits.iter().enumerate() {
        chain.append(push, index, commit, id)?;
    }
    Ok(())
}

/// One commit as its raw object holds it: its tree, committer time, the signature Git checks
/// for this repository's object format, and the payload that signature is over.
struct Raw {
    tree: String,
    time: Option<u64>,
    signature: Option<Vec<u8>>,
    payload: Vec<u8>,
}

/// A raw commit object, split as Git splits a signed commit for `git verify-commit`
/// (`parse_buffer_signed_by_header`): the signature is the `gpgsig` header of this object
/// format (`gpgsig-sha256` under sha256) with its continuation lines, and the payload is the
/// object without any `gpgsig` header.
fn raw_commit(data: &[u8], sha256: bool) -> Option<Raw> {
    let header: &[u8] = if sha256 { b"gpgsig-sha256" } else { b"gpgsig" };
    let (mut tree, mut time) = (None, None);
    let mut signature: Option<Vec<u8>> = None;
    let mut payload = Vec::with_capacity(data.len());
    let (mut in_signature, mut other_signature) = (false, false);
    let mut rest = data;
    while !rest.is_empty() {
        let end = rest
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(rest.len(), |at| at + 1);
        let line = &rest[..end];
        let sig = if in_signature && line.first() == Some(&b' ') {
            Some(&line[1..])
        } else if line.starts_with(header) && line.get(header.len()) == Some(&b' ') {
            other_signature = false;
            Some(&line[header.len() + 1..])
        } else {
            if line.starts_with(b"gpgsig") {
                other_signature = true;
            } else if other_signature && line.first() != Some(&b' ') {
                other_signature = false;
            }
            None
        };
        if let Some(sig) = sig {
            signature
                .get_or_insert_with(Vec::new)
                .extend_from_slice(sig);
            in_signature = true;
            rest = &rest[end..];
            continue;
        }
        in_signature = false;
        if line.first() == Some(&b'\n') {
            // The header ends: the message is payload whole.
            payload.extend_from_slice(rest);
            break;
        }
        if !other_signature {
            payload.extend_from_slice(line);
        }
        let text = String::from_utf8_lossy(line);
        let text = text.trim_end_matches('\n');
        if let Some(oid) = text.strip_prefix("tree ") {
            tree.get_or_insert_with(|| oid.to_owned());
        } else if let Some(ident) = text.strip_prefix("committer ") {
            // As `%ct`: the number after the ident's last `>`.
            time.get_or_insert_with(|| {
                ident
                    .rsplit_once('>')
                    .and_then(|(_, date)| date.split_whitespace().next())
                    .and_then(|seconds| seconds.parse::<u64>().ok())
            });
        }
        rest = &rest[end..];
    }
    Some(Raw {
        tree: tree?,
        time: time.flatten(),
        signature,
        payload,
    })
}

/// The objects at `oids`, each present, by one `git cat-file --batch`.
fn objects(git: &Git, oids: &[String]) -> Result<Vec<(String, Vec<u8>)>, String> {
    oids.iter()
        .zip(super::batch(git, oids)?)
        .map(|(oid, object)| object.ok_or_else(|| format!("{oid} is missing")))
        .collect()
}

/// Every file of each tree in `trees`, as [`files`] lists it, without sizes, by one `git
/// cat-file --batch` per tree level. A talk is two levels deep (`talk.json`, `entries/<n>`), so
/// a tree below `entries/` is listed as one file of kind `tree`, which no entry is.
fn tree_files(git: &Git, trees: &[String], hash_len: usize) -> Result<Vec<Vec<File>>, String> {
    let mut listed: Vec<Vec<File>> = vec![Vec::new(); trees.len()];
    let mut level: Vec<(usize, String, String)> = trees
        .iter()
        .enumerate()
        .map(|(index, tree)| (index, String::new(), tree.clone()))
        .collect();
    for depth in 0..2 {
        let mut wanted: Vec<String> = level.iter().map(|(_, _, tree)| tree.clone()).collect();
        wanted.sort();
        wanted.dedup();
        let mut read = std::collections::HashMap::new();
        for (oid, (kind, data)) in wanted.iter().zip(objects(git, &wanted)?) {
            let entries = (kind == "tree")
                .then(|| super::tree_entries(&data, hash_len))
                .flatten()
                .ok_or_else(|| format!("TALK_REFUSED: {oid} is not a tree that parses"))?;
            read.insert(oid.clone(), entries);
        }
        let mut next = Vec::new();
        for (index, prefix, tree) in level {
            for (mode, name, oid) in &read[&tree] {
                let path = format!("{prefix}{name}");
                if mode == "40000" && depth == 0 {
                    next.push((index, format!("{path}/"), oid.clone()));
                    continue;
                }
                let kind = match mode.as_str() {
                    "40000" => "tree",
                    "160000" => "commit",
                    _ => "blob",
                };
                listed[index].push(File {
                    mode: if mode == "40000" {
                        "040000".to_owned()
                    } else {
                        mode.clone()
                    },
                    kind: kind.to_owned(),
                    oid: oid.clone(),
                    size: None,
                    path,
                });
            }
        }
        level = next;
    }
    Ok(listed)
}

/// The one file `after` adds to `before`, keeping every other byte for byte.
fn added_of(before: &[File], after: &[File]) -> Result<File, String> {
    if !before.iter().all(|file| after.contains(file)) {
        return Err(
            "a talk only appends: an entry keeps every earlier file, and a different body under the same id is refused"
                .to_owned(),
        );
    }
    let mut extra = after.iter().filter(|file| !before.contains(file));
    match (extra.next(), extra.next()) {
        (Some(file), None) => Ok(file.clone()),
        _ => Err("an append adds exactly one entry".to_owned()),
    }
}

/// A talk's chain as one push appends to it: the talk at the commit before the one judged next,
/// and this push's new commits, read once.
struct Chain {
    talk: Talk,
    raws: Vec<Raw>,
    /// Each new commit's added file (with its size), or why it adds no one file.
    added: Vec<Result<File, String>>,
    /// The text of each added plain file no larger than [`BAY_TALK_MAX_BYTES`], by blob.
    texts: std::collections::HashMap<String, Vec<u8>>,
    /// The entries this push added so far, by blob: their commit and JSON.
    known: Known,
    /// Each actor's own lines of the agent class file, by actor.
    principal_files: std::collections::HashMap<String, std::path::PathBuf>,
}

impl Chain {
    fn read(push: &Push, talk: Talk, old: &str, commits: &[String]) -> Result<Self, String> {
        let git = push.git;
        let sha256 = old.len() == 64;
        let mut wanted = vec![old.to_owned()];
        wanted.extend(commits.iter().cloned());
        let mut raws = Vec::with_capacity(wanted.len());
        for (oid, (kind, data)) in wanted.iter().zip(objects(git, &wanted)?) {
            let raw = (kind == "commit")
                .then(|| raw_commit(&data, sha256))
                .flatten()
                .ok_or_else(|| format!("{oid} is not a commit that parses"))?;
            raws.push(raw);
        }
        let trees: Vec<String> = raws.iter().map(|raw| raw.tree.clone()).collect();
        let listed = tree_files(git, &trees, old.len() / 2)?;
        let mut added: Vec<Result<File, String>> = listed
            .windows(2)
            .map(|pair| added_of(&pair[0], &pair[1]))
            .collect();
        // The sizes of the added plain files, then the text of those within the bay limit.
        let plain: Vec<String> = added
            .iter()
            .filter_map(|file| file.as_ref().ok())
            .filter(|file| file.mode == "100644" && file.kind == "blob")
            .map(|file| file.oid.clone())
            .collect();
        let sizes = batch_check(git, &plain)?;
        for file in added.iter_mut().flatten() {
            file.size = sizes.get(&file.oid).copied();
        }
        let small: Vec<String> = plain
            .into_iter()
            .filter(|oid| {
                sizes
                    .get(oid)
                    .is_some_and(|size| *size <= BAY_TALK_MAX_BYTES as u64)
            })
            .collect();
        let texts = small
            .iter()
            .zip(objects(git, &small)?)
            .map(|(oid, (_, data))| (oid.clone(), data))
            .collect();
        raws.remove(0);
        Ok(Self {
            talk,
            raws,
            added,
            texts,
            known: Known::new(),
            principal_files: std::collections::HashMap::new(),
        })
    }

    /// Whether commit `index` of this push is signed by the agent key enrolled for `actor`:
    /// one `ssh-keygen -Y verify`, in Git's namespace, of the signature Git would check, over
    /// the payload Git would check, against `actor`'s own lines of the agent class file.
    fn signed_by(&mut self, push: &Push, index: usize, actor: &str) -> Result<bool, String> {
        let raw = &self.raws[index];
        let Some(signature) = raw
            .signature
            .as_ref()
            .and_then(|sig| std::str::from_utf8(sig).ok())
        else {
            return Ok(false);
        };
        let file = if let Some(file) = self.principal_files.get(actor) {
            file.clone()
        } else {
            let file = push
                .registry
                .principal_file(push.git, KeyClass::Agent, actor)?;
            self.principal_files.insert(actor.to_owned(), file.clone());
            file
        };
        Ok(signing::verify_data(
            &file,
            actor,
            "git",
            &raw.payload,
            signature,
        ))
    }

    /// The JSON of the file commit `index` adds: canonical text of at most
    /// [`BAY_TALK_MAX_BYTES`], as [`canonical_blob`] judges one.
    fn entry_value(&self, file: &File) -> Result<Value, String> {
        if file.mode != "100644" || file.kind != "blob" {
            return Err(format!("TALK_REFUSED: {} is not a plain file", file.path));
        }
        let Some(data) = self.texts.get(&file.oid) else {
            return Err(format!(
                "TALK_REFUSED: {} is over BAY_TALK_MAX_BYTES ({BAY_TALK_MAX_BYTES} bytes), a bay limit",
                file.path
            ));
        };
        let text = std::str::from_utf8(data)
            .map_err(|_| format!("TALK_REFUSED: {} is not JSON", file.path))?;
        let value: Value = serde_json::from_str(text)
            .map_err(|_| format!("TALK_REFUSED: {} is not JSON", file.path))?;
        if canonical(&value)? != text {
            return Err(format!(
                "TALK_REFUSED: {} is not in RFC 8785 canonical form",
                file.path
            ));
        }
        Ok(value)
    }

    /// Judges commit `index` of this push as one append, then takes it onto the talk.
    fn append(&mut self, push: &Push, index: usize, new: &str, id: &str) -> Result<(), String> {
        let git = push.git;
        let talk = &self.talk;
        if talk.has("close") {
            return Err(format!("talk {id} is closed; it takes no more entries"));
        }
        if talk.entries.len() >= ENTRIES_MAX {
            return Err(format!(
                "talk {id} holds {ENTRIES_MAX} entries, the most one takes"
            ));
        }
        let file = self.added[index].clone()?;
        let next = talk.entries.len() + 1;
        let Some((_, kind)) = entry_name(&file.path).filter(|(number, _)| *number == next) else {
            return Err(format!(
                "an entry of talk {id} is entries/{next:04}-<kind>.json, not {}",
                file.path
            ));
        };
        let kind = kind.to_owned();
        let entry = self.entry_value(&file)?;
        check_entry(&kind, id, &entry)?;
        let at = entry["at"].as_str().and_then(unix_of);
        if self.raws[index].time != at {
            return Err(format!(
                "the {kind} entry's `at` is not its commit time, {new}'s"
            ));
        }
        not_ahead("at", at.unwrap_or_default())?;
        let actor = entry["actor"]["name"].as_str().unwrap_or_default();
        if !self.signed_by(push, index, actor)? {
            return Err(format!(
                "{new} is not signed by the agent key of {actor}, the actor its {kind} entry names"
            ));
        }
        let talk = &self.talk;
        match kind.as_str() {
            "ack" => ack(talk, actor, &entry),
            "backbrief" => backbrief(push, talk, actor, &entry, &self.known),
            "answer" => answer(git, talk, actor, &entry, &self.known),
            "close" => close(push, talk, actor, &entry, &self.known),
            other => Err(format!("`{other}` is not a talk entry")),
        }?;
        self.talk.entries.push(super::Entry {
            path: file.path,
            kind,
            blob: file.oid.clone(),
        });
        new.clone_into(&mut self.talk.tip);
        self.known.insert(file.oid, (new.to_owned(), entry));
        Ok(())
    }
}

/// The size of each blob at `oids`, by one `git cat-file --batch-check`; a missing object or
/// one that is not a blob has none.
fn batch_check(
    git: &Git,
    oids: &[String],
) -> Result<std::collections::HashMap<String, u64>, String> {
    if oids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let mut input = oids.join("\n");
    input.push('\n');
    let out = git
        .run_with(&["cat-file", "--batch-check"], &[], Some(input.as_bytes()))
        .map_err(|error| error.message())?;
    Ok(out
        .lines()
        .filter_map(|line| {
            let words: Vec<&str> = line.split(' ').collect();
            match words.as_slice() {
                [oid, "blob", size] => Some(((*oid).to_owned(), size.parse().ok()?)),
                _ => None,
            }
        })
        .collect())
}

/// The agent a commit verifies as, or `None`.
fn agent_signer(push: &Push, commit: &str) -> Result<Option<String>, String> {
    let file = push.registry.class_file(push.git, KeyClass::Agent)?;
    signing::verify_commit(push.git, commit, &file).map_err(|error| error.message())
}

/// The class other than agent a commit verifies as, if any.
fn other_class(push: &Push, commit: &str) -> Result<Option<&'static str>, String> {
    for class in [
        KeyClass::Executor,
        KeyClass::Producer,
        KeyClass::Reviewer,
        KeyClass::Approver,
        KeyClass::Registry,
    ] {
        let file = push.registry.class_file(push.git, class)?;
        if signing::verify_commit(push.git, commit, &file)
            .map_err(|error| error.message())?
            .is_some()
        {
            return Ok(Some(class.file()));
        }
    }
    Ok(None)
}

/// The JSON of a blob that is canonical text of at most [`BAY_TALK_MAX_BYTES`].
fn canonical_blob(git: &Git, file: &File) -> Result<Value, String> {
    if file.mode != "100644" || file.kind != "blob" {
        return Err(format!("TALK_REFUSED: {} is not a plain file", file.path));
    }
    if file
        .size
        .is_none_or(|size| size > BAY_TALK_MAX_BYTES as u64)
    {
        return Err(format!(
            "TALK_REFUSED: {} is over BAY_TALK_MAX_BYTES ({BAY_TALK_MAX_BYTES} bytes), a bay limit",
            file.path
        ));
    }
    let text = git
        .run(&["cat-file", "blob", &file.oid])
        .map_err(|error| error.message())?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| format!("TALK_REFUSED: {} is not JSON", file.path))?;
    if canonical(&value)? != text {
        return Err(format!(
            "TALK_REFUSED: {} is not in RFC 8785 canonical form",
            file.path
        ));
    }
    Ok(value)
}

/// A commit's committer time.
fn commit_time(git: &Git, commit: &str) -> Result<Option<u64>, String> {
    let time = git
        .run(&["show", "-s", "--format=%ct", commit])
        .map_err(|error| error.message())?;
    Ok(time.parse::<u64>().ok())
}

/// A time a writer set is no further ahead of this host's clock than [`BAY_TALK_CLOCK_SKEW_S`].
fn not_ahead(field: &str, at: u64) -> Result<(), String> {
    let now = crate::live::unix_now();
    if at > now.saturating_add(BAY_TALK_CLOCK_SKEW_S) {
        return Err(format!(
            "TALK_REFUSED: `{field}` is {}s ahead of this host's clock; a talk's times are at most BAY_TALK_CLOCK_SKEW_S ({BAY_TALK_CLOCK_SKEW_S}s) ahead",
            at - now
        ));
    }
    Ok(())
}

/// The sender's key: the agent key of the actor the body names as its sender.
fn sender_key(push: &Push, new: &str, from: &str) -> Result<(), String> {
    match agent_signer(push, new)? {
        Some(signer) if signer == from => Ok(()),
        Some(signer) => Err(format!(
            "TALK_REFUSED: {new} is signed by agent {signer}'s key and names {from} as its sender"
        )),
        None => {
            let by = other_class(push, new)?.map_or_else(
                || "no registry key".to_owned(),
                |class| format!("an {class} key"),
            );
            Err(format!(
                "TALK_REFUSED: {new} is signed by {by}; only the sender's agent key sends a talk"
            ))
        }
    }
}

/// G149: a reviewer's delivery talk is signed by the sender's reviewer key, and stands on its
/// own review record of the change's landing on this host's trunk (`HEAD` of a bay host).
fn reviewer_sender(push: &Push, new: &str, from: &str, body: &Value) -> Result<(), String> {
    let file = push.registry.class_file(push.git, KeyClass::Reviewer)?;
    let signer = signing::verify_commit(push.git, new, &file).map_err(|error| error.message())?;
    if signer.as_deref() != Some(from) {
        return Err(format!(
            "TALK_REFUSED: {new} is not signed by reviewer {from}'s key"
        ));
    }
    super::reviewed(push.git, push.registry, "HEAD", body).map_err(|refusal| refusal.text())
}

fn create(push: &Push, new: &str, who: &str, id: &str) -> Result<(), String> {
    let git = push.git;
    let parents = git
        .run(&["show", "-s", "--format=%P", new])
        .map_err(|error| error.message())?;
    if !parents.trim().is_empty() {
        return Err("TALK_REFUSED: a talk starts as one parentless commit".to_owned());
    }
    let root = files(git, new)?;
    let [file] = root.as_slice() else {
        return Err(format!("TALK_REFUSED: a new talk holds {TALK_FILE} alone"));
    };
    if file.path != TALK_FILE {
        return Err(format!("TALK_REFUSED: a new talk holds {TALK_FILE} alone"));
    }
    let body = canonical_blob(git, file)?;
    check_body(&body).map_err(|refusal| refusal.text())?;
    if body["id"] != id || body["who"] != who {
        return Err(format!(
            "TALK_REFUSED: the ref names talk {id} to {who}, and its body another"
        ));
    }
    check_actors(push.registry, &body).map_err(|refusal| refusal.text())?;
    let from = body["from"]["name"].as_str().unwrap_or_default();
    if super::sender_class(push.registry, &body) == Some(KeyClass::Reviewer) {
        reviewer_sender(push, new, from, &body)?;
    } else {
        sender_key(push, new, from)?;
    }
    let sent = body["sent"].as_str().and_then(unix_of);
    if commit_time(git, new)? != sent {
        return Err("TALK_REFUSED: `sent` is not the talk's commit time".to_owned());
    }
    not_ahead("sent", sent.unwrap_or_default())?;
    let supersedes = body["supersedes"].as_str();
    if let Some(old) = supersedes {
        may_supersede(push, &talk_ref(who, id), old, from)?;
    }
    // Only this sender's talks count, and any talk's `supersedes` replaces one of them.
    let counted = |talk: &Talk| talk.from_name() == from || talk.supersedes().is_some();
    let held = talks_where(git, &format!("{TALK_PREFIX}{who}/"), counted)?;
    let pushed: Vec<Talk> = push
        .created(&talk_ref(who, id))
        .filter(|talk| talk.who == who)
        .cloned()
        .collect();
    if let Some((open, oldest)) = backlog(&held, &pushed, from, supersedes) {
        return Err(format!(
            "TALK_REFUSED: {from} holds {open} open talks to {who} (BAY_TALK_OPEN_MAX {BAY_TALK_OPEN_MAX}); the oldest open one is {oldest}"
        ));
    }
    if super::sender_class(push.registry, &body) == Some(KeyClass::Reviewer) {
        // G149b: a reviewer holds one open talk per change; a later finding supersedes it.
        let talks: Vec<&Talk> = held.iter().chain(&pushed).collect();
        if let Some(open) = open_findings(&talks, &body).first() {
            return Err(format!(
                "TALK_REFUSED: reviewer {from} holds open talk {} to {who} about change {}; a reviewer holds one open talk per change, and a later finding supersedes it",
                open.id,
                body["change"].as_str().unwrap_or_default()
            ));
        }
    }
    Ok(())
}

/// The talk `old` this host holds, or this push brings.
fn held_talk(push: &Push, name: &str, old: &str) -> Result<Option<Talk>, String> {
    if let Some(talk) = talks_matching(push.git, &format!("{TALK_PREFIX}*/{old}"))?
        .into_iter()
        .next()
    {
        return Ok(Some(talk));
    }
    Ok(push.created(name).find(|talk| talk.id == old).cloned())
}

/// Only the sender or `decides` of `old` supersedes it, once, and not once it is closed
/// (ruling T1).
fn may_supersede(push: &Push, name: &str, old: &str, from: &str) -> Result<(), String> {
    let Some(talk) = held_talk(push, name, old)? else {
        return Err(format!(
            "TALK_REFUSED: `supersedes` names talk {old}, which this host does not hold"
        ));
    };
    let refused = Reason::TalkRefused.code();
    if !talk.superseded_by_allowed(from) {
        return Err(format!(
            "{refused}: talk {old} was sent by {}; only its sender or its `decides` supersedes it",
            talk.from_name()
        ));
    }
    if talk.has("close") {
        return Err(format!(
            "{refused}: talk {old} is closed; a closed talk takes no successor"
        ));
    }
    let mut rivals = talks_where(push.git, &format!("{TALK_PREFIX}{}/", talk.who), |other| {
        other.supersedes() == Some(old) && other.name != name
    })?;
    rivals.extend(
        push.created(name)
            .filter(|other| other.supersedes() == Some(old))
            .cloned(),
    );
    if let Some(rival) = rivals.first() {
        return Err(format!(
            "{refused}: talk {old} is already superseded by {}; a talk takes one successor",
            rival.id
        ));
    }
    Ok(())
}

fn ack(talk: &Talk, actor: &str, entry: &Value) -> Result<(), String> {
    if actor != talk.who {
        return Err(format!(
            "only {}, who owes the reply, acks talk {}; an ack by {actor} is refused",
            talk.who, talk.id
        ));
    }
    if talk.has("ack") {
        return Err(format!(
            "talk {} is already acked; a second ack is the same ack",
            talk.id
        ));
    }
    if entry["ack"] != true {
        return Err("an ack entry carries `ack`: true".to_owned());
    }
    Ok(())
}

/// A backbrief (`DESIGN` § 11c): the outcome `who` thinks was asked, the first step and what is
/// still unclear; an `owes` that is a registry actor; a `follow_up` after `sent`; a status word;
/// a blocked one names its reason code and a new `follow_up`.
fn backbrief(
    push: &Push,
    talk: &Talk,
    actor: &str,
    entry: &Value,
    known: &Known,
) -> Result<(), String> {
    if actor != talk.who {
        return Err(format!("only {} backbriefs talk {}", talk.who, talk.id));
    }
    for field in ["outcome", "first_step"] {
        if entry[field]
            .as_str()
            .is_none_or(|text| text.trim().is_empty())
        {
            return Err(format!("a backbrief names `{field}`"));
        }
    }
    if !entry["unclear"].is_string() {
        return Err("a backbrief names `unclear`, what is still unclear".to_owned());
    }
    if let Some(owes) = entry.get("owes") {
        let name = owes.as_str().unwrap_or_default();
        if !push.registry.enrols(KeyClass::Agent, name) {
            return Err(format!(
                "a backbrief's `owes` {name:?} is not a registry actor"
            ));
        }
    }
    let follow_up = match entry.get("follow_up") {
        None => None,
        Some(value) => {
            let text = value.as_str().unwrap_or_default();
            let at = unix_of(text).ok_or("a backbrief's `follow_up` is a time")?;
            if at <= talk.sent() {
                return Err(format!(
                    "a backbrief's `follow_up` {text} is not after the talk's `sent`"
                ));
            }
            Some(text)
        }
    };
    let status = match entry.get("status") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .filter(|word| STATUS_WORDS.contains(word))
                .ok_or_else(|| {
                    format!(
                        "a backbrief's `status` {value} is not a status word: ready, blocked, validated or done"
                    )
                })?,
        ),
    };
    let reason = entry.get("reason");
    if status != Some("blocked") {
        return match reason {
            Some(_) => Err("only a blocked backbrief names a reason".to_owned()),
            None => Ok(()),
        };
    }
    let current = talk.follow_up_known(push.git, known)?;
    if follow_up.is_none() || follow_up == current.as_deref() {
        return Err(
            "a blocked backbrief states its new `follow_up`: it does not close the talk".to_owned(),
        );
    }
    let code = reason
        .ok_or("a blocked backbrief names its reason code (INTERFACES § 2) in `reason`")?
        .as_str()
        .unwrap_or_default();
    if Reason::from_code(code).is_none() {
        return Err(format!(
            "a blocked backbrief's `reason` {code:?} is not a reason code (INTERFACES § 2)"
        ));
    }
    Ok(())
}

fn answer(git: &Git, talk: &Talk, actor: &str, entry: &Value, known: &Known) -> Result<(), String> {
    if talk.body["kind"] != "discovery" {
        return Err(format!(
            "only a discovery talk takes an answer; {} is not one",
            talk.id
        ));
    }
    if actor != talk.who && talk.body["decides"].as_str() != Some(actor) {
        return Err(format!(
            "only {} or `decides` answers talk {}",
            talk.who, talk.id
        ));
    }
    let value = entry["value"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or("an answer carries its decision, `value`")?;
    match talk.standing_answer_known(git, known)? {
        Some((_, prior)) if prior["value"].as_str() == Some(value) => Err(format!(
            "talk {} already has the answer {value:?}; the same answer is the same answer",
            talk.id
        )),
        Some((commit, prior)) if entry["supersedes"].as_str() != Some(commit.as_str()) => {
            Err(super::send::conflict(&talk.id, &commit, &prior).text())
        }
        None if entry.get("supersedes").is_some() => Err(format!(
            "an answer supersedes the standing answer, and talk {} has none",
            talk.id
        )),
        Some(_) | None => Ok(()),
    }
}

fn close(
    push: &Push,
    talk: &Talk,
    actor: &str,
    entry: &Value,
    known: &Known,
) -> Result<(), String> {
    if actor != talk.who {
        return Err(format!("only {} closes talk {}", talk.who, talk.id));
    }
    if entry["state"] == "answered" {
        return answered(push, talk, entry);
    }
    if entry.get("landing").is_some() || ANSWERS.iter().any(|(field, _)| entry.get(field).is_some())
    {
        return Err("only an answered close names a landing".to_owned());
    }
    if entry["state"] == "expired" {
        return expired(push, talk, entry, known);
    }
    let by = entry["by"].as_str().filter(|by| is_talk_id(by));
    let (Some("superseded"), Some(by)) = (entry["state"].as_str(), by) else {
        return Err(
            "a close entry names `state`: superseded and the talk `by` that superseded it"
                .to_owned(),
        );
    };
    let name = talk_ref(&talk.who, by);
    let Some(tip) = (push.after)(&name)? else {
        return Err(format!(
            "talk {} closes as superseded by {by}, which this host does not hold under {}",
            talk.id, talk.who
        ));
    };
    let successor = Talk::read(push.git, &name, &tip)?;
    if successor.supersedes() != Some(talk.id.as_str()) {
        return Err(format!("talk {by} does not supersede {}", talk.id));
    }
    Ok(())
}

/// G52 (ruling T3): an `expired` close names no successor, and stands only on a talk read after
/// its `expires`: its ack's commit time is later.
fn expired(push: &Push, talk: &Talk, entry: &Value, known: &Known) -> Result<(), String> {
    if entry.get("by").is_some() {
        return Err("an expired close names no talk `by`".to_owned());
    }
    if !talk.expired_known(push.git, known)? {
        return Err(format!(
            "talk {} was not read after its `expires`: an expired close stands only on an ack later than `expires`",
            talk.id
        ));
    }
    Ok(())
}

/// G148: an `answered` close of a delivery talk about change X names one answering change, by
/// its relation, and its `landing`: that change's publication (its landed marker), whose trailer
/// for the relation names X.
fn answered(push: &Push, talk: &Talk, entry: &Value) -> Result<(), String> {
    let Some(change) = talk.about_change() else {
        return Err(format!(
            "talk {} asks about no change, so no landing answers it",
            talk.id
        ));
    };
    let named: Vec<(&str, &str, &str)> = ANSWERS
        .iter()
        .filter_map(|(field, trailer)| entry[field].as_str().map(|by| (*field, *trailer, by)))
        .collect();
    let ([(field, trailer, by)], Some(landing), None) =
        (named.as_slice(), entry["landing"].as_str(), entry.get("by"))
    else {
        return Err(
            "an answered close names one of fixed_by, undone_by or superseded_by, and its `landing`"
                .to_owned(),
        );
    };
    if !is_change_id(by) {
        return Err(format!("`{field}` {by:?} is not a change id"));
    }
    if (push.marker)(&format!("{}{by}", crate::names::LANDED))?.as_deref() != Some(landing) {
        return Err(format!("{landing} is not the landing of change {by}"));
    }
    if !trailer_values(push.git, landing, trailer)?
        .iter()
        .any(|value| value == change)
    {
        return Err(format!(
            "the landing {landing} of {by} does not name change {change} in `{trailer}:`, so it is not `{field}`"
        ));
    }
    Ok(())
}
