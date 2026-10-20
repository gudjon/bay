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
/// A git fault (a conflict is not one: it is [`Merge::Conflict`]), including a draft that shares
/// no history with trunk: bay never passes `--allow-unrelated-histories` (G211).
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
    // A content merge reads the blobs both sides changed: in a blob-less store, Git fetches them.
    let (code, stdout, stderr) = git.fetching().run_status(&args)?;
    let mut lines = stdout.lines();
    let tree = lines.next().unwrap_or_default().to_owned();
    // G211: Git also exits 1 for a revision it cannot read, printing no tree, so a conflict is
    // exit 1 with a tree id; anything else is a fault and builds no candidate.
    let is_tree = !tree.is_empty() && tree.bytes().all(|byte| byte.is_ascii_hexdigit());
    match code {
        0 if is_tree => Ok(Merge::Clean(tree)),
        1 if is_tree => Ok(Merge::Conflict(
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
