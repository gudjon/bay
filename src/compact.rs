//! Marker and record compaction (`DESIGN` § 10a ref-growth budget; ruling R11).
//!
//! A change leaves refs for the life of the repository: its chain (`refs/bay/changes/<id>`), its
//! latest request (`refs/bay/requests/<id>`) and, once it landed, its marker (`refs/bay/landed/<id>`)
//! and its claims (`refs/bay/intent/<hash>/<gen>`, `refs/bay/landed-intent/<hash>/<gen>`). Once retention
//! cut its chain, an executor moves them into `refs/bay/landed-index/<shard>`: the shard is the first
//! byte of sha256(ref name) in decimal, 0–255, and each shard is a chain of executor-signed commits.
//! G923: a shard's tree has two levels, a name at `<hh>/<name>` ([`path`]), so an append rewrites
//! the top tree and one small tree under it. A marker's entry is a blob holding its target. G921: a
//! chain's or a request's entry is a tree holding its record: `target`, the record file its target
//! holds (`change.json`, `request.json`) and `log`, the time and subject of the step it holds. A shard
//! written before two levels keeps its flat entries, a marker's blob at the top, and both are read.
//! The executor only appends; the host takes a shard by fast-forward only, deletes a ref only with
//! its record in the same push, and refuses a create of a name its shard records (`crate::hook`), so
//! create-only still holds once the ref is gone. Every lookup reads the ref and then its one shard.
//! A push moves one shard, at most [`sized`] names of it, on a runner's pass that ran no operation,
//! and live runners take different shards ([`pick`]), so a push's cost stays near [`TARGET`] and
//! runners do not race it. Under the default budget, none, every cut change moves: the refs the
//! remote holds are those of the retention window, and a store drops its copy of a ref moved
//! elsewhere ([`forget`]).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use sha2::{Digest as _, Sha256};

use crate::change::{Chain, ChangeRecord, ChangeState, HeldChain, RECORD_FILE, RECORD_IDENTITY};
use crate::git::Git;
use crate::names::{CHANGES, INTENT, LANDED, LANDED_INTENT, REQUESTS};
use crate::repo::Repo;
use crate::signing::Signer;

pub const INDEX_PREFIX: &str = crate::names::LANDED_INDEX;
/// The default ref-growth budget (§ 10a): none, so every change retention cut moves into its
/// shard, and the refs the remote holds are those of the retention window.
pub const DEFAULT_BUDGET: u64 = 0;

/// The most names one compaction push moves, so the host's judgement of it stays bounded.
pub const PUSH_MAX: usize = 4096;
/// The name × remote-ref matches one compaction push may cost its git client (BACKLOG 332: a
/// lowered budget put six runners on one push of 4,096 names against 60,000 remote refs; each git
/// client ran a full core until the push bound stopped it, and no name moved). Git matches each
/// explicit name, and each lease, against every ref the remote lists, about 1 µs a match; the push
/// holds one lease, its shard's (BACKLOG 336), so n names cost about 1 µs × n × the remote's refs.
const PUSH_PAIRS: usize = 2_000_000;
/// The fewest names a compaction push moves, however many refs the remote holds.
const PUSH_MIN: usize = 16;
/// BACKLOG 334, 336: the time a compaction push aims at. A host that forwards each push upstream,
/// on a busy box, spent about 9 s a push and 0.5–0.7 s a name at 59,000 refs, far from the bench
/// model behind [`PUSH_PAIRS`]; a constant cannot know the topology or the load, and the times of
/// the last pushes can ([`sized`]).
const TARGET: std::time::Duration = std::time::Duration::from_secs(15);
/// BACKLOG 336: the pushes a store keeps the names and time of, to part a push's fixed cost from
/// its cost a name.
const KEPT: usize = 4;
/// The names a store's first compaction push moves, before it has timed one.
const FIRST: usize = 4;
/// G695: how long a count of the remote's bay refs under the budget stands, so a pass lists
/// them at most this often while the remote stays under it.
const COUNTED_EVERY: std::time::Duration = std::time::Duration::from_secs(3600);
/// The namespaces whose markers are compacted: an entry is the target the marker names.
const MARKERS: [&str; 3] = [LANDED, INTENT, LANDED_INTENT];
/// G921: the namespaces whose refs are compacted with their record, each with the file its
/// target's tree holds the record in.
const RECORDS: [(&str, &str); 2] = [(CHANGES, RECORD_FILE), (REQUESTS, "request.json")];
/// The file of a record entry that holds the target of its ref.
pub const TARGET_FILE: &str = "target";
/// The file of a record entry that holds its step log ([`logs`]).
pub const LOG_FILE: &str = "log";

/// Whether `name` is in a compacted namespace.
#[must_use]
pub fn compactable(name: &str) -> bool {
    MARKERS.iter().any(|prefix| name.starts_with(prefix)) || record_file(name).is_some()
}

/// The record file a compacted chain or request keeps beside its target, as its target's tree
/// holds it; `None` for a marker.
#[must_use]
pub fn record_file(name: &str) -> Option<&'static str> {
    RECORDS
        .iter()
        .find(|(prefix, _)| name.starts_with(prefix))
        .map(|(_, file)| *file)
}

/// The shard of `name`: the first byte of sha256(name).
#[must_use]
pub fn shard(name: &str) -> u8 {
    Sha256::digest(name.as_bytes())[0]
}

/// `refs/bay/landed-index/<shard>` for `name`.
#[must_use]
pub fn index_ref(name: &str) -> String {
    format!("{INDEX_PREFIX}{}", shard(name))
}

/// G923: the tree path that records `name`, `<hh>/<name>`: hh the second byte of sha256(name)
/// in hex, and `/` written `%2F` (no bay ref name holds `%`).
#[must_use]
pub fn path(name: &str) -> String {
    format!("{:02x}/{}", Sha256::digest(name.as_bytes())[1], flat(name))
}

/// The path a shard written before two levels records `name` at.
fn flat(name: &str) -> String {
    name.replace('/', "%2F")
}

