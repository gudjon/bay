//! Merging a draft onto trunk without touching any workspace (`DESIGN` § 6 step 2).

use crate::git::{Git, GitError};

pub enum Merge {
    /// The merged result tree.
    Clean(String),
    /// The paths that conflict, and the result tree with conflict markers in them.
    Conflict(Vec<String>, String),
}

/// `git merge-tree --write-tree <trunk> <draft>`: the merge base is found by Git.
///
/// # Errors
///
/// A git fault (a conflict is not one: it is [`Merge::Conflict`]).
pub fn merge_onto(git: &Git, trunk: &str, draft: &str) -> Result<Merge, GitError> {
    merge_tree(git, None, trunk, draft)
}

/// The tree that reverts `commit` onto `trunk` (`bay undo`, ruling R10):
/// `git merge-tree --write-tree --merge-base=<commit> <trunk> <commit>^`.
///
/// # Errors
///
/// A git fault (a conflict is not one: it is [`Merge::Conflict`]).
pub fn revert_onto(git: &Git, trunk: &str, commit: &str) -> Result<Merge, GitError> {
    merge_tree(git, Some(commit), trunk, &format!("{commit}^"))
}

fn merge_tree(git: &Git, base: Option<&str>, ours: &str, theirs: &str) -> Result<Merge, GitError> {
    let base = base.map(|base| format!("--merge-base={base}"));
    let mut args = vec!["merge-tree", "--write-tree", "--name-only", "--no-messages"];
    args.extend(base.as_deref());
    args.extend([ours, theirs]);
    let (code, stdout, stderr) = git.run_status(&args)?;
    let mut lines = stdout.lines();
    let tree = lines.next().unwrap_or_default().to_owned();
    match code {
        0 => Ok(Merge::Clean(tree)),
        1 => Ok(Merge::Conflict(
            lines
                .take_while(|line| !line.is_empty())
                .map(str::to_owned)
                .collect(),
            tree,
        )),
        _ => Err(GitError::Failed {
            args: args.join(" "),
            stderr,
        }),
    }
}
