//! `bay status --stranded`: work that exists only on this disk (G156).
//!
//! A local branch whose tip no remote-tracking ref contains holds commits that no remote has:
//! a disk that is lost loses them. Each one is listed with the commits it holds that no
//! remote-tracking ref reaches (`ahead`), the age of its tip and the worktree it is checked out
//! in. A worktree record whose folder is gone (`git worktree list --porcelain` names it
//! `prunable`) is listed too. The read uses local refs only: no fetch, so a remote-tracking ref
//! is as new as this clone's last fetch, and no prune and no delete, so every branch and record
//! it names is still there to recover or remove by hand.
//!
//! `bay status --heads` carries the same findings in its `detail.stranded` (G302), read by
//! [`local`], so one view shows every head: the remote's and those only this disk has.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Value, json};

use crate::git::Git;
use crate::result::BayResult;

#[must_use]
pub fn stranded(dir: &Path) -> BayResult {
    run(dir).unwrap_or_else(BayResult::failed)
}

fn run(dir: &Path) -> Result<BayResult, String> {
    let git = Git::new(dir);
    if git.outside_repository().map_err(|error| error.message())? {
        return Err(format!("{} is not in a git repository", dir.display()));
    }
    let mut detail = local(&git)?;
    detail["fetched"] = json!(false);
    detail["note"] = json!(
        "read from local refs only: no fetch, no prune, nothing deleted; a remote-tracking ref is as new as the last fetch"
    );
    Ok(BayResult::ok(detail))
}

/// The stranded findings of the repository `git` runs in, from local refs only: `branches`
/// (each local branch with commits no remote-tracking ref reaches), `prunable_worktrees` (each
/// worktree record whose folder is gone) and `remote_refs` (how many remote-tracking refs the
/// read compared against). Never a fetch, a prune or a delete.
///
/// # Errors
///
/// When Git cannot list the worktrees or the refs, or count a branch's commits.
pub fn local(git: &Git) -> Result<Value, String> {
    let worktrees = worktrees(git)?;
    let checked_out: BTreeMap<&str, &str> = worktrees
        .iter()
        .filter_map(|worktree| Some((worktree.branch.as_deref()?, worktree.path.as_str())))
        .collect();
    let remote_refs = git
        .run(&["for-each-ref", "--format=%(refname)", "refs/remotes/"])
        .map_err(|error| error.message())?
        .lines()
        .count();
    let now = crate::live::unix_now();
    let heads = git
        .run(&[
            "for-each-ref",
            "--format=%(objectname) %(committerdate:unix) %(refname)",
            "refs/heads/",
        ])
        .map_err(|error| error.message())?;
    let mut branches = Vec::new();
    for line in heads.lines() {
        let mut fields = line.splitn(3, ' ');
        let (Some(tip), Some(at), Some(name)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        // The commits of the tip that no remote-tracking ref reaches: none means a remote holds
        // the branch's every commit.
        let ahead: u64 = git
            .run(&["rev-list", "--count", tip, "--not", "--remotes"])
            .map_err(|error| error.message())?
            .parse()
            .map_err(|_| format!("git rev-list gave no count for {name}"))?;
        if ahead == 0 {
            continue;
        }
        let branch = name.trim_start_matches("refs/heads/");
        branches.push(json!({
            "branch": branch,
            "tip": tip,
            "ahead": ahead,
            "age_s": at.parse::<u64>().ok().map(|at| now.saturating_sub(at)),
            "worktree": checked_out.get(name),
        }));
    }
    let prunable: Vec<Value> = worktrees
        .iter()
        .filter_map(|worktree| {
            let why = worktree.prunable.as_ref()?;
            Some(json!({
                "path": worktree.path,
                "branch": worktree
                    .branch
                    .as_deref()
                    .map(|name| name.trim_start_matches("refs/heads/")),
                "head": worktree.head,
                "why": why,
            }))
        })
        .collect();
    Ok(json!({
        "branches": branches,
        "prunable_worktrees": prunable,
        "remote_refs": remote_refs,
    }))
}

/// One record of `git worktree list --porcelain`.
struct Worktree {
    path: String,
    head: Option<String>,
    /// The full ref name of the branch checked out, if any.
    branch: Option<String>,
    /// Why Git would prune the record (its folder is gone), when it would.
    prunable: Option<String>,
}

fn worktrees(git: &Git) -> Result<Vec<Worktree>, String> {
    let text = git
        .run(&["worktree", "list", "--porcelain"])
        .map_err(|error| error.message())?;
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut worktree = Worktree {
            path: String::new(),
            head: None,
            branch: None,
            prunable: None,
        };
        for line in block.lines() {
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "worktree" => value.clone_into(&mut worktree.path),
                "HEAD" => worktree.head = Some(value.to_owned()),
                "branch" => worktree.branch = Some(value.to_owned()),
                "prunable" => worktree.prunable = Some(value.to_owned()),
                _ => {}
            }
        }
        if !worktree.path.is_empty() {
            out.push(worktree);
        }
    }
    Ok(out)
}
