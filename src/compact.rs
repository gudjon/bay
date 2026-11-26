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
//! Every lookup reads the ref and then its one shard. A push moves one shard, at most [`batch`]
//! names of it, on a runner's pass that ran no operation, and live runners take different shards
//! ([`pick`]), so a push's cost on its git client stays bounded and runners do not race it.

use std::collections::{BTreeMap, HashMap, HashSet};

use sha2::{Digest as _, Sha256};

use crate::change::{ChangeRecord, ChangeState, RECORD_IDENTITY};
use crate::git::Git;
use crate::repo::Repo;
use crate::signing::Signer;

pub const INDEX_PREFIX: &str = crate::names::LANDED_INDEX;
/// The default ref-growth budget (§ 10a).
pub const DEFAULT_BUDGET: u64 = 100_000;
/// The most names one compaction push moves, so the host's judgement of it stays bounded.
pub const PUSH_MAX: usize = 4096;
/// The name × remote-ref matches one compaction push may cost its git client (BACKLOG 332: a
/// lowered budget put six runners on one push of 4,096 names against 60,000 remote refs; each git
/// client ran a full core until the push bound stopped it, and no name moved). Git matches each
/// explicit name, and each lease, against every ref the remote lists, about 1 µs a match, so a
/// push of n names under leases costs about 2 µs × n × the remote's refs.
const PUSH_PAIRS: usize = 2_000_000;
/// The fewest names a compaction push moves, however many refs the remote holds.
const PUSH_MIN: usize = 16;
/// G695: how long a count of the remote's bay refs under the budget stands, so a pass lists
/// them at most this often while the remote stays under it.
const COUNTED_EVERY: std::time::Duration = std::time::Duration::from_secs(3600);
/// The namespaces whose markers are compacted.
const NAMESPACES: [&str; 3] = [
    crate::names::LANDED,
    crate::names::INTENT,
    crate::names::LANDED_INTENT,
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

/// The markers of the landed changes in `records` that retention cut and the remote still holds,
/// with their targets: each change's landed marker, its claim and its landed claim. A claim goes
/// only once its landed claim, which names the publication and so the change once the claim's own
/// commit is gone, is on the remote or recorded in its shard, as the host judges it.
fn candidates(
    git: &Git,
    records: &[ChangeRecord],
    remote: &HashMap<String, String>,
) -> Result<Vec<(String, String)>, String> {
    let mut names = Vec::new();
    let mut claims = Vec::new();
    for record in records
        .iter()
        .filter(|record| record.retention_cut && record.state == ChangeState::Landed)
    {
        names.push(format!("{}{}", crate::names::LANDED, record.id));
        if let Some(claim) = &record.claim {
            let landed = claim.landed_ref();
            if remote.contains_key(&landed) {
                names.push(claim.intent_ref());
                names.push(landed);
            } else if remote.contains_key(&claim.intent_ref()) {
                claims.push((claim.intent_ref(), landed));
            }
        }
    }
    let recorded = recorded_names(git, claims.iter().map(|(_, landed)| landed.as_str()))?;
    names.extend(
        claims
            .into_iter()
            .filter(|(_, landed)| recorded.contains(landed))
            .map(|(claim, _)| claim),
    );
    Ok(names
        .into_iter()
        .filter_map(|name| {
            let target = remote.get(&name)?.clone();
            Some((name, target))
        })
        .collect())
}

/// Which of `names` this repository's copy of the shard indexes records: one `ls-tree` per
/// shard that would hold any of them.
fn recorded_names<'a>(
    git: &Git,
    names: impl Iterator<Item = &'a str>,
) -> Result<HashSet<String>, String> {
    let mut wanted: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for name in names {
        wanted.entry(index_ref(name)).or_default().push(path(name));
    }
    let mut found = HashSet::new();
    for (index, paths) in &wanted {
        let Some(tip) = git.ref_target(index).map_err(|error| error.message())? else {
            continue;
        };
        let mut args = vec!["ls-tree", "--name-only", tip.as_str(), "--"];
        args.extend(paths.iter().map(String::as_str));
        let listed = git.run(&args).map_err(|error| error.message())?;
        found.extend(listed.lines().map(name_of));
    }
    Ok(found)
}

/// How many names one compaction push moves against a remote that lists `remote_refs` refs:
/// [`PUSH_PAIRS`] over them, from [`PUSH_MIN`] to [`PUSH_MAX`].
#[must_use]
pub fn batch(remote_refs: usize) -> usize {
    (PUSH_PAIRS / remote_refs.max(1)).clamp(PUSH_MIN, PUSH_MAX)
}