/// The name an entry at `path` records, at its [`path`] or flat; `None` for a path of neither.
#[must_use]
pub fn name_of(path: &str) -> Option<String> {
    let name = path.rsplit('/').next()?.replace("%2F", "/");
    (path == flat(&name) || path == self::path(&name)).then_some(name)
}

/// The name whose target the blob at `path` holds: a marker's entry, flat or at its path, or a
/// record's `target` file.
fn target_of(path: &str) -> Option<String> {
    if let Some(entry) = path
        .strip_suffix(TARGET_FILE)
        .and_then(|entry| entry.strip_suffix('/'))
        .filter(|entry| entry.contains('/'))
    {
        return name_of(entry).filter(|name| record_file(name).is_some());
    }
    name_of(path).filter(|name| record_file(name).is_none())
}

/// The type, object and path of each entry of an `ls-tree` listing.
fn listing(listed: &str) -> impl Iterator<Item = (&str, &str, &str)> {
    listed.lines().filter_map(|line| {
        let (entry, path) = line.split_once('\t')?;
        let mut fields = entry.split_whitespace().skip(1);
        Some((fields.next()?, fields.next()?, path))
    })
}

/// Whether `text` is a full object id (40 or 64 lower-case hex).
#[must_use]
pub fn is_object_id(text: &str) -> bool {
    matches!(text.len(), 40 | 64)
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The target the shard commit `index` records for `name`, or `None` when it records none.
///
/// # Errors
///
/// A git fault, or a record that is not an object id: unknown is never "absent".
pub fn recorded_at(git: &Git, index: &str, name: &str) -> Result<Option<String>, String> {
    Ok(recorded_in(git, index, &[name.to_owned()])?.remove(name))
}

/// The target the shard commit `index` records for each of `names`, all of its shard, in either
/// layout: one `ls-tree` and one batch read.
fn recorded_in(
    git: &Git,
    index: &str,
    names: &[String],
) -> Result<HashMap<String, String>, String> {
    let mut args = vec![
        "ls-tree".to_owned(),
        "-r".to_owned(),
        index.to_owned(),
        "--".to_owned(),
    ];
    for name in names {
        args.extend([flat(name), path(name)]);
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let listed = git.run(&args).map_err(|error| error.message())?;
    let wanted: HashSet<&str> = names.iter().map(String::as_str).collect();
    let found: Vec<(String, String)> = listing(&listed)
        .filter(|(kind, _, _)| *kind == "blob")
        .filter_map(|(_, blob, path)| {
            let name = target_of(path)?;
            wanted
                .contains(name.as_str())
                .then(|| (name, blob.to_owned()))
        })
        .collect();
    let blobs: Vec<String> = found.iter().map(|(_, blob)| blob.clone()).collect();
    let targets = read_blobs(git, &blobs)?;
    found
        .into_iter()
        .map(|(name, blob)| match targets.get(&blob) {
            Some(target) => Ok((name, target.clone())),
            None => Err(format!("the shard index record of {name} cannot be read")),
        })
        .collect()
}

/// The target this repository's copy of the shard index records for `name`.
///
/// # Errors
///
/// A git fault, or a malformed record.
pub fn recorded(git: &Git, name: &str) -> Result<Option<String>, String> {
    match git
        .ref_target(&index_ref(name))
        .map_err(|error| error.message())?
    {
        Some(index) => recorded_at(git, &index, name),
        None => Ok(None),
    }
}

/// What this repository's copy of the shard indexes records for each of `names`: one `ls-tree`
/// per shard that would hold any of them.
fn recorded_all<'a>(
    git: &Git,
    names: impl Iterator<Item = &'a str>,
) -> Result<HashMap<String, String>, String> {
    let mut wanted: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for name in names {
        wanted
            .entry(index_ref(name))
            .or_default()
            .push(name.to_owned());
    }
    let mut found = HashMap::new();
    for (index, names) in &wanted {
        let Some(tip) = git.ref_target(index).map_err(|error| error.message())? else {
            continue;
        };
        found.extend(recorded_in(git, &tip, names)?);
    }
    Ok(found)
}

/// G921: the record of change `id` the shard commit `index` keeps, once its chain moved there.
///
/// # Errors
///
/// A git fault, or a record that does not parse.
pub fn kept_at(git: &Git, index: &str, id: &str) -> Result<Option<ChangeRecord>, String> {
    let at = path(&format!("{CHANGES}{id}"));
    let listed = git
        .run(&["ls-tree", index, "--", &at])
        .map_err(|error| error.message())?;
    let Some((_, tree, _)) = listing(&listed).find(|(kind, _, _)| *kind == "tree") else {
        return Ok(None);
    };
    Chain::new(git)
        .record_at(tree)
        .map(Some)
        .map_err(|error| error.message())
}

/// The record of change `id` this repository's copy of its chain's shard keeps ([`kept_at`]).
///
/// # Errors
///
/// A git fault, or a record that does not parse.
pub fn kept(git: &Git, id: &str) -> Result<Option<ChangeRecord>, String> {
    match git
        .ref_target(&index_ref(&format!("{CHANGES}{id}")))
        .map_err(|error| error.message())?
    {
        Some(index) => kept_at(git, &index, id),
        None => Ok(None),
    }
}

/// G921: the record of change `id` the remote's shard keeps, once its chain moved there: one
/// `ls-remote` of the shard and, only when the remote holds it, one fetch of it.
///
/// # Errors
///
/// The remote cannot be read, or the record does not parse.
pub fn remote_kept(repo: &Repo, id: &str) -> Result<Option<ChangeRecord>, String> {
    let index = index_ref(&format!("{CHANGES}{id}"));
    let listed = repo
        .git
        .run(&["ls-remote", repo.remote(), &index])
        .map_err(|error| error.message())?;
    if !listed
        .lines()
        .any(|line| line.split_once('\t').is_some_and(|(_, name)| name == index))
    {
        return Ok(None);
    }
    fetch_indexes(repo, std::slice::from_ref(&index))?;
    kept(&repo.git, id)
}

/// The refspec [`fetch`] fetches: every shard index ref.
#[must_use]
pub fn refspec() -> String {
    format!("+{INDEX_PREFIX}*:{INDEX_PREFIX}*")
}

/// Brings in the remote's shard indexes, unless [`Repo::prefetch`] just did (G733). A remote that
/// cannot be read is an error.
///
/// # Errors
///
/// The fetch fails.
pub fn fetch(repo: &Repo) -> Result<(), String> {
    let refspec = refspec();
    if repo.take_prefetched(&refspec) {
        return Ok(());
    }
    repo.git
        .run(&["fetch", "-q", "--no-tags", repo.remote(), &refspec])
        .map(|_| ())
        .map_err(|error| error.message())
}

/// Brings in the remote's shard indexes `indexes`, in one fetch.
fn fetch_indexes(repo: &Repo, indexes: &[String]) -> Result<(), String> {
    let refspecs: Vec<String> = indexes
        .iter()
        .map(|index| format!("+{index}:{index}"))
        .collect();
    let mut args = vec!["fetch", "-q", "--no-tags", repo.remote()];
    args.extend(refspecs.iter().map(String::as_str));
    repo.git
        .run(&args)
        .map(|_| ())
        .map_err(|error| error.message())
}

/// The remote's marker `name`: the ref's target, or else the target its shard records, read by
/// one `ls-remote` of the ref and its shard and, only when the shard exists, one fetch of it.
///
/// # Errors
///
/// The remote cannot be read, or the shard holds a malformed record.
pub fn remote_marker(repo: &Repo, name: &str) -> Result<Option<String>, String> {
    let index = index_ref(name);
    let mut args = vec!["ls-remote", repo.remote(), name];
    if compactable(name) {
        args.push(&index);
    }
    let listed = repo.git.run(&args).map_err(|error| error.message())?;
    let target = |wanted: &str| {
        listed.lines().find_map(|line| {
            let (oid, listed_name) = line.split_once('\t')?;
            (listed_name == wanted).then(|| oid.to_owned())
        })
    };
    if let Some(oid) = target(name) {
        return Ok(Some(oid));
    }
    if target(&index).is_none() {
        return Ok(None);
    }
    fetch_indexes(repo, std::slice::from_ref(&index))?;
    recorded(&repo.git, name)
}

/// The record each of `blobs` holds, by one `cat-file --batch`: a blob that is not a record
/// (one object id and a newline) is an error, as is one that is missing.
///
/// # Errors
///
/// A git fault, a missing blob, or a blob that is not a record.
pub fn read_blobs(git: &Git, blobs: &[String]) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    if blobs.is_empty() {
        return Ok(out);
    }
    let input = blobs.join("\n") + "\n";
    let read = git
        .run_bytes(&["cat-file", "--batch"], &[], Some(input.as_bytes()))
        .map_err(|error| error.message())?;
    let mut rest = read.as_slice();
    for blob in blobs {
        let malformed = || format!("the shard index blob {blob} is not a record");
        let end = rest
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(malformed)?;
        let header = String::from_utf8_lossy(&rest[..end]).into_owned();
        let fields: Vec<&str> = header.split(' ').collect();
        let [oid, "blob", size] = fields.as_slice() else {
            return Err(malformed());
        };
        let size: usize = size.parse().map_err(|_| malformed())?;
        let body = rest.get(end + 1..end + 1 + size).ok_or_else(malformed)?;
        let text = std::str::from_utf8(body).map_err(|_| malformed())?;
        let target = text.strip_suffix('\n').ok_or_else(malformed)?;
        if *oid != blob.as_str() || !is_object_id(target) {
            return Err(malformed());
        }
        out.insert(blob.clone(), target.to_owned());
        rest = rest.get(end + 2 + size..).unwrap_or_default();
    }
    Ok(out)
}

