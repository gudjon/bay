//! Marker compaction (`DESIGN` § 10a ref-growth budget; ruling R11).
//!
//! The markers kept for the life of the repository (`refs/bay/landed/<id>`,
//! `refs/bay/intent/<hash>/<gen>`, `refs/bay/landed-intent/<hash>/<gen>`) grow by a few refs per
//! change. Past the budget (`[retention] ref_budget`, default 100,000 bay refs on the remote) an
//! executor moves the markers of landed changes whose chain retention already cut into
//! `refs/bay/landed-index/<shard>`: the shard is the first byte of sha256(ref name) in decimal,
//! 0–255; each shard is a chain of executor-signed commits whose flat tree holds one blob per
//! compacted name, at the name with `/` written `%2F`, whose content is the ref's target id. The
//! executor only appends; the host takes a shard by fast-forward only, and refuses a create of a
//! name its shard records (`crate::hook`), so create-only still holds once the ref is gone.
//! Every lookup reads the ref and then its one shard.

use std::collections::{BTreeMap, HashMap};

use sha2::{Digest as _, Sha256};

use crate::change::{ChangeRecord, ChangeState, RECORD_IDENTITY};
use crate::git::Git;
use crate::repo::Repo;
use crate::signing::Signer;

pub const INDEX_PREFIX: &str = "refs/bay/landed-index/";
/// The default ref-growth budget (§ 10a).
pub const DEFAULT_BUDGET: u64 = 100_000;
/// The most names one compaction push moves, so the host's judgement of it stays bounded.
pub const PUSH_MAX: usize = 4096;
/// The namespaces whose markers are compacted.
const NAMESPACES: [&str; 3] = [
    "refs/bay/landed/",
    "refs/bay/intent/",
    "refs/bay/landed-intent/",
];

