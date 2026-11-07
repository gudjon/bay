//! Merging a draft onto trunk without touching any workspace (`DESIGN` § 6 step 2).

use crate::git::{Git, GitError};

/// Git's variable naming the tree it reads `.gitattributes` from (`--attr-source`).
pub const ATTR_SOURCE: &str = "GIT_ATTR_SOURCE";

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
    // BACKLOG 67 item 1a: merge attributes (`merge=union` for an append-only list) come from
    // `ours`, main, never from the change merged onto it, so no change picks how it is merged.
    let (code, stdout, stderr) = git
        .fetching()
        .run_status_with(&args, &[(ATTR_SOURCE, ours)])?;
    let mut lines = stdout.lines();
    let tree = lines.next().unwrap_or_default().to_owned();
    // G211: Git also exits 1 for a revision it cannot read, printing no tree, so a conflict is
    // exit 1 with a tree id; anything else is a fault and builds no candidate.
    let is_tree = !tree.is_empty() && tree.bytes().all(|byte| byte.is_ascii_hexdigit());
    match code {
        0 if is_tree => settle_revision(git, base.as_deref(), ours, theirs, Vec::new(), tree),
        1 if is_tree => {
            let paths: Vec<String> = lines
                .take_while(|line| !line.is_empty())
                .map(str::to_owned)
                .collect();
            match manifest_resolved(git, base.as_deref(), ours, theirs, paths, tree)? {
                Merge::Clean(tree) => {
                    settle_revision(git, base.as_deref(), ours, theirs, Vec::new(), tree)
                }
                Merge::Conflict(paths, tree) => {
                    settle_revision(git, base.as_deref(), ours, theirs, paths, tree)
                }
            }
        }
        _ => Err(GitError::Failed {
            args: args.join(" "),
            stderr,
        }),
    }
}

/// BACKLOG 49: a conflict in `.bay-assets` alone is no conflict of the work. Its lines are
/// keyed by path, so it is merged path by path (`crate::assets::merge3`): two changes that each
/// add a file merge, and only a path both sides changed differently stays a conflict. The
/// resolved manifest replaces the marked one in `tree`; the other paths stay as Git left them.
fn manifest_resolved(
    git: &Git,
    base: Option<&str>,
    ours: &str,
    theirs: &str,
    mut paths: Vec<String>,
    tree: String,
) -> Result<Merge, GitError> {
    if !paths.iter().any(|path| path == crate::assets::MANIFEST) {
        return Ok(Merge::Conflict(paths, tree));
    }
    let base = match base {
        Some(base) => base.trim_start_matches("--merge-base=").to_owned(),
        None => git.run(&["merge-base", ours, theirs])?,
    };
    let text = |commit: &str| {
        git.file_at(commit, crate::assets::MANIFEST)
            .map(Option::unwrap_or_default)
    };
    let Some(merged) = crate::assets::merge3(&text(&base)?, &text(ours)?, &text(theirs)?) else {
        return Ok(Merge::Conflict(paths, tree));
    };
    let tree = with_file(git, &tree, crate::assets::MANIFEST, &merged)?;
    paths.retain(|path| path != crate::assets::MANIFEST);
    Ok(if paths.is_empty() {
        Merge::Clean(tree)
    } else {
        Merge::Conflict(paths, tree)
    })
}

/// A temporary file's path no other merge in this process or another uses.
fn scratch_file(name: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let next = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("bay-merge-{}-{next}.{name}", std::process::id()))
}

/// `tree` with `path` holding `text`.
fn with_file(git: &Git, tree: &str, path: &str, text: &str) -> Result<String, GitError> {
    let blob = git.run_with(
        &["hash-object", "-w", "--stdin"],
        &[],
        Some(text.as_bytes()),
    )?;
    let index = scratch_file("index");
    let index_text = index.display().to_string();
    let envs = [("GIT_INDEX_FILE", index_text.as_str())];
    let resolved = git
        .run_with(&["read-tree", tree], &envs, None)
        .and_then(|_| {
            git.run_with(
                &[
                    "update-index",
                    "--cacheinfo",
                    &format!("100644,{blob},{path}"),
                ],
                &envs,
                None,
            )
        })
        .and_then(|_| git.run_with(&["write-tree"], &envs, None));
    let _ = std::fs::remove_file(&index);
    resolved
}

/// The top-level `revision = <n>` of a policy's text: its line's index and its number.
fn stated_revision(text: &str) -> Option<(usize, u64)> {
    let mut lines = text.lines().enumerate();
    lines.find_map(|(index, line)| {
        let (key, value) = line.split_once('=')?;
        let number = value.split('#').next()?.trim().parse().ok()?;
        (key.trim() == "revision").then_some((index, number))
    })
}

