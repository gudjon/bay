//! NEW-4: sandbox lanes (`[[lane]]` in `.bay/config.toml`, [`crate::config::Lane`]).
//!
//! A lane names the refs where work that must never reach trunk lives (`refs/heads/research/*`).
//! bay's landing never carries a view's history: a draft is a tree frozen on the change's base,
//! and a publication is a new commit on trunk. What can travel is content: an agent that merges,
//! cherry-picks or checks out a lane branch into its view lands those bytes. So bay:
//!
//! - detects: a merge outside bay ([`crate::unattributed`], G85) whose other parents bring a
//!   lane ref's commits onto trunk names the lane and those commits (`lane_leak`, [`of_landing`])
//!   in `bay status`, the fleet map and `bay watch`;
//! - runs a lane change (`bay start --lane`) to its checks and never lands it
//!   (`LANE_NEVER_LANDS`, refused by the executor and by the bay host hook);
//! - refuses at land a change of no lane whose view reaches a lane ref's commits off trunk
//!   (`LANE_LEAK`, [`reached`]), and denies the shell shapes that copy from a lane ref into a view
//!   of no lane (`crate::deny`).
//!
//! Content copied by hand (an editor's paste, a file copied from another clone) carries no
//! history and names no ref: bay cannot detect it, and does not claim to.
//!
//! A lane ref is read where this clone holds it: under its own name, and, for a branch
//! (`refs/heads/<rest>`), under the remote-tracking name `refs/remotes/<remote>/<rest>`, where
//! [`fetch`] brings the remote's. A lane branch deleted on the remote after it was merged is
//! no longer a tip here once this clone forgets it.
//!
//! The cost is bounded: one ref listing, at most [`MAX_TIPS`] tips, and for each tip one
//! `merge-base --is-ancestor`, one `merge-base --all` per head judged, and one bounded
//! `rev-list` per tip that leaks.

use serde_json::{Value, json};

use crate::config::{Config, Lane};
use crate::git::Git;
use crate::repo::Repo;

/// The most lane tips one judgement reads; past it, the rest are not judged and the reader is
/// told (`truncated`).
pub const MAX_TIPS: usize = 64;

/// The most commits one leak names; `count` counts them all.
pub const MAX_COMMITS: usize = 20;

/// Whether ref name `name` matches lane pattern `pattern`: equal, or, for a pattern with a `*`,
/// the same text before and after it, the `*` matching any run of characters, slashes too.
#[must_use]
pub fn matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((prefix, suffix)) => {
            name.len() >= prefix.len() + suffix.len()
                && name.starts_with(prefix)
                && name.ends_with(suffix)
        }
    }
}

/// The names a lane pattern's refs are held under in a clone of `remote`: the pattern itself,
/// and for a branch pattern its remote-tracking form.
#[must_use]
pub fn local_patterns(pattern: &str, remote: &str) -> Vec<String> {
    let mut patterns = vec![pattern.to_owned()];
    if let Some(rest) = pattern.strip_prefix("refs/heads/") {
        patterns.push(format!("refs/remotes/{remote}/{rest}"));
    }
    patterns
}

/// The fetch refspec that brings a lane pattern's refs from the remote: a branch into its
/// remote-tracking name, any other ref into its own name.
#[must_use]
pub fn refspec(pattern: &str, remote: &str) -> String {
    match pattern.strip_prefix("refs/heads/") {
        Some(rest) => format!("+{pattern}:refs/remotes/{remote}/{rest}"),
        None => format!("+{pattern}:{pattern}"),
    }
}

/// The lane a ref name is a ref of, among `lanes`, in a clone of `remote`.
#[must_use]
pub fn lane_of<'a>(lanes: &'a [Lane], remote: &str, name: &str) -> Option<&'a Lane> {
    lanes.iter().find(|lane| {
        lane.refs.iter().any(|pattern| {
            local_patterns(pattern, remote)
                .iter()
                .any(|local| matches(local, name))
        })
    })
}

/// One lane ref this clone holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tip {
    pub lane: String,
    /// The ref name as this clone holds it.
    pub name: String,
    pub commit: String,
}