/// G921: the step log a record of each of `targets` keeps: its commit's committer time and
/// subject, `<unix time> <subject>` and a newline. A cut chain holds one step, its cut, and a request
/// is one. One batch read.
///
/// # Errors
///
/// A git fault, or a target that is not a commit.
pub fn logs(git: &Git, targets: &[String]) -> Result<HashMap<String, String>, String> {
    let read = git.objects(targets).map_err(|error| error.message())?;
    targets
        .iter()
        .zip(read)
        .map(|(target, commit)| {
            let unreadable = || format!("{target} is not a commit");
            let text =
                String::from_utf8(commit.ok_or_else(unreadable)?).map_err(|_| unreadable())?;
            let (header, message) = text.split_once("\n\n").unwrap_or((text.as_str(), ""));
            let time = header
                .lines()
                .find_map(|line| line.strip_prefix("committer "))
                .and_then(|committer| committer.rsplit(' ').nth(1))
                .ok_or_else(unreadable)?;
            let subject = message.lines().next().unwrap_or_default();
            Ok((target.clone(), format!("{time} {subject}\n")))
        })
        .collect()
}

/// Whether `record` ended its change and retention cut its chain, so its refs move (§ 10a).
fn moves(record: &ChangeRecord) -> bool {
    record.retention_cut && matches!(record.state, ChangeState::Landed | ChangeState::Abandoned)
}

/// The refs the change `id` of `record` leaves that move once retention cut it: its chain and its
/// latest request, and a landed change's marker, claim and landed claim.
fn names_of(id: &str, record: &ChangeRecord) -> Vec<String> {
    let mut names = vec![format!("{CHANGES}{id}"), format!("{REQUESTS}{id}")];
    if record.state == ChangeState::Landed {
        names.push(format!("{LANDED}{id}"));
        if let Some(claim) = &record.claim {
            names.extend([claim.intent_ref(), claim.landed_ref()]);
        }
    }
    names
}