/// `text` with its `revision` line stating `revision`, whatever follows it on the line.
fn with_revision(text: &str, at: usize, revision: u64) -> String {
    let mut out = String::new();
    for (index, line) in text.lines().enumerate() {
        if index == at {
            let comment = line.split_once('#').map(|(_, comment)| comment);
            out.push_str("revision = ");
            out.push_str(&revision.to_string());
            if let Some(comment) = comment {
                out.push_str(" #");
                out.push_str(comment);
            }
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// G675 (a lint change sat blocked 262 s: it edited a check's script and had not raised the
/// policy's `revision`). `revision` only says the policy changed, so a merged `tree` that changes
/// the policy or an adapter path of main's policy states main's revision plus one, unless it
/// already states more. A policy main or the tree cannot read is left as it is, to its refusal.
///
/// # Errors
///
/// A git fault.
pub fn raise_revision(git: &Git, trunk: &str, tree: &str) -> Result<String, String> {
    let Ok(Some(base)) = crate::policy::load_at(git, trunk) else {
        return Ok(tree.to_owned());
    };
    if !crate::policy::policy_changed(git, trunk, tree, Some(&base))? {
        return Ok(tree.to_owned());
    }
    let path = crate::policy::POLICY_PATH;
    let Some(text) = git.file_at(tree, path).map_err(|error| error.message())? else {
        return Ok(tree.to_owned());
    };
    let Some((at, stated)) = stated_revision(&text) else {
        return Ok(tree.to_owned());
    };
    let main = crate::policy::revision_at(git, trunk)?
        .unwrap_or(0)
        .max(base.revision);
    if stated > main {
        return Ok(tree.to_owned());
    }
    with_file(git, tree, path, &with_revision(&text, at, main + 1)).map_err(|error| error.message())
}

/// G668 (a fleet finding, 2026-11-06: a policy edited 58 times in a week, and two unrelated edits
/// collided on `revision`, twice in one hour; two that raise it to one number merge cleanly and
/// then fail "must raise"). `revision` only says the policy changed, so when main and a change
/// both raised it, that is no disagreement: the rest of the file merges as ever, and the result
/// states the next number. A change that did not raise it is left to its refusal, and a conflict
/// in the rest of the file stays one.
fn settle_revision(
    git: &Git,
    base: Option<&str>,
    ours: &str,
    theirs: &str,
    mut paths: Vec<String>,
    tree: String,
) -> Result<Merge, GitError> {
    let unchanged = |paths: Vec<String>, tree: String| {
        if paths.is_empty() {
            Merge::Clean(tree)
        } else {
            Merge::Conflict(paths, tree)
        }
    };
    let path = crate::policy::POLICY_PATH;
    // One read for the common case, a change that leaves the policy alone.
    let blob = |commit: &str| format!("{commit}:{path}");
    let (ours_blob, theirs_blob) = (blob(ours), blob(theirs));
    let (_, blobs, _) = git.run_status_with(&["rev-parse", &ours_blob, &theirs_blob], &[])?;
    let blobs: Vec<&str> = blobs.lines().collect();
    if blobs.len() != 2 || blobs[0] == blobs[1] {
        return Ok(unchanged(paths, tree));
    }
    let base = match base {
        Some(base) => base.trim_start_matches("--merge-base=").to_owned(),
        None => git.run(&["merge-base", ours, theirs])?,
    };
    let text = |commit: &str| git.file_at(commit, path);
    let (Some(base_text), Some(ours_text), Some(theirs_text)) =
        (text(&base)?, text(ours)?, text(theirs)?)
    else {
        return Ok(unchanged(paths, tree));
    };
    let (Some(at_base), Some(at_ours), Some(at_theirs)) = (
        stated_revision(&base_text),
        stated_revision(&ours_text),
        stated_revision(&theirs_text),
    ) else {
        return Ok(unchanged(paths, tree));
    };
    let start = at_base.1;
    if at_ours.1 <= start || at_theirs.1 <= start {
        return Ok(unchanged(paths, tree));
    }
    // The three texts, stating one number, so that only the rest of the file can disagree.
    let files = [
        (
            scratch_file("ours"),
            with_revision(&ours_text, at_ours.0, start),
        ),
        (
            scratch_file("base"),
            with_revision(&base_text, at_base.0, start),
        ),
        (
            scratch_file("theirs"),
            with_revision(&theirs_text, at_theirs.0, start),
        ),
    ];
    let written = files
        .iter()
        .try_for_each(|(file, body)| std::fs::write(file, body))
        .map_err(GitError::Spawn);
    let paths_text = files.each_ref().map(|(file, _)| file.display().to_string());
    let outcome = written.and_then(|()| {
        git.run_status_with(
            &[
                "merge-file",
                "-p",
                &paths_text[0],
                &paths_text[1],
                &paths_text[2],
            ],
            &[],
        )
    });
    for (file, _) in &files {
        let _ = std::fs::remove_file(file);
    }
    let (code, merged, _) = outcome?;
    if code != 0 {
        return Ok(unchanged(paths, tree));
    }
    let next = at_theirs.1.max(at_ours.1 + 1);
    let Some((at, _)) = stated_revision(&merged) else {
        return Ok(unchanged(paths, tree));
    };
    let tree = with_file(git, &tree, path, &with_revision(&merged, at, next))?;
    paths.retain(|conflicted| conflicted != path);
    Ok(unchanged(paths, tree))
}