/// The lane refs this clone holds: what `lanes` names, at most [`MAX_TIPS`] of them in ref
/// name order, and whether more were left out. One `for-each-ref`; no lane, no Git call.
///
/// # Errors
///
/// A git fault.
pub fn tips(git: &Git, lanes: &[Lane], remote: &str) -> Result<(Vec<Tip>, bool), String> {
    if lanes.is_empty() {
        return Ok((Vec::new(), false));
    }
    // `for-each-ref` takes a prefix up to a slash: the text before each `*`, cut back to its
    // last slash, and the pattern matched here.
    let mut listed: Vec<String> = Vec::new();
    for lane in lanes {
        for pattern in &lane.refs {
            for local in local_patterns(pattern, remote) {
                let prefix = match local.find('*') {
                    None => local,
                    Some(star) => match local[..star].rfind('/') {
                        Some(at) => local[..=at].to_owned(),
                        None => local[..star].to_owned(),
                    },
                };
                if !listed.contains(&prefix) {
                    listed.push(prefix);
                }
            }
        }
    }
    let mut args = vec!["for-each-ref", "--format=%(objectname) %(refname)"];
    args.extend(listed.iter().map(String::as_str));
    let refs = git.run(&args).map_err(|error| error.message())?;
    let mut tips: Vec<Tip> = refs
        .lines()
        .filter_map(|line| {
            let (commit, name) = line.split_once(' ')?;
            let lane = lane_of(lanes, remote, name)?;
            Some(Tip {
                lane: lane.name.clone(),
                name: name.to_owned(),
                commit: commit.to_owned(),
            })
        })
        .collect();
    tips.sort_by(|a, b| a.name.cmp(&b.name));
    tips.dedup_by(|a, b| a.name == b.name);
    let truncated = tips.len() > MAX_TIPS;
    tips.truncate(MAX_TIPS);
    Ok((tips, truncated))
}

/// The lane tips this clone holds under the config committed at `trunk`, with the remote of
/// this repository's bootstrap (`origin` when it names none).
///
/// # Errors
///
/// The config at `trunk` cannot be read, or a git fault.
pub fn tips_at(git: &Git, trunk: &str) -> Result<(Vec<Lane>, Vec<Tip>, bool), String> {
    let config = Config::at(git, trunk)?;
    if config.lanes.is_empty() {
        return Ok((Vec::new(), Vec::new(), false));
    }
    let remote = crate::repo::Bootstrap::read(git).map_or_else(
        |_| crate::repo::DEFAULT_REMOTE.to_owned(),
        |bootstrap| bootstrap.remote,
    );
    let (tips, truncated) = tips(git, &config.lanes, &remote)?;
    Ok((config.lanes, tips, truncated))
}

/// Brings the remote's lane refs into this clone ([`refspec`]), best effort: offline, or for a
/// ref the remote lacks, what this clone holds is what a judgement reads. No lane, no fetch.
pub fn fetch(repo: &Repo) {
    let refspecs: Vec<String> = repo
        .config
        .lanes
        .iter()
        .flat_map(|lane| lane.refs.iter())
        .map(|pattern| refspec(pattern, repo.remote()))
        .collect();
    if refspecs.is_empty() {
        return;
    }
    let mut args = vec!["fetch", "-q", "--no-tags", repo.remote()];
    args.extend(refspecs.iter().map(String::as_str));
    if repo.git.run(&args).is_ok() {
        return;
    }
    // One exact ref the remote lacks fails the whole fetch: the others are fetched one by one.
    for refspec in &refspecs {
        let _ = repo
            .git
            .run(&["fetch", "-q", "--no-tags", repo.remote(), refspec]);
    }
}

/// The commits of one lane tip that a judged history reaches off its base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leak {
    pub lane: String,
    /// The lane ref, as this clone holds it.
    pub name: String,
    /// At most [`MAX_COMMITS`] of the commits, newest first.
    pub commits: Vec<String>,
    /// How many there are.
    pub count: u64,
}

impl Leak {
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "lane": self.lane,
            "ref": self.name,
            "commits": self.commits,
            "count": self.count,
        })
    }
}