/// The refs of the changes in `cut` the remote still holds and may move now, with their targets:
/// a chain or a request only at a target this store holds (`stored`), whose record it writes,
/// and a claim only once its landed claim, which names the publication and so the change once the
/// claim's own commit is gone, is on the remote or recorded in its shard, as the host judges it.
/// The chain moves last, once the remote holds no other ref of its change: a store reads a change
/// from its chain, so a ref left after its chain moved would be left for good.
fn candidates(
    git: &Git,
    cut: &[&HeldChain],
    remote: &HashMap<String, String>,
    stored: &HashSet<String>,
) -> Result<Vec<(String, String)>, String> {
    let mut names = Vec::new();
    let mut claims = Vec::new();
    for held in cut {
        let chain = format!("{CHANGES}{}", held.id);
        // A chain that moved on since its record was read waits for a pass that reads it again.
        if remote.get(&chain).is_some_and(|head| *head != held.head) {
            continue;
        }
        let refs = names_of(&held.id, &held.record);
        let rest = refs
            .iter()
            .any(|name| *name != chain && remote.contains_key(name));
        for name in refs {
            let Some(target) = remote.get(&name) else {
                continue;
            };
            if rest && name == chain {
                continue;
            }
            if record_file(&name).is_some() && !stored.contains(target) {
                continue;
            }
            match name.strip_prefix(INTENT) {
                Some(claim) if !remote.contains_key(&format!("{LANDED_INTENT}{claim}")) => {
                    claims.push((
                        name.clone(),
                        target.clone(),
                        format!("{LANDED_INTENT}{claim}"),
                    ));
                }
                _ => names.push((name, target.clone())),
            }
        }
    }
    let recorded = recorded_all(git, claims.iter().map(|(_, _, landed)| landed.as_str()))?;
    names.extend(
        claims
            .into_iter()
            .filter(|(_, _, landed)| recorded.contains_key(landed))
            .map(|(claim, target, _)| (claim, target)),
    );
    Ok(names)
}

/// G921: deletes from this store each of `held` (a ref and its target here) that this store's copy
/// of its shard records: a ref another store moved, which the remote no longer holds and never
/// takes again. A chain here may be older than the cut its shard keeps, and goes all the same:
/// the record kept there answers for its change. One `ls-tree` per shard and one transaction;
/// returns how many went.
///
/// # Errors
///
/// A git fault reading the shards, or a malformed record.
pub fn forget(git: &Git, held: &[(String, String)]) -> Result<usize, String> {
    let recorded = recorded_all(git, held.iter().map(|(name, _)| name.as_str()))?;
    let mut deletes = String::new();
    let mut gone = 0;
    for (name, target) in held {
        if recorded.contains_key(name) {
            let _ = writeln!(deletes, "delete {name} {target}");
            gone += 1;
        }
    }
    if gone > 0 {
        git.run_with(&["update-ref", "--stdin"], &[], Some(deletes.as_bytes()))
            .map_err(|error| error.message())?;
    }
    Ok(gone)
}

/// G921: [`forget`]s each of `held` (a chain here and its target) the remote no longer holds,
/// after one fetch of the shards that would record them, made only when there are some: a read
/// that lists the store's changes never reads a shard otherwise.
///
/// # Errors
///
/// The shards cannot be fetched, or [`forget`] fails.
pub fn forget_moved(repo: &Repo, held: &[(String, String)]) -> Result<usize, String> {
    if held.is_empty() {
        return Ok(0);
    }
    let indexes: std::collections::BTreeSet<String> =
        held.iter().map(|(name, _)| index_ref(name)).collect();
    fetch_indexes(repo, &indexes.into_iter().collect::<Vec<_>>())?;
    forget(&repo.git, held)
}

/// How many names one compaction push moves against a remote that lists `remote_refs` refs:
/// [`PUSH_PAIRS`] over them, from [`PUSH_MIN`] to [`PUSH_MAX`].
#[must_use]
pub fn batch(remote_refs: usize) -> usize {
    (PUSH_PAIRS / remote_refs.max(1)).clamp(PUSH_MIN, PUSH_MAX)
}

/// The names the next compaction push moves, from `pushes`, the names and the time of this
/// store's last compaction pushes, oldest first (BACKLOG 334, 336): [`FIRST`] before any; half
/// the last after a push over twice [`TARGET`]; else as many as the fitted `cost` says take [`TARGET`].
/// A push over the target never grows the next, and one under it at most doubles it, so one push
/// never runs past twice the target. Always at least one, at most [`batch`] of the remote's refs.
#[must_use]
pub fn sized(pushes: &[(usize, std::time::Duration)], remote_refs: usize) -> usize {
    let next = match pushes.last() {
        None => FIRST,
        Some(&(names, took)) if took > TARGET * 2 => names / 2,
        Some(&(names, took)) => {
            let most = if took > TARGET {
                names
            } else {
                names.saturating_mul(2)
            };
            let fit = if let Some((fixed, each)) = cost(pushes) {
                TARGET.saturating_sub(fixed).as_micros() / each.as_micros().max(1)
            } else {
                // One size only: all of its time is charged a name, which is never too many,
                // and a push under the target tries one more name, so the next fit can part them.
                let rate = u128::try_from(names).unwrap_or(u128::MAX) * TARGET.as_millis()
                    / took.as_millis().max(1);
                if took < TARGET {
                    rate.max(u128::try_from(names).unwrap_or(u128::MAX) + 1)
                } else {
                    rate
                }
            };
            usize::try_from(fit).unwrap_or(usize::MAX).min(most)
        }
    };
    next.clamp(1, batch(remote_refs))
}

/// BACKLOG 336: a push's fixed cost and its cost a name, fitted by least squares to `pushes`
/// (names, time): a host that forwards each push upstream spent about 9 s a push whatever its
/// names, so a rate a name charged that cost to every name and shrank each push to one or two.
/// The fixed cost is at most the quickest push, and a name costs at least 1 µs. None when the
/// pushes all moved the same names, which cannot part the two.
fn cost(
    pushes: &[(usize, std::time::Duration)],
) -> Option<(std::time::Duration, std::time::Duration)> {
    let k = i128::try_from(pushes.len()).ok()?;
    let (mut moved, mut spent, mut squares, mut products) = (0i128, 0i128, 0i128, 0i128);
    for &(names, took) in pushes {
        let names = i128::try_from(names).ok()?;
        let micros = i128::try_from(took.as_micros()).ok()?;
        moved += names;
        spent += micros;
        squares += names * names;
        products += names * micros;
    }
    let spread = k * squares - moved * moved;
    if spread <= 0 {
        return None;
    }
    let each = ((k * products - moved * spent) / spread).max(1);
    let quickest = pushes.iter().map(|&(_, took)| took).min()?;
    let fixed = ((spent - each * moved) / k).clamp(0, i128::try_from(quickest.as_micros()).ok()?);
    Some((
        std::time::Duration::from_micros(u64::try_from(fixed).ok()?),
        std::time::Duration::from_micros(u64::try_from(each).ok()?),
    ))
}

