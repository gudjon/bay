//! `bay status --heads`: a census of the remote's branches, and `--prune` of the landed ones
//! (G269).
//!
//! A repository many agents share gathers branches on its remote: one per agent, task or
//! experiment, most of them merged long ago. The census lists every `refs/heads/*` on the
//! configured remote with its tip, the committer time of its tip, and one class:
//!
//! - `trunk`: the trunk;
//! - `target`: the ref of a `[[target]]`;
//! - `lane`: a ref a `[[lane]]` pattern matches;
//! - `landed`: everything it holds is on the trunk, with the proof: `ancestor` (its tip is an
//!   ancestor of the trunk's), `patch` (`git cherry` finds every commit of it patch-equivalent
//!   to one on the trunk), or `change` (each commit that is not carries one `Change-Id` whose
//!   change has landed: the remote's landed marker names a commit the trunk reaches);
//! - `stale`: it has commits on no trunk by any proof above, and its tip is older than the
//!   stale age (`--stale-days`, default 30);
//! - `unique`: it has commits on no trunk by any proof above.
//!
//! bay's own drafts live under `refs/bay/changes/`, never under `refs/heads/`, so no branch is a
//! live change's draft and the census has no class for one.
//!
//! The read costs one `ls-remote`, one fetch of the tips it listed (exactly those objects, into
//! `refs/bay/census/heads/*`, removed when the census ends), two `for-each-ref`s, one `git
//! cherry` per branch the trunk does not contain, one `git log` of their unmatched commits and,
//! only when one of those carries a `Change-Id`, one read of the landed markers.
//!
//! `--prune` deletes the `landed` branches proven by `ancestor` or `patch`, and nothing else:
//! never the trunk, a target, a lane ref, a stale or a unique branch, and never one landed by
//! `change` alone (a branch can carry a landed change's `Change-Id` and still hold edits that
//! never landed). For each branch it deletes, in this order:
//!
//! 1. it archives the tip on the remote, so the remote's gc keeps its objects: the local ref
//!    `refs/bay/archive/heads/<name>` is set to the tip and pushed under an expect-absent lease
//!    (`--force-with-lease=<archive>:`). An archive ref that already holds another tip, here or
//!    on the remote, gives the name a `-<short sha>` suffix; one that holds this tip is reused.
//!    An archive push that fails leaves the branch alone;
//! 2. it appends the restore line to `<common dir>/bay/pruned-heads.log`:
//!    `git push <remote> <archive>:refs/heads/<name>  # <sha>` (the archive ref is in this clone,
//!    and on the remote for any other clone to fetch); a line it cannot write leaves the branch
//!    alone;
//! 3. it deletes the branch under a lease of the tip the census judged
//!    (`--force-with-lease=refs/heads/<name>:<sha>`), so a branch someone moved since is
//!    refused by the remote, not deleted;
//! 4. when the config at the trunk's tip sets `[heads] on_prune` (N-19), it runs that prune
//!    adapter once for the branch it deleted: `<on_prune> pruned <name> <sha> <archive ref>`,
//!    its program materialized from the trunk's tip the way the hold adapter's is
//!    (`crate::hold::run_from_trunk`), never taken from this machine's work tree, within
//!    `[heads] timeout_s` (30 s unless set). The adapter is where a team reaches outside the
//!    repository, to close the branch's open pull request on its host for example; bay itself
//!    never does. Its outcome is recorded on the branch's `pruned` row as `on_prune: {ok, why}`
//!    (`why`: the exit status and the first line of its stderr, or why it did not run). A
//!    failing adapter never undoes the delete and never stops the loop: the next branch is
//!    judged and pruned as if it had passed.
//!
//! Every branch not deleted is reported with its reason. A bay host accepts an archive ref as a
//! creation only (`crate::hook`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::git::Git;
use crate::names::{ARCHIVE_HEADS, CENSUS};
use crate::repo::Repo;
use crate::result::BayResult;

/// The stale age when `--stale-days` names none.
pub const DEFAULT_STALE_DAYS: u64 = 30;

/// The file under `<common dir>/bay/` that holds one restore line per pruned branch.
pub const LOG_FILE: &str = "pruned-heads.log";