/// The shard a runner moves in a pass, from `shards` (each shard that holds a marker to move,
/// ascending): runner `rank` of `runners` live runners takes the shard at its own place, so
/// runners over the budget at once push different shards; with fewer shards than runners the
/// place wraps, so the last shard is still moved.
#[must_use]
pub fn pick(shards: &[u8], rank: usize, runners: usize) -> Option<u8> {
    if shards.is_empty() {
        return None;
    }
    shards.get(rank % runners.max(1) % shards.len()).copied()
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

/// One executor pass of compaction: when the remote holds more bay refs than the budget, moves
/// markers of the landed, cut changes in `records` into their shard index, one shard a push
/// (BACKLOG 332): the shard [`pick`] gives this runner (`me`, its presence ref), at most
/// [`batch`] of its names, in one atomic push that appends the shard and deletes each moved ref
/// under a lease on its target. Returns the names moved; none when under the budget.
///
/// # Errors
///
/// The remote cannot be read, a git fault, or the push was refused.
pub fn run(
    repo: &Repo,
    (signer, me): (Option<&Signer>, &str),
    records: &[ChangeRecord],
) -> Result<Vec<String>, String> {
    // G695 (a fleet finding, 2026-11-10: every runner pass listed 30,000 refs, 2.3 MB and
    // 1.7 s, to learn the remote was under a budget of 100,000). Under the budget, the count
    // stands for an hour; over it, each pass counts again until the markers are moved.
    let marker = repo.git.common_dir().ok().map(|common| {
        std::path::Path::new(&common)
            .join("bay")
            .join("under-ref-budget")
    });
    let counted = marker
        .as_ref()
        .and_then(|marker| {
            std::fs::metadata(marker)
                .and_then(|meta| meta.modified())
                .ok()
        })
        .and_then(|at| at.elapsed().ok())
        .is_some_and(|age| age < COUNTED_EVERY);
    if counted {
        return Ok(Vec::new());
    }
    let remote = remote_bay_refs(repo)?;
    if (remote.len() as u64) <= repo.config.ref_budget() {
        if let Some(marker) = &marker
            && let Some(dir) = marker.parent()
            && std::fs::create_dir_all(dir).is_ok()
        {
            let _ = std::fs::write(marker, b"");
        }
        return Ok(Vec::new());
    }
    // The landed claims already recorded are read from this repository's copy of the shards.
    fetch(repo)?;
    let mut shards: BTreeMap<u8, Vec<(String, String)>> = BTreeMap::new();
    for (name, target) in candidates(&repo.git, records, &remote)? {
        shards.entry(shard(&name)).or_default().push((name, target));
    }
    let order: Vec<u8> = shards.keys().copied().collect();
    let (rank, runners) = standing(repo, me);
    let Some(chosen) = pick(&order, rank, runners) else {
        return Ok(Vec::new());
    };
    let mut moved = shards.remove(&chosen).unwrap_or_default();
    moved.truncate(batch(remote.len()));
    let index = format!("{INDEX_PREFIX}{chosen}");
    let parent = remote.get(&index).cloned();
    let entries: Vec<&(String, String)> = moved.iter().collect();
    let commit = append(&repo.git, signer, &index, parent.as_deref(), &entries)?;
    let mut args = vec![
        "push".to_owned(),
        "-q".to_owned(),
        "--atomic".to_owned(),
        format!(
            "--force-with-lease={index}:{}",
            parent.as_deref().unwrap_or_default()
        ),
    ];
    args.extend(
        moved
            .iter()
            .map(|(name, target)| format!("--force-with-lease={name}:{target}")),
    );
    args.push(repo.remote().to_owned());
    args.push(format!("{commit}:{index}"));
    args.extend(moved.iter().map(|(name, _)| format!(":{name}")));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    repo.git
        .run(&args)
        .map_err(|error| format!("the compaction could not be pushed: {}", error.message()))?;
    let mut lines = vec![format!("update {index} {commit}")];
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
    use super::{PUSH_MAX, PUSH_MIN, batch, compactable, index_ref, name_of, path, pick, shard};

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

    /// BACKLOG 332: a push's names fall as the remote's refs grow, so its cost on the git client
    /// (about 2 µs a name for each remote ref) stays near four seconds.
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
        assert_eq!(pick(&shards, 0, 2), Some(3));
        assert_eq!(pick(&shards, 1, 2), Some(9));
        assert_eq!(pick(&shards, 0, 1), Some(3));
        assert_eq!(pick(&shards, 4, 6), Some(9));
        assert_eq!(pick(&[7], 1, 2), Some(7));
        assert_eq!(pick(&[], 0, 1), None);
    }
}