/// The names and the time of each compaction push `record` holds, oldest first, one
/// `<names> <ms>` line each; a line that does not read is passed over.
fn pushes(record: &std::path::Path) -> Vec<(usize, std::time::Duration)> {
    std::fs::read_to_string(record)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (names, millis) = line.trim().split_once(' ')?;
            Some((
                names.parse().ok()?,
                std::time::Duration::from_millis(millis.parse().ok()?),
            ))
        })
        .collect()
}

/// Adds a push of `names` that took `took` to `record`, which keeps the last [`KEPT`].
fn keep(record: &std::path::Path, names: usize, took: std::time::Duration) {
    let mut kept = pushes(record);
    kept.push((names, took));
    let from = kept.len().saturating_sub(KEPT);
    let lines: Vec<String> = kept[from..]
        .iter()
        .map(|(names, took)| format!("{names} {}", took.as_millis()))
        .collect();
    let text = lines.join("\n") + "\n";
    if let Some(dir) = record.parent()
        && std::fs::create_dir_all(dir).is_ok()
    {
        let _ = std::fs::write(record, text);
    }
}

/// One compaction push: the names it moved, into which shard index, the most it could move
/// ([`sized`]) and how long the push took.
#[derive(Debug)]
pub struct Pushed {
    pub names: Vec<String>,
    pub index: String,
    pub batch: usize,
    pub took: std::time::Duration,
}

/// The shard a runner moves in a pass, from `shards` (each shard that holds a marker to move,
/// ascending): a shard is runner `rank`'s of `runners` live runners when shard % runners is
/// its rank, so runners over the budget at once push different shards. Each runner lists its own
/// shards, from the records it holds, so a place in the list sent two runners to one shard when
/// their lists differed (BACKLOG 339: 11 of 145 pushes refused on the shard's lease). A runner with
/// no shard of its own left helps from the end of its list, at its rank counted back, so the last
/// shards are still moved and a helper meets an owner only there.
#[must_use]
pub fn pick(shards: &[u8], rank: usize, runners: usize) -> Option<u8> {
    let runners = runners.max(1);
    let rank = rank % runners;
    shards
        .iter()
        .copied()
        .find(|&shard| usize::from(shard) % runners == rank)
        .or_else(|| shards.iter().rev().nth(rank % shards.len().max(1)).copied())
}

/// This runner's place among the live runners, by presence ref, and how many they are: the
/// presences this store holds that read live now, and `me`.
fn standing(repo: &Repo, me: &str) -> (usize, usize) {
    let _ = crate::presence::fetch(repo);
    let mut names = crate::presence::live_names(&repo.git);
    names.push(me.to_owned());
    names.sort();
    names.dedup();
    let rank = names.iter().position(|name| name == me).unwrap_or(0);
    (rank, names.len())
}

/// The remote's bay refs, name to target, in one `ls-remote`.
fn remote_bay_refs(repo: &Repo) -> Result<HashMap<String, String>, String> {
    let listed = repo
        .git
        .run(&[
            "ls-remote",
            repo.remote(),
            &format!("{}*", crate::names::BAY),
            &format!("{}*", crate::names::NOTES),
        ])
        .map_err(|error| error.message())?;
    Ok(listed
        .lines()
        .filter_map(|line| {
            let (oid, name) = line.split_once('\t')?;
            (name.starts_with(crate::names::BAY) || name.starts_with(crate::names::NOTES))
                .then(|| (name.to_owned(), oid.to_owned()))
        })
        .collect())
}

/// G695, BACKLOG 333: whether the count `marker` records stands: it found the remote's bay refs
/// under `budget`, the budget in force now, less than [`COUNTED_EVERY`] ago. A count under
/// another budget stands for nothing, so a lowered budget is read on the next pass.
fn counted(marker: &std::path::Path, budget: u64) -> bool {
    std::fs::read_to_string(marker).is_ok_and(|text| text.trim() == budget.to_string())
        && std::fs::metadata(marker)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age < COUNTED_EVERY)
}

/// G921: [`forget`]s each of `names` this store holds that `remote` no longer does, and answers
/// the remote's targets of the chains and requests among `names` whose objects this store holds:
/// only those move ([`candidates`]). A request's ref is the remote's alone, but the cut chain
/// keeps its commit as a parent.
fn settle_held(
    git: &Git,
    names: &[String],
    remote: &HashMap<String, String>,
) -> Result<HashSet<String>, String> {
    let gone: Vec<(String, String)> = names
        .iter()
        .cloned()
        .zip(git.object_ids(names).map_err(|error| error.message())?)
        .filter(|(name, _)| !remote.contains_key(name))
        .filter_map(|(name, target)| Some((name, target?)))
        .collect();
    forget(git, &gone)?;
    let records: Vec<String> = names
        .iter()
        .filter(|name| record_file(name).is_some())
        .filter_map(|name| remote.get(name).cloned())
        .collect();
    Ok(records
        .iter()
        .cloned()
        .zip(git.object_ids(&records).map_err(|error| error.message())?)
        .filter_map(|(target, held)| held.map(|_| target))
        .collect())
}