/// The lane commits the histories of `heads` reach that are not in `base`'s history (all of
/// them when `base` is `None`, for a root): one [`Leak`] per lane tip that shares such a commit
/// with them. A tip already in `base`'s history leaks nothing new.
///
/// # Errors
///
/// A git fault.
pub fn reached(
    git: &Git,
    tips: &[Tip],
    heads: &[&str],
    base: Option<&str>,
) -> Result<Vec<Leak>, String> {
    let on_base = |commit: &str| base.map_or(Ok(false), |base| git.is_ancestor(commit, base));
    let mut leaks = Vec::new();
    for tip in tips {
        if on_base(tip.commit.as_str())? {
            continue;
        }
        // The newest commits each head shares with the tip; one off the base is a leak.
        let mut shared: Vec<String> = Vec::new();
        for &head in heads {
            let (code, out, stderr) = git
                .run_status(&["merge-base", "--all", head, &tip.commit])
                .map_err(|error| error.message())?;
            match code {
                0 => {}
                1 => continue,
                _ => return Err(format!("merge-base --all {head} {}: {stderr}", tip.name)),
            }
            for commit in out.lines().map(str::trim).filter(|line| !line.is_empty()) {
                if !shared.iter().any(|seen| seen == commit) && !on_base(commit)? {
                    shared.push(commit.to_owned());
                }
            }
        }
        if shared.is_empty() {
            continue;
        }
        let mut range = shared;
        if let Some(base) = base {
            range.push(format!("^{base}"));
        }
        let max = MAX_COMMITS.to_string();
        let mut listed: Vec<&str> = vec!["rev-list", "-n", max.as_str()];
        listed.extend(range.iter().map(String::as_str));
        let commits = git.run(&listed).map_err(|error| error.message())?;
        let mut counted: Vec<&str> = vec!["rev-list", "--count"];
        counted.extend(range.iter().map(String::as_str));
        let count = git.run(&counted).map_err(|error| error.message())?;
        leaks.push(Leak {
            lane: tip.lane.clone(),
            name: tip.name.clone(),
            commits: commits.lines().map(str::to_owned).collect(),
            count: count
                .trim()
                .parse()
                .map_err(|_| format!("git rev-list --count printed {count:?}"))?,
        });
    }
    Ok(leaks)
}

/// The lane leaks of trunk commit `commit` with parents `parents` (`%P`), which reached trunk
/// outside bay: the commits off trunk's first-parent line that a merge's other parents bring,
/// judged against its first parent. A commit with one parent brings none off that line: a lane
/// branch fast-forwarded onto trunk, or a lane commit cherry-picked there, is named as an
/// `outside` landing, never as a lane's, since a lane branch forked from a trunk commit reaches
/// that commit too.
///
/// # Errors
///
/// A git fault.
pub fn of_landing(
    git: &Git,
    tips: &[Tip],
    commit: &str,
    parents: &str,
) -> Result<Vec<Leak>, String> {
    let parents: Vec<&str> = parents.split_whitespace().collect();
    let [first, others @ ..] = parents.as_slice() else {
        return Ok(Vec::new());
    };
    if tips.is_empty() || others.is_empty() {
        return Ok(Vec::new());
    }
    reached(git, tips, others, Some(*first)).map_err(|why| format!("{commit}: {why}"))
}

/// `leaks` as a row's `lane_leak`, and the first lane they name (`lane`).
#[must_use]
pub fn row(leaks: &[Leak]) -> (Value, Value) {
    (
        leaks.first().map_or(Value::Null, |leak| json!(leak.lane)),
        leaks.iter().map(Leak::to_json).collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::{local_patterns, matches, refspec};

    #[test]
    fn a_pattern_matches_its_refs_and_no_other() {
        assert!(matches("refs/heads/research/*", "refs/heads/research/a"));
        assert!(matches("refs/heads/research/*", "refs/heads/research/a/b"));
        assert!(!matches("refs/heads/research/*", "refs/heads/research"));
        assert!(!matches("refs/heads/research/*", "refs/heads/main"));
        assert!(matches("refs/heads/proto-*", "refs/heads/proto-1"));
        assert!(matches("refs/sandbox/x", "refs/sandbox/x"));
        assert!(!matches("refs/sandbox/x", "refs/sandbox/xy"));
        assert!(matches("refs/heads/*-wip", "refs/heads/a-wip"));
        assert!(!matches("refs/heads/*-wip", "refs/heads/-wi"));
    }

    #[test]
    fn a_branch_pattern_is_also_read_and_fetched_as_its_remote_tracking_ref() {
        assert_eq!(
            local_patterns("refs/heads/research/*", "origin"),
            ["refs/heads/research/*", "refs/remotes/origin/research/*"]
        );
        assert_eq!(
            local_patterns("refs/sandbox/*", "origin"),
            ["refs/sandbox/*"]
        );
        assert_eq!(
            refspec("refs/heads/research/*", "up"),
            "+refs/heads/research/*:refs/remotes/up/research/*"
        );
        assert_eq!(
            refspec("refs/sandbox/*", "up"),
            "+refs/sandbox/*:refs/sandbox/*"
        );
    }
}