/// How long the prune adapter may take when `[heads] timeout_s` is not set (N-19).
pub const DEFAULT_ON_PRUNE_TIMEOUT_S: u64 = 30;

/// The first argument the prune adapter is run with (N-19).
pub const PRUNED: &str = "pruned";

/// What `bay status --heads` was asked.
pub struct Request {
    /// Delete the landed branches.
    pub prune: bool,
    /// The stale age in days; [`DEFAULT_STALE_DAYS`] when `None`.
    pub stale_days: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Trunk,
    Target,
    Lane,
    Landed,
    Stale,
    Unique,
}

impl Class {
    const ALL: [Self; 6] = [
        Self::Trunk,
        Self::Target,
        Self::Lane,
        Self::Landed,
        Self::Stale,
        Self::Unique,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Trunk => "trunk",
            Self::Target => "target",
            Self::Lane => "lane",
            Self::Landed => "landed",
            Self::Stale => "stale",
            Self::Unique => "unique",
        }
    }

    /// Why a branch of this class is never pruned; `None` for `landed`.
    fn kept(self) -> Option<&'static str> {
        match self {
            Self::Trunk => Some("the trunk is never pruned"),
            Self::Target => Some("a [[target]] ref is never pruned"),
            Self::Lane => Some("a [[lane]] ref is never pruned"),
            Self::Stale => Some("it holds commits the trunk does not; stale, but never pruned"),
            Self::Unique => Some("it holds commits the trunk does not"),
            Self::Landed => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Proof {
    Ancestor,
    Patch,
    Change,
}

impl Proof {
    fn name(self) -> &'static str {
        match self {
            Self::Ancestor => "ancestor",
            Self::Patch => "patch",
            Self::Change => "change",
        }
    }
}

/// One branch on the remote, as the census judged it.
struct Head {
    name: String,
    sha: String,
    class: Class,
    proof: Option<Proof>,
    /// The committer time of its tip.
    at: Option<u64>,
    /// The commits no proof puts on the trunk, for `stale` and `unique`.
    unique_commits: Option<usize>,
    /// Why a proof could not be read: the branch is then not `landed`.
    unread: Option<String>,
}

impl Head {
    fn to_json(&self) -> Value {
        let mut row = json!({
            "name": self.name,
            "sha": self.sha,
            "class": self.class.name(),
            "newest_commit_at": self.at.map(crate::clock::rfc3339_of),
            "proof": self.proof.map(Proof::name),
        });
        if let Some(count) = self.unique_commits {
            row["unique_commits"] = json!(count);
        }
        if let Some(why) = &self.unread {
            row["unread"] = json!(why);
        }
        row
    }
}

#[must_use]
pub fn heads(dir: &Path, request: &Request) -> BayResult {
    run(dir, request).unwrap_or_else(BayResult::failed)
}

/// Removes the census refs when the census ends, whichever way it ends.
struct CensusRefs<'a>(&'a Git);

impl Drop for CensusRefs<'_> {
    fn drop(&mut self) {
        let _ = clear(self.0);
    }
}

/// Deletes every ref under [`CENSUS`].
fn clear(git: &Git) -> Result<(), String> {
    let listed = git
        .run(&["for-each-ref", "--format=%(refname)", CENSUS])
        .map_err(|error| error.message())?;
    if listed.is_empty() {
        return Ok(());
    }
    let input = listed.lines().fold(String::new(), |mut out, name| {
        let _ = writeln!(out, "delete {name}");
        out
    });
    git.run_with(&["update-ref", "--stdin"], &[], Some(input.as_bytes()))
        .map(|_| ())
        .map_err(|error| error.message())
}

fn census_ref(name: &str) -> String {
    format!("{CENSUS}heads/{name}")
}