/// One executor pass of compaction: when the remote holds more bay refs than the budget, moves
/// the refs of the changes in `chains` that ended and that retention cut into their shard index,
/// one shard a push (BACKLOG 332): the shard [`pick`] gives this runner (`me`, its presence ref), at
/// most [`sized`] of its names, in one atomic push that appends the shard under a lease on its tip
/// and deletes each moved ref. The push's names and time are kept for the next ones to size
/// themselves by (BACKLOG 334, 336). G921: each of those refs this store holds that the remote no
/// longer does, and that its shard records at the target here, goes from this store first
/// ([`forget`]), so the chains a runner holds stay the live ones and the window's. Returns the
/// push; none when under the budget or nothing moves. A store that holds no cut change lists
/// nothing.
///
/// A delete carries no lease of its own (BACKLOG 336: a lease is one more match against every
/// remote ref, a name): the push sends the target the remote listed as the ref's old value, and
/// the host's hook takes a delete only when the shard this push appends records that same target
/// (`crate::hook`). A marker is create-only there, and a chain or a request moves only from the
/// target this store holds, so the target the shard records is the one the delete removes. On a
/// host without bay's hook a ref moved between the listing and the push is deleted at its new
/// target while the shard records the old one.
///
/// # Errors
///
/// The remote cannot be read, a git fault, or the push was refused.
pub fn run(
    repo: &Repo,
    (signer, me): (Option<&Signer>, &str),
    chains: &[HeldChain],
) -> Result<Option<Pushed>, String> {
    let cut: Vec<&HeldChain> = chains
        .iter()
        .filter(|held| held.record.id == held.id && moves(&held.record))
        .collect();
    if cut.is_empty() {
        return Ok(None);
    }
    // G695 (a fleet finding, 2026-11-10: every runner pass listed 30,000 refs, 2.3 MB and
    // 1.7 s, to learn the remote was under a budget of 100,000). Under the budget, the count
    // stands for an hour; over it, each pass counts again until the refs are moved.
    let state = repo
        .git
        .common_dir()
        .ok()
        .map(|common| std::path::Path::new(&common).join("bay"));
    let marker = state.as_ref().map(|state| state.join("under-ref-budget"));
    let record = state.as_ref().map(|state| state.join("compaction-push"));
    let budget = repo.config.ref_budget();
    if marker
        .as_deref()
        .is_some_and(|marker| counted(marker, budget))
    {
        return Ok(None);
    }
    let remote = remote_bay_refs(repo)?;
    if (remote.len() as u64) <= budget {
        if let Some(marker) = &marker
            && let Some(dir) = marker.parent()
            && std::fs::create_dir_all(dir).is_ok()
        {
            let _ = std::fs::write(marker, budget.to_string());
        }
        return Ok(None);
    }
    // What the shards already record is read from this repository's copy of them.
    fetch(repo)?;
    let names: Vec<String> = cut
        .iter()
        .flat_map(|held| names_of(&held.id, &held.record))
        .collect();
    let stored = settle_held(&repo.git, &names, &remote)?;
    let mut shards: BTreeMap<u8, Vec<(String, String)>> = BTreeMap::new();
    for (name, target) in candidates(&repo.git, &cut, &remote, &stored)? {
        shards.entry(shard(&name)).or_default().push((name, target));
    }
    let order: Vec<u8> = shards.keys().copied().collect();
    let (rank, runners) = standing(repo, me);
    let Some(chosen) = pick(&order, rank, runners) else {
        return Ok(None);
    };
    let mut moved = shards.remove(&chosen).unwrap_or_default();
    let size = sized(
        &record.as_deref().map(pushes).unwrap_or_default(),
        remote.len(),
    );
    moved.truncate(size);
    let index = format!("{INDEX_PREFIX}{chosen}");
    let parent = remote.get(&index).cloned();
    let commit = append(&repo.git, signer, &index, parent.as_deref(), &moved)?;
    let mut args = vec![
        "push".to_owned(),
        "-q".to_owned(),
        "--atomic".to_owned(),
        format!(
            "--force-with-lease={index}:{}",
            parent.as_deref().unwrap_or_default()
        ),
    ];
    args.push(repo.remote().to_owned());
    args.push(format!("{commit}:{index}"));
    args.extend(moved.iter().map(|(name, _)| format!(":{name}")));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let began = std::time::Instant::now();
    let pushed = repo.git.run(&args);
    let took = began.elapsed();
    // A quick refusal (a lease another runner won) says nothing of what a push costs.
    if (pushed.is_ok() || took > TARGET * 2)
        && let Some(record) = &record
    {
        keep(record, moved.len(), took);
    }
    pushed.map_err(|error| {
        format!(
            "the compaction could not be pushed ({} names, {:.1} s): {}",
            moved.len(),
            took.as_secs_f64(),
            error.message()
        )
    })?;
    let mut lines = vec![format!("update {index} {commit}")];
    lines.extend(moved.iter().map(|(name, _)| format!("delete {name}")));
    let local = lines.join("\n") + "\n";
    let _ = repo
        .git
        .run_with(&["update-ref", "--stdin"], &[], Some(local.as_bytes()));
    Ok(Some(Pushed {
        names: moved.into_iter().map(|(name, _)| name).collect(),
        index,
        batch: size,
        took,
    }))
}

