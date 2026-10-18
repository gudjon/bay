//! Merging a draft onto trunk without touching any workspace (`DESIGN` § 6 step 2).

use crate::git::{Git, GitError};

pub enum Merge {
    /// The merged result tree.
    Clean(String),
    /// The paths that conflict.
    Conflict(Vec<String>),
}

/// `git merge-tree --write-tree <trunk> <draft>`: the merge base is found by Git.
///
/// # Errors
///
/// A git fault (a conflict is not one: it is [`Merge::Conflict`]).
pub fn merge_onto(git: &Git, trunk: &str, draft: &str) -> Result<Merge, GitError> {
    let (code, stdout, stderr) = git.run_status(&[
        "merge-tree",
        "--write-tree",
        "--name-only",
        "--no-messages",
        trunk,
        draft,
    ])?;
    let mut lines = stdout.lines();
    let tree = lines.next().unwrap_or_default().to_owned();
    match code {
        0 => Ok(Merge::Clean(tree)),
        1 => Ok(Merge::Conflict(
            lines
                .take_while(|line| !line.is_empty())
                .map(str::to_owned)
                .collect(),
        )),
        _ => Err(GitError::Failed {
            args: format!("merge-tree --write-tree {trunk} {draft}"),
            stderr,
        }),
    }
}