fn run(dir: &Path, request: &Request) -> Result<BayResult, String> {
    // The config at the trunk the remote holds now: a target or lane added since this clone's
    // last fetch is never judged as a plain branch.
    let repo = Repo::open_fetched(dir).map_err(|error| error.message())?;
    let git = &repo.git;
    let remote = repo.remote().to_owned();
    let trunk = repo.trunk().to_owned();
    let stale_days = request.stale_days.unwrap_or(DEFAULT_STALE_DAYS);

    let remote_heads = list(git, &remote)?;
    let Some(trunk_tip) = remote_heads.get(&trunk).cloned() else {
        return Err(format!(
            "the remote {remote} has no branch {trunk}, the trunk"
        ));
    };

    // One fetch of exactly the tips listed, so every judgement and every lease is of the tip
    // the listing named.
    clear(git)?;
    let _census = CensusRefs(git);
    let refspecs = remote_heads
        .iter()
        .fold(String::new(), |mut out, (name, sha)| {
            let _ = writeln!(out, "+{sha}:{}", census_ref(name));
            out
        });
    git.run_with(
        &["fetch", "-q", "--no-tags", "--stdin", &remote],
        &[],
        Some(refspecs.as_bytes()),
    )
    .map_err(|error| {
        format!(
            "the branches of {remote} cannot be fetched: {}",
            error.message()
        )
    })?;

    let (mut rows, unmatched) = classify(&repo, &remote_heads, &trunk_tip)?;
    prove_by_change(&repo, &trunk_tip, &mut rows, &unmatched);
    let stale_before = crate::live::unix_now().saturating_sub(stale_days.saturating_mul(86_400));
    for row in &mut rows {
        if row.class == Class::Unique && row.at.is_some_and(|at| at < stale_before) {
            row.class = Class::Stale;
        }
    }

    let mut counts = serde_json::Map::new();
    for class in Class::ALL {
        let count = rows.iter().filter(|row| row.class == class).count();
        counts.insert(class.name().to_owned(), json!(count));
    }
    let mut detail = json!({
        "remote": &remote,
        "trunk": &trunk,
        "trunk_tip": &trunk_tip,
        "stale_days": stale_days,
        "heads": rows.iter().map(Head::to_json).collect::<Vec<_>>(),
        "counts": counts,
        "fetched": true,
    });
    if request.prune {
        let log = repo.common_dir.join("bay").join(LOG_FILE);
        let (pruned, skipped) = prune(&repo, &remote, &trunk_tip, &rows, &log);
        detail["pruned"] = json!(pruned);
        detail["skipped"] = json!(skipped);
        detail["log"] = json!(log.display().to_string());
        detail["note"] = json!(
            "each pruned branch's restore line is in the log; remote-tracking refs stay until git fetch --prune"
        );
    } else {
        detail["note"] = json!(
            "read only: nothing deleted; bay status --heads --prune deletes the landed branches"
        );
    }
    Ok(BayResult::ok(detail))
}

/// The remote's branches, by name (`refs/heads/` taken off), with their tips: one `ls-remote`.
fn list(git: &Git, remote: &str) -> Result<BTreeMap<String, String>, String> {
    let listed = git
        .run(&["ls-remote", remote, "refs/heads/*"])
        .map_err(|error| format!("the remote {remote} cannot be listed: {}", error.message()))?;
    Ok(listed
        .lines()
        .filter_map(|line| {
            let (oid, name) = line.split_once('\t')?;
            Some((
                name.strip_prefix("refs/heads/")?.to_owned(),
                oid.trim().to_owned(),
            ))
        })
        .collect())
}

/// For each branch no proof by ancestry or patch settles: its index among the rows and its
/// commits no trunk commit matches by patch.
type Unmatched = Vec<(usize, Vec<String>)>;