/// G923: a directory of this process's own under the store's `bay` state, removed when dropped:
/// a shard append writes its blobs and its index there.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(git: &Git) -> Result<Self, String> {
        let common = git.common_dir().map_err(|error| error.message())?;
        let dir = std::path::Path::new(&common)
            .join("bay")
            .join(format!("compaction-{}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        Ok(Self(dir))
    }

    /// Writes each of `contents` as a blob, by one `hash-object`; their ids, in order.
    fn blobs(&self, git: &Git, contents: &[String]) -> Result<Vec<String>, String> {
        let mut paths = String::new();
        for (at, content) in contents.iter().enumerate() {
            let file = self.0.join(at.to_string());
            std::fs::write(&file, content)
                .map_err(|error| format!("{}: {error}", file.display()))?;
            let _ = writeln!(paths, "{}", file.display());
        }
        let ids = git
            .run_with(
                &["hash-object", "-w", "--no-filters", "--stdin-paths"],
                &[],
                Some(paths.as_bytes()),
            )
            .map_err(|error| error.message())?;
        let ids: Vec<String> = ids.lines().map(str::to_owned).collect();
        if ids.len() != contents.len() {
            return Err(format!(
                "git wrote {} of {} shard index blobs",
                ids.len(),
                contents.len()
            ));
        }
        Ok(ids)
    }

    /// The tree of `parent` (none: the empty tree) with the entries `info` adds, as
    /// `update-index --index-info` reads them, by an index of its own, so only the trees on their
    /// paths are written.
    fn tree(&self, git: &Git, parent: Option<&str>, info: &str) -> Result<String, String> {
        let index = self.0.join("index");
        let index = index.display().to_string();
        let envs = [("GIT_INDEX_FILE", index.as_str())];
        let base = parent.map_or(["read-tree", "--empty"], |parent| ["read-tree", parent]);
        git.run_with(&base, &envs, None)
            .map_err(|error| error.message())?;
        git.run_with(
            &["update-index", "--index-info"],
            &envs,
            Some(info.as_bytes()),
        )
        .map_err(|error| error.message())?;
        git.run_with(&["write-tree"], &envs, None)
            .map_err(|error| error.message())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A shard commit on `parent` whose tree is the parent's plus `entries` at their [`path`]s: a
/// marker's target as a blob, a chain's or a request's record as a tree of its target, its record
/// file and its step log (G921). Written through an index of its own (G923); signed when a signer
/// is given.
fn append(
    git: &Git,
    signer: Option<&Signer>,
    index: &str,
    parent: Option<&str>,
    entries: &[(String, String)],
) -> Result<String, String> {
    let records: Vec<String> = entries
        .iter()
        .filter(|(name, _)| record_file(name).is_some())
        .map(|(_, target)| target.clone())
        .collect();
    let steps = logs(git, &records)?;
    let files: Vec<String> = entries
        .iter()
        .filter_map(|(name, target)| Some(format!("{target}:{}", record_file(name)?)))
        .collect();
    let mut files = git
        .object_ids(&files)
        .map_err(|error| error.message())?
        .into_iter();
    let mut contents = Vec::new();
    for (name, target) in entries {
        contents.push(format!("{target}\n"));
        if record_file(name).is_some() {
            contents.push(
                steps
                    .get(target)
                    .cloned()
                    .ok_or_else(|| format!("{target} of {name} has no step log"))?,
            );
        }
    }
    let scratch = Scratch::new(git)?;
    let mut blobs = scratch.blobs(git, &contents)?.into_iter();
    let mut info = String::new();
    for (name, target) in entries {
        let at = path(name);
        let blob = blobs.next().unwrap_or_default();
        let Some(file) = record_file(name) else {
            let _ = writeln!(info, "100644 blob {blob}\t{at}");
            continue;
        };
        let step = blobs.next().unwrap_or_default();
        let held = files
            .next()
            .flatten()
            .ok_or_else(|| format!("{target} of {name} holds no {file}"))?;
        let _ = writeln!(info, "100644 blob {blob}\t{at}/{TARGET_FILE}");
        let _ = writeln!(info, "100644 blob {held}\t{at}/{file}");
        let _ = writeln!(info, "100644 blob {step}\t{at}/{LOG_FILE}");
    }
    let tree = scratch.tree(git, parent, &info)?;
    let mut args: Vec<String> = signer.map(Signer::config_args).unwrap_or_default();
    args.extend(["commit-tree".to_owned(), tree]);
    if let Some(parent) = parent {
        args.extend(["-p".to_owned(), parent.to_owned()]);
    }
    args.push(
        if signer.is_some() {
            "-S"
        } else {
            "--no-gpg-sign"
        }
        .to_owned(),
    );
    args.extend([
        "-m".to_owned(),
        format!("compact {} names into {index}", entries.len()),
    ]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let identity: Vec<(String, String)> = signer.map_or_else(
        || {
            RECORD_IDENTITY
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect()
        },
        Signer::identity,
    );
    let envs: Vec<(&str, &str)> = identity
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    git.run_with(&args, &envs, None)
        .map_err(|error| error.message())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        FIRST, KEPT, PUSH_MAX, PUSH_MIN, TARGET, batch, compactable, cost, counted, index_ref,
        keep, name_of, path, pick, pushes, record_file, shard, sized, target_of,
    };

    /// G923: a name sits at `<hh>/<name>` in its shard, and a shard written flat still reads.
    #[test]
    fn a_name_has_one_shard_and_one_path() {
        let name = "refs/bay/landed/abcdefgh";
        assert_eq!(
            index_ref(name),
            format!("refs/bay/landed-index/{}", shard(name))
        );
        let flat = "refs%2Fbay%2Flanded%2Fabcdefgh";
        let at = path(name);
        let (hh, rest) = at.split_once('/').expect("two levels");
        assert_eq!((hh.len(), rest), (2, flat));
        assert_eq!(name_of(&at).as_deref(), Some(name));
        assert_eq!(name_of(flat).as_deref(), Some(name));
        let other = if hh == "00" { "01" } else { "00" };
        assert_eq!(name_of(&format!("{other}/{flat}")), None);
        assert_eq!(target_of(&at).as_deref(), Some(name));
        assert_eq!(target_of(flat).as_deref(), Some(name));
        assert!(compactable(name));
        assert!(compactable("refs/bay/landed-intent/0123/1"));
        assert!(!compactable("refs/bay/landed-index/7"));
    }

    /// G921: a chain and a request move with their record, a tree whose `target` names the ref's.
    #[test]
    fn a_chain_and_a_request_move_with_their_record() {
        let chain = "refs/bay/changes/abcdefgh";
        assert!(compactable(chain));
        assert_eq!(record_file(chain), Some("change.json"));
        assert_eq!(
            record_file("refs/bay/requests/abcdefgh"),
            Some("request.json")
        );
        assert_eq!(record_file("refs/bay/landed/abcdefgh"), None);
        assert_eq!(
            target_of(&format!("{}/target", path(chain))).as_deref(),
            Some(chain)
        );
        assert_eq!(target_of(&path(chain)), None);
        assert_eq!(target_of(&format!("{}/log", path(chain))), None);
        assert_eq!(target_of("refs%2Fbay%2Fchanges%2Fabcdefgh"), None);
    }

    /// BACKLOG 332: a push's names fall as the remote's refs grow, so its cost on the git client
    /// (about 1 µs a name for each remote ref) stays near two seconds.
    #[test]
    fn a_push_costs_at_most_its_pairs() {
        assert_eq!(batch(60_000), 33);
        assert_eq!(batch(1_000), 2_000);
        assert_eq!(batch(0), PUSH_MAX);
        assert_eq!(batch(10), PUSH_MAX);
        assert_eq!(batch(10_000_000), PUSH_MIN);
    }

    /// BACKLOG 332: live runners over the budget at once take different shards, and the last
    /// shard is still taken when the runners outnumber the shards.
    #[test]
    fn live_runners_take_different_shards() {
        let shards = [3, 9, 200];
        assert_eq!(pick(&shards, 0, 2), Some(200));
        assert_eq!(pick(&shards, 1, 2), Some(3));
        assert_eq!(pick(&shards, 0, 1), Some(3));
        assert_eq!(pick(&shards, 4, 6), Some(9));
        assert_eq!(pick(&[7], 1, 2), Some(7));
        assert_eq!(pick(&[7], 0, 2), Some(7));
        assert_eq!(pick(&[], 0, 1), None);
    }

    /// BACKLOG 339: two runners list their shards from the records each holds, so their lists
    /// differ (run 5: one walked 1, 2, 3, ... and the other 0, 3, 5, 9, ...); while each has a
    /// shard of its own they never take the same one, whatever the other's list holds.
    #[test]
    fn runners_whose_lists_differ_take_different_shards() {
        let lists: [&[u8]; 5] = [
            &[1, 2, 3, 4],
            &[0, 3, 5, 9],
            &[3, 5, 7, 8],
            &[2, 3],
            &[3, 4, 9, 200],
        ];
        for first in lists {
            for second in lists {
                for (a, b) in [(0, 1), (1, 0)] {
                    let (mine, theirs) = (pick(first, a, 2), pick(second, b, 2));
                    let own =
                        |list: &[u8], rank: usize| list.iter().any(|&s| usize::from(s) % 2 == rank);
                    if own(first, a) && own(second, b) {
                        assert_ne!(mine, theirs, "{first:?} rank {a} and {second:?} rank {b}");
                    }
                }
            }
        }
    }

    /// BACKLOG 334: a push of one size is sized by its time: the first moves FIRST names; a push
    /// under the target grows the next towards it, at most twice, and by one at the least; a push
    /// over the target shrinks the next, and one over twice the target halves it; never none,
    /// never past the bound of the remote's refs.
    #[test]
    fn a_push_is_sized_by_the_last_push_time() {
        let secs = Duration::from_secs;
        assert_eq!(sized(&[], 60_000), FIRST);
        assert_eq!(sized(&[(4, secs(5))], 60_000), 8, "twice, never more");
        assert_eq!(sized(&[(4, TARGET / 2 * 3)], 60_000), 2);
        assert_eq!(sized(&[(10, TARGET)], 60_000), 10, "on target: the same");
        assert_eq!(sized(&[(33, secs(35))], 60_000), 16, "over twice: half");
        assert_eq!(sized(&[(1, secs(60))], 60_000), 1, "never none");
        assert_eq!(
            sized(&[(30, secs(1))], 60_000),
            batch(60_000),
            "the bound holds"
        );
        assert_eq!(sized(&[(0, Duration::ZERO)], 1_000), 1);
        assert_eq!(
            sized(&[(1, Duration::from_millis(9600))], 60_000),
            2,
            "under the target, one more name, so the next push can part the fixed cost"
        );
    }

    /// BACKLOG 336 (about 9 s a push and 0.6 s a name into a host that forwards each push upstream
    /// shrank each push to one to four names): a push's fixed cost is not charged a name. The fit aims
    /// the next push at the target; a push over the target never grows the next.
    #[test]
    fn a_fixed_cost_is_not_charged_a_name() {
        let ms = Duration::from_millis;
        assert_eq!(
            cost(&[(4, ms(10_400)), (8, ms(12_800))]),
            Some((ms(8_000), ms(600)))
        );
        assert_eq!(cost(&[(4, ms(9_400)), (4, ms(9_800))]), None, "one size");
        assert_eq!(
            sized(&[(4, ms(9_400)), (8, ms(9_800))], 60_000),
            16,
            "9 s fixed and 0.1 s a name: twice the last"
        );
        assert_eq!(
            sized(&[(4, ms(10_400)), (8, ms(12_800))], 60_000),
            11,
            "8 s fixed and 0.6 s a name: as many as take the target"
        );
        assert_eq!(
            sized(&[(4, ms(10_400)), (8, ms(16_000))], 60_000),
            7,
            "over the target: fewer than the last"
        );
        let (fixed, each) = cost(&[(8, ms(9_000)), (4, ms(12_000))]).unwrap();
        assert_eq!(
            each,
            Duration::from_micros(1),
            "more names, less time: a name costs 1 µs"
        );
        assert_eq!(
            fixed,
            ms(9_000),
            "the fixed cost is at most the quickest push"
        );
    }

    #[test]
    fn a_push_record_reads_back_and_keeps_the_last() {
        let dir = std::env::temp_dir().join(format!("bay-push-record-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let record = dir.join("compaction-push");
        assert!(pushes(&record).is_empty(), "no push yet");
        std::fs::write(&record, "12 8500\n").unwrap();
        assert_eq!(pushes(&record), [(12, Duration::from_millis(8500))]);
        std::fs::write(&record, "12").unwrap();
        assert!(pushes(&record).is_empty(), "a record without its time");
        for names in 1..=KEPT + 2 {
            keep(&record, names, Duration::from_millis(1000));
        }
        let kept: Vec<usize> = pushes(&record).iter().map(|&(names, _)| names).collect();
        assert_eq!(
            kept,
            (3..=KEPT + 2).collect::<Vec<_>>(),
            "the last KEPT, oldest first"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_count_stands_only_under_the_budget_it_was_made_against() {
        let dir = std::env::temp_dir().join(format!("bay-counted-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("under-ref-budget");
        assert!(!counted(&marker, 100_000), "no count yet");
        std::fs::write(&marker, "").unwrap();
        assert!(!counted(&marker, 100_000), "a count that names no budget");
        std::fs::write(&marker, "100000").unwrap();
        assert!(counted(&marker, 100_000));
        assert!(!counted(&marker, 56_000), "the budget was lowered since");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