/// Whether `name` is in a compacted namespace.
#[must_use]
pub fn compactable(name: &str) -> bool {
    NAMESPACES.iter().any(|prefix| name.starts_with(prefix))
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

/// The tree path that records `name`: `/` written `%2F` (no bay ref name holds `%`).
#[must_use]
pub fn path(name: &str) -> String {
    name.replace('/', "%2F")
}

/// The name a tree path records.
#[must_use]
pub fn name_of(path: &str) -> String {
    path.replace("%2F", "/")
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
    let listed = git
        .run(&["ls-tree", index, "--", &path(name)])
        .map_err(|error| error.message())?;
    let Some(line) = listed.lines().next() else {
        return Ok(None);
    };
    let blob = line
        .split_whitespace()
        .nth(2)
        .ok_or_else(|| format!("{index} has a malformed entry for {name}"))?;
    let blob = blob.to_owned();
    let mut read = read_blobs(git, std::slice::from_ref(&blob))?;
    Ok(read.remove(&blob))
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

/// Brings in the remote's shard indexes. A remote that cannot be read is an error.
///
/// # Errors
///
/// The fetch fails.
pub fn fetch(repo: &Repo) -> Result<(), String> {
    let refspec = format!("+{INDEX_PREFIX}*:{INDEX_PREFIX}*");
    repo.git
        .run(&["fetch", "-q", "--no-tags", repo.remote(), &refspec])
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
    repo.git
        .run(&[
            "fetch",
            "-q",
            "--no-tags",
            repo.remote(),
            &format!("+{index}:{index}"),
        ])
        .map_err(|error| error.message())?;
    recorded(&repo.git, name)
}

/// Every name the local shard indexes record under `prefix`, with its target: one `ls-tree` per
/// shard and one batch read of the records.
///
/// # Errors
///
/// A git fault, or a malformed record.
pub fn entries(git: &Git, prefix: &str) -> Result<BTreeMap<String, String>, String> {
    let shards = git
        .run(&["for-each-ref", "--format=%(objectname)", INDEX_PREFIX])
        .map_err(|error| error.message())?;
    let mut wanted: Vec<(String, String)> = Vec::new();
    for index in shards.lines() {
        let listed = git
            .run(&["ls-tree", index])
            .map_err(|error| error.message())?;
        for line in listed.lines() {
            let Some((entry, path)) = line.split_once('\t') else {
                continue;
            };
            let name = name_of(path);
            if let (true, Some(blob)) = (name.starts_with(prefix), entry.split_whitespace().nth(2))
            {
                wanted.push((name, blob.to_owned()));
            }
        }
    }
    let blobs: Vec<String> = wanted.iter().map(|(_, blob)| blob.clone()).collect();
    let contents = read_blobs(git, &blobs)?;
    wanted
        .into_iter()
        .map(|(name, blob)| match contents.get(&blob) {
            Some(target) => Ok((name, target.clone())),
            None => Err(format!("the shard index record of {name} cannot be read")),
        })
        .collect()
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

/// The markers a landed change that retention cut can give up: its landed marker and, for its
/// intent claim, the claim and the landed claim (a claim only together with its landed claim,
/// which names the publication and so the change, once the claim's own commit is gone).
fn markers_of(record: &ChangeRecord, remote: &HashMap<String, String>) -> Vec<(String, String)> {
    if !record.retention_cut || record.state != ChangeState::Landed {
        return Vec::new();
    }
    let mut names = vec![format!("refs/bay/landed/{}", record.id)];
    if let Some(claim) = &record.claim
        && remote.contains_key(&claim.landed_ref())
    {
        names.push(claim.intent_ref());
        names.push(claim.landed_ref());
    }
    names
        .into_iter()
        .filter_map(|name| Some((remote.get(&name)?.clone(), name)))
        .map(|(target, name)| (name, target))
        .collect()
}

/// The remote's bay refs, name to target, in one `ls-remote`.
fn remote_bay_refs(repo: &Repo) -> Result<HashMap<String, String>, String> {
    let listed = repo
        .git
        .run(&["ls-remote", repo.remote(), "refs/bay/*", "refs/notes/bay/*"])
        .map_err(|error| error.message())?;
    Ok(listed
        .lines()
        .filter_map(|line| {
            let (oid, name) = line.split_once('\t')?;
            (name.starts_with("refs/bay/") || name.starts_with("refs/notes/bay/"))
                .then(|| (name.to_owned(), oid.to_owned()))
        })
        .collect())
}

/// One executor pass of compaction: when the remote holds more bay refs than the budget, moves
/// the markers of every landed, cut change in `records` (at most [`PUSH_MAX`]) into their shard
/// indexes, in one atomic push that appends each shard and deletes each moved ref under a lease
/// on its target. Returns the names moved; none when under the budget.
///
/// # Errors
///
/// The remote cannot be read, a git fault, or the push was refused.
pub fn run(
    repo: &Repo,
    signer: Option<&Signer>,
    records: &[ChangeRecord],
) -> Result<Vec<String>, String> {
    let remote = remote_bay_refs(repo)?;
    if (remote.len() as u64) <= repo.config.ref_budget() {
        return Ok(Vec::new());
    }
    let mut moved: Vec<(String, String)> = records
        .iter()
        .flat_map(|record| markers_of(record, &remote))
        .collect();
    moved.truncate(PUSH_MAX);
    // A claim never leaves without its landed claim, which names the change once it is gone.
    if moved.len() == PUSH_MAX
        && moved
            .last()
            .is_some_and(|(name, _)| name.starts_with("refs/bay/intent/"))
    {
        moved.pop();
    }
    if moved.is_empty() {
        return Ok(Vec::new());
    }
    fetch(repo)?;
    let mut shards: BTreeMap<String, Vec<&(String, String)>> = BTreeMap::new();
    for entry in &moved {
        shards.entry(index_ref(&entry.0)).or_default().push(entry);
    }
    let mut leases = Vec::new();
    let mut refspecs = Vec::new();
    let mut appended = Vec::new();
    for (index, entries) in &shards {
        let parent = remote.get(index).cloned();
        let commit = append(&repo.git, signer, index, parent.as_deref(), entries)?;
        leases.push(format!(
            "--force-with-lease={index}:{}",
            parent.as_deref().unwrap_or_default()
        ));
        refspecs.push(format!("{commit}:{index}"));
        appended.push((index.clone(), commit, parent));
    }
    for (name, target) in &moved {
        leases.push(format!("--force-with-lease={name}:{target}"));
        refspecs.push(format!(":{name}"));
    }
    let mut args = vec!["push", "-q", "--atomic"];
    args.extend(leases.iter().map(String::as_str));
    args.push(repo.remote());
    args.extend(refspecs.iter().map(String::as_str));
    repo.git
        .run(&args)
        .map_err(|error| format!("the compaction could not be pushed: {}", error.message()))?;
    let mut lines: Vec<String> = appended
        .iter()
        .map(|(index, commit, _)| format!("update {index} {commit}"))
        .collect();
    lines.extend(moved.iter().map(|(name, _)| format!("delete {name}")));
    let local = lines.join("\n") + "\n";
    let _ = repo
        .git
        .run_with(&["update-ref", "--stdin"], &[], Some(local.as_bytes()));
    Ok(moved.into_iter().map(|(name, _)| name).collect())
}

/// A shard commit on `parent` whose tree is the parent's tree plus `entries`, signed when a
/// signer is given.
fn append(
    git: &Git,
    signer: Option<&Signer>,
    index: &str,
    parent: Option<&str>,
    entries: &[&(String, String)],
) -> Result<String, String> {
    let mut listing: Vec<String> = match parent {
        Some(parent) => git
            .run(&["ls-tree", parent])
            .map_err(|error| error.message())?
            .lines()
            .map(str::to_owned)
            .collect(),
        None => Vec::new(),
    };
    for (name, target) in entries {
        let blob = git
            .run_with(
                &["hash-object", "-w", "--stdin"],
                &[],
                Some(format!("{target}\n").as_bytes()),
            )
            .map_err(|error| error.message())?;
        listing.push(format!("100644 blob {blob}\t{}", path(name)));
    }
    let listing = listing.join("\n") + "\n";
    let tree = git
        .run_with(&["mktree"], &[], Some(listing.as_bytes()))
        .map_err(|error| error.message())?;
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
    use super::{compactable, index_ref, name_of, path, shard};

    #[test]
    fn a_name_has_one_shard_and_one_path() {
        let name = "refs/bay/landed/abcdefgh";
        assert_eq!(
            index_ref(name),
            format!("refs/bay/landed-index/{}", shard(name))
        );
        assert_eq!(path(name), "refs%2Fbay%2Flanded%2Fabcdefgh");
        assert_eq!(name_of(&path(name)), name);
        assert!(compactable(name));
        assert!(compactable("refs/bay/landed-intent/0123/1"));
        assert!(!compactable("refs/bay/changes/abcdefgh"));
        assert!(!compactable("refs/bay/landed-index/7"));
    }
}