/// Each branch judged by name, by ancestry and by patch, in name order, and the branches left
/// for [`prove_by_change`].
fn classify(
    repo: &Repo,
    remote_heads: &BTreeMap<String, String>,
    trunk_tip: &str,
) -> Result<(Vec<Head>, Unmatched), String> {
    let git = &repo.git;
    let config = &repo.config;
    let trunk = repo.trunk();
    let census_prefix = census_ref("");
    let times: HashMap<String, u64> = git
        .run(&[
            "for-each-ref",
            "--format=%(committerdate:unix) %(refname)",
            &census_prefix,
        ])
        .map_err(|error| error.message())?
        .lines()
        .filter_map(|line| {
            let (at, name) = line.split_once(' ')?;
            Some((
                name.strip_prefix(&census_prefix)?.to_owned(),
                at.parse().ok()?,
            ))
        })
        .collect();
    let merged = format!("--merged={trunk_tip}");
    let ancestors: BTreeSet<String> = git
        .run(&[
            "for-each-ref",
            &merged,
            "--format=%(refname)",
            &census_prefix,
        ])
        .map_err(|error| error.message())?
        .lines()
        .filter_map(|name| name.strip_prefix(&census_prefix).map(str::to_owned))
        .collect();

    let mut rows: Vec<Head> = Vec::new();
    let mut unmatched: Unmatched = Vec::new();
    for (name, sha) in remote_heads {
        let full = format!("refs/heads/{name}");
        let mut row = Head {
            name: name.clone(),
            sha: sha.clone(),
            class: Class::Unique,
            proof: None,
            at: times.get(name).copied(),
            unique_commits: None,
            unread: None,
        };
        if name == trunk {
            row.class = Class::Trunk;
        } else if config
            .targets
            .iter()
            .any(|target| target.reference() == full)
        {
            row.class = Class::Target;
        } else if config.lanes.iter().any(|lane| {
            lane.refs
                .iter()
                .any(|pattern| crate::lane::matches(pattern, &full))
        }) {
            row.class = Class::Lane;
        } else if ancestors.contains(name) {
            row.class = Class::Landed;
            row.proof = Some(Proof::Ancestor);
        } else {
            match git.run(&["cherry", trunk_tip, &census_ref(name)]) {
                Ok(text) => {
                    let plus: Vec<String> = text
                        .lines()
                        .filter_map(|line| line.strip_prefix("+ "))
                        .map(|oid| oid.trim().to_owned())
                        .collect();
                    if plus.is_empty() {
                        row.class = Class::Landed;
                        row.proof = Some(Proof::Patch);
                    } else {
                        unmatched.push((rows.len(), plus));
                    }
                }
                Err(error) => row.unread = Some(error.message()),
            }
        }
        rows.push(row);
    }
    Ok((rows, unmatched))
}

/// Judges each branch in `unmatched` by `Change-Id`: `landed` (proof `change`) when each of its
/// unmatched commits carries one `Change-Id` whose landing the trunk reaches (the remote's
/// marker names a commit that is an ancestor of `trunk_tip`); otherwise it keeps its class and
/// counts the commits no proof puts on the trunk. One `git log` of the unmatched commits; the
/// markers are read only when one of them names a change. A read that fails proves nothing.
fn prove_by_change(repo: &Repo, trunk_tip: &str, rows: &mut [Head], unmatched: &Unmatched) {
    let git = &repo.git;
    let commits: BTreeSet<&str> = unmatched
        .iter()
        .flat_map(|(_, plus)| plus.iter().map(String::as_str))
        .collect();
    let mut change_of: HashMap<String, String> = HashMap::new();
    let mut unread: Option<String> = None;
    if !commits.is_empty() {
        let input = commits.iter().fold(String::new(), |mut out, oid| {
            let _ = writeln!(out, "{oid}");
            out
        });
        match git.run_with(
            &[
                "log",
                "--no-walk=unsorted",
                "--stdin",
                "--format=%H%x1f%(trailers:key=Change-Id,valueonly,separator=%x2c)",
            ],
            &[],
            Some(input.as_bytes()),
        ) {
            Ok(text) => {
                for line in text.lines() {
                    let Some((oid, change)) = line.split_once('\x1f') else {
                        continue;
                    };
                    let change = change.trim();
                    if !change.is_empty() && !change.contains(',') {
                        change_of.insert(oid.trim().to_owned(), change.to_owned());
                    }
                }
            }
            Err(error) => unread = Some(error.message()),
        }
    }
    let mut landed: BTreeSet<String> = BTreeSet::new();
    if !change_of.is_empty() {
        match crate::publish::landed_markers(repo) {
            Ok(markers) => {
                let named: BTreeSet<&String> = change_of.values().collect();
                for change in named {
                    let Some(marker) = markers.get(change) else {
                        continue;
                    };
                    let reached = git.is_ancestor(marker, trunk_tip).unwrap_or(false);
                    if reached {
                        landed.insert(change.clone());
                    }
                }
            }
            Err(error) => unread = Some(error),
        }
    }
    for (index, plus) in unmatched {
        let unproven = plus
            .iter()
            .filter(|oid| {
                !change_of
                    .get(oid.as_str())
                    .is_some_and(|change| landed.contains(change))
            })
            .count();
        let row = &mut rows[*index];
        if unproven == 0 {
            row.class = Class::Landed;
            row.proof = Some(Proof::Change);
        } else {
            row.unique_commits = Some(unproven);
            row.unread.clone_from(&unread);
        }
    }
}

/// Why a landed branch proven by its `Change-Id` alone is never pruned.
const CHANGE_ONLY: &str = "landed by Change-Id only; its commits may differ from what landed";

/// The archive refs, name to tip: those this clone holds (one `for-each-ref`) and those the
/// remote holds (one `ls-remote`).
type Archives = (HashMap<String, String>, HashMap<String, String>);

fn archives(git: &Git, remote: &str) -> Result<Archives, String> {
    let local = git
        .run(&[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            ARCHIVE_HEADS,
        ])
        .map_err(|error| error.message())?
        .lines()
        .filter_map(|line| {
            let (oid, name) = line.split_once(' ')?;
            Some((name.to_owned(), oid.to_owned()))
        })
        .collect();
    let pattern = format!("{ARCHIVE_HEADS}*");
    let held = git
        .run(&["ls-remote", remote, &pattern])
        .map_err(|error| error.message())?
        .lines()
        .filter_map(|line| {
            let (oid, name) = line.split_once('\t')?;
            Some((name.to_owned(), oid.trim().to_owned()))
        })
        .collect();
    Ok((local, held))
}

/// The archive ref of branch `name` at `sha`: `refs/bay/archive/heads/<name>`, or, when that
/// holds another tip here or on the remote, the name with `-<short sha>`; and whether the
/// remote already holds it at `sha`. `None` when both names hold another tip.
fn archive_name(name: &str, sha: &str, (local, held): &Archives) -> Option<(String, bool)> {
    let short = &sha[..sha.len().min(12)];
    [
        format!("{ARCHIVE_HEADS}{name}"),
        format!("{ARCHIVE_HEADS}{name}-{short}"),
    ]
    .into_iter()
    .find(|candidate| {
        [local.get(candidate), held.get(candidate)]
            .into_iter()
            .all(|tip| tip.is_none_or(|tip| tip == sha))
    })
    .map(|candidate| {
        let on_remote = held.contains_key(&candidate);
        (candidate, on_remote)
    })
}

/// Archives `sha` as `archive`: the local ref first, then, unless the remote holds it already,
/// a push under an expect-absent lease.
fn archive(
    git: &Git,
    remote: &str,
    archive: &str,
    sha: &str,
    on_remote: bool,
) -> Result<(), String> {
    git.run(&["update-ref", archive, sha])
        .map_err(|error| error.message())?;
    if on_remote {
        return Ok(());
    }
    let lease = format!("--force-with-lease={archive}:");
    let refspec = format!("{archive}:{archive}");
    match git.run_status(&["push", "-q", &lease, remote, &refspec]) {
        Ok((0, _, _)) => Ok(()),
        Ok((_, _, stderr)) => Err(format!("the remote refused it: {stderr}")),
        Err(error) => Err(error.message()),
    }
}

/// Deletes each `landed` branch of `rows` proven by `ancestor` or `patch` on `remote`: after
/// archiving its tip on the remote and appending its restore line to `log`, under a lease of the
/// tip the census judged; then runs the prune adapter for it, when the config sets one, from
/// `trunk_tip`. Returns the pruned and the skipped rows.
fn prune(
    repo: &Repo,
    remote: &str,
    trunk_tip: &str,
    rows: &[Head],
    log: &Path,
) -> (Vec<Value>, Vec<Value>) {
    let git = &repo.git;
    let adapter = repo.config.heads.as_ref().and_then(|heads| {
        let command = heads.on_prune.as_deref()?;
        let timeout = heads.timeout_s.unwrap_or(DEFAULT_ON_PRUNE_TIMEOUT_S);
        Some((command, Duration::from_secs(timeout)))
    });
    let mut pruned = Vec::new();
    let mut skipped = Vec::new();
    let prunable = |row: &Head| row.class.kept().is_none() && row.proof != Some(Proof::Change);
    // Read only when some branch is to be deleted.
    let held = if rows.iter().any(prunable) {
        Some(archives(git, remote))
    } else {
        None
    };
    for row in rows {
        let skip = |why: String| {
            json!({
                "name": row.name,
                "sha": row.sha,
                "class": row.class.name(),
                "proof": row.proof.map(Proof::name),
                "why": why,
            })
        };
        // Only a landed branch is ever deleted.
        if let Some(why) = row.class.kept() {
            skipped.push(skip(why.to_owned()));
            continue;
        }
        // Only one whose every commit is on trunk, by ancestry or by patch.
        if row.proof == Some(Proof::Change) {
            skipped.push(skip(CHANGE_ONLY.to_owned()));
            continue;
        }
        let held = match &held {
            Some(Ok(held)) => held,
            Some(Err(error)) => {
                skipped.push(skip(format!(
                    "the archive refs cannot be read: {error}; not deleted"
                )));
                continue;
            }
            None => continue,
        };
        // The tip is archived on the remote before the delete, so its objects stay.
        let Some((archived, on_remote)) = archive_name(&row.name, &row.sha, held) else {
            skipped.push(skip(format!(
                "both archive names of {} hold another tip; not deleted",
                row.name
            )));
            continue;
        };
        if let Err(error) = archive(git, remote, &archived, &row.sha, on_remote) {
            skipped.push(skip(format!(
                "the tip cannot be archived as {archived}: {error}; not deleted"
            )));
            continue;
        }
        let restore = format!("git push {remote} {archived}:refs/heads/{}", row.name);
        // The restore line is on disk before the delete is sent.
        if let Err(error) = append(log, &format!("{restore}  # {}", row.sha)) {
            skipped.push(skip(format!(
                "the restore line cannot be written to {}: {error}; not deleted",
                log.display()
            )));
            continue;
        }
        // The lease: the remote deletes the branch only while its tip is the one judged.
        let lease = format!("--force-with-lease=refs/heads/{}:{}", row.name, row.sha);
        let branch = format!("refs/heads/{}", row.name);
        match git.run_status(&["push", "-q", "--delete", &lease, remote, &branch]) {
            Ok((0, _, _)) => {
                let mut done = json!({
                    "name": row.name,
                    "sha": row.sha,
                    "proof": row.proof.map(Proof::name),
                    "archive": archived,
                    "restore": restore,
                });
                // N-19: after the delete, never before; its outcome is recorded and never
                // undoes the delete or stops the loop.
                if let Some((command, timeout)) = adapter {
                    done["on_prune"] = on_prune(
                        repo,
                        trunk_tip,
                        command,
                        timeout,
                        [row.name.as_str(), row.sha.as_str(), archived.as_str()],
                    );
                }
                pruned.push(done);
            }
            Ok((_, _, stderr)) => skipped.push(skip(format!(
                "the remote refused the delete (the branch may have moved since the census): {stderr}"
            ))),
            Err(error) => skipped.push(skip(format!(
                "the delete did not run: {}",
                error.message()
            ))),
        }
    }
    (pruned, skipped)
}

/// Runs the prune adapter `command` once for a branch just deleted (N-19), as
/// `<command> pruned <name> <sha> <archive ref>` (`branch`), its program materialized from
/// `trunk_tip`, within `timeout`. Returns `{ok, why}`: `ok` when it exited 0; `why` the exit
/// status and the first line of its stderr when it did not, or why it did not run.
fn on_prune(
    repo: &Repo,
    trunk_tip: &str,
    command: &str,
    timeout: Duration,
    branch: [&str; 3],
) -> Value {
    let [name, sha, archive] = branch;
    match crate::hold::run_from_trunk(
        repo,
        trunk_tip,
        "[heads] on_prune",
        command,
        &[PRUNED, name, sha, archive],
        timeout,
        "prune-run",
    ) {
        Ok((status, _, _)) if status.success() => json!({"ok": true, "why": null}),
        Ok((status, _, stderr)) => {
            let stderr = String::from_utf8_lossy(&stderr);
            let first = stderr
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or_default();
            json!({"ok": false, "why": format!("it exited with {status}: {first}")})
        }
        Err(fault) => json!({"ok": false, "why": format!("it did not run: {fault}")}),
    }
}

/// Appends `line` to `path`, creating it and its folder, and syncs it before returning.
fn append(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    file.sync_all()
}
