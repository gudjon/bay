//! `bay save <intent>` (G279, owner ruling 2026-10-21): the edits a person made directly in
//! the repository's main checkout, never through `bay start`, land as one change in one step.
//!
//! bay collects the working tree's changes against `HEAD` (`git status --porcelain=v2 -z`:
//! modified, added untracked files that are not ignored, and deleted files) and commits them on
//! `HEAD` through an index of its own, so the checkout's index and branch are never staged or
//! committed in. The changed files are the declared paths of a normal `bay start`, whose first
//! draft is that commit merged onto the current trunk the way `start --from` takes a branch in
//! (`merge_onto`, with `HEAD` as the merge base). `bay merge --wait` follows, with its result
//! semantics. On `landed` the checkout's trunk branch fast-forwards to the landing, once the
//! saved files still hold what was saved, so no edit is lost. Any refusal leaves the checkout
//! exactly as it was, and names the change when one was opened, for `bay start <change>`.
//!
//! `--message` is the first draft's commit message, kept in the change's chain. It is never the
//! intent's revision, which would name another intent, and the trunk commit keeps bay's own
//! message and trailers.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::change::{Chain, commit_on};
use crate::git::{Git, GitError};
use crate::inputs;
use crate::merge::{Merge, merge_onto};
use crate::publish;
use crate::repo::Repo;
use crate::result::{BayResult, Decision, Reason};
use crate::start::{self, Saved, StartRequest};

pub struct SaveRequest {
    /// What this work is, in a few words (or a card id). Required.
    pub intent: String,
    /// The first draft's commit message; `bay save <intent>` when not given.
    pub message: Option<String>,
}

/// The states of a main checkout that is in the middle of another operation, by the file Git
/// keeps in its git directory while it lasts.
const IN_PROGRESS: [(&str, &str); 5] = [
    ("MERGE_HEAD", "a merge"),
    ("rebase-merge", "a rebase"),
    ("rebase-apply", "a rebase"),
    ("CHERRY_PICK_HEAD", "a cherry-pick"),
    ("REVERT_HEAD", "a revert"),
];

#[must_use]
pub fn save(dir: &Path, request: &SaveRequest) -> BayResult {
    let repo = match Repo::open_fetched(dir) {
        Ok(repo) => repo,
        Err(error) => return error.into_result(),
    };
    let mut result = save_in(dir, &repo, request).unwrap_or_else(BayResult::failed);
    // G80: every result says which trunk it reflects, and when that trunk was fetched.
    result.freshness = result.freshness.or_else(|| repo.freshness());
    result
}

fn save_in(dir: &Path, repo: &Repo, request: &SaveRequest) -> Result<BayResult, String> {
    if request.intent.trim().is_empty() {
        return Err(
            "bay save needs to know what this work is; say it in a few words (or a card id)"
                .to_owned(),
        );
    }
    // G279: a view's edits land by `bay merge`; a save is of the main checkout only.
    if let Some(change) = repo.view_change(dir) {
        return Err(format!(
            "bay save runs in the repository's main checkout, and this is the view of change \
             {change}: use bay merge here"
        ));
    }
    main_checkout_only(repo, dir)?;
    let checkout = Git::new(&repo.work_root);
    if let Some(what) = in_progress(repo) {
        return Err(format!(
            "the main checkout is in the middle of {what}: finish it or abort it, then save again"
        ));
    }
    let head = checkout
        .run(&["rev-parse", "--verify", "-q", "HEAD^{commit}"])
        .map_err(|_| "the main checkout has no commit at HEAD to save onto".to_owned())?;
    let trunk = repo.fetch_trunk().map_err(|error| error.message())?;
    if !repo.git.is_ancestor(&head, &trunk)? {
        return Err(format!(
            "the main checkout's HEAD {head} is not on the history of main {} ({trunk}): \
             bay save merges only edits made on main's history",
            repo.trunk()
        ));
    }
    let changed = changes(&checkout)?;
    if let Some(refused) = take_locks(repo, &changed, None) {
        return Ok(refused);
    }
    let (saved, offloaded, undo) = offload(repo, &trunk, &changed)?;
    // Every way this save ends short of a merge, a refusal or an error, leaves the folder as it
    // found it (the manifest and the exclude list): `undo` puts them back when dropped unkept.
    if saved.is_empty() {
        return Ok(BayResult::ok(json!({
            "saved": saved,
            "head": head,
            "why": "nothing to save",
        })));
    }
    let tree = snapshot_tree(repo, &checkout, &head, &saved)?;
    if tree == tree_of(&repo.git, &head)? {
        return Ok(BayResult::ok(json!({
            "saved": Vec::<String>::new(),
            "head": head,
            "why": "nothing to save: the changed files hold what HEAD holds",
        })));
    }
    let message = request
        .message
        .clone()
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| format!("bay save {}", request.intent));
    let snapshot = commit_on(&repo.git, &tree, &head, &message).map_err(|error| error.message())?;

    let started = start_saved(dir, request, &saved, &snapshot, &message);
    if started.decision != Decision::Ok {
        return Ok(untouched(started, &saved));
    }
    let id = started.change.clone().ok_or("start returned no change")?;
    if let Some(refused) = take_locks(repo, &changed, Some(&id)) {
        return Ok(untouched(refused, &saved));
    }
    let detail = started.detail.clone().unwrap_or_default();
    let view = detail["view"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| format!("start of {id} returned no view"))?;
    // An intent whose change is still open (an earlier save that was refused) is adopted:
    // its draft becomes the main checkout's edits on the current trunk.
    if detail.get("adopted").is_some()
        && let Some(refused) = replace_draft(dir, repo, &id, &view, &saved, &snapshot)?
    {
        return Ok(untouched(refused, &saved));
    }

    let landed = publish::land(&view, Some(&id), true, None, false);
    if landed.decision != Decision::Landed {
        return Ok(untouched(landed, &saved));
    }
    if let Some(undo) = &undo {
        undo.keep();
    }
    let commit = landed
        .detail
        .as_ref()
        .and_then(|detail| detail["commit"].as_str())
        .map(str::to_owned);
    let moved = checkout_to_merge(repo, &checkout, (&head, &tree), &saved, commit.as_deref());
    let mut result = landed;
    let detail = with_fields(
        result.detail.take(),
        &[
            ("saved", json!(saved)),
            ("offloaded", json!(offloaded)),
            ("change", json!(id)),
            ("landed", json!(commit)),
            ("checkout", moved),
        ],
    );
    Ok(result.with_detail(detail))
}

/// The main checkout moved to the merge `commit` ([`bring_checkout`]), with the files of the
/// merge kept outside Git that it lacks (BACKLOG 49 phase 2); a file that cannot be fetched is
/// named in `assets`, never a failed save, since the save already merged.
fn checkout_to_merge(
    repo: &Repo,
    checkout: &Git,
    (head, tree): (&str, &str),
    saved: &[String],
    commit: Option<&str>,
) -> Value {
    let Some(commit) = commit else {
        return json!({"moved": false, "why": "the merge named no commit on main"});
    };
    let mut moved = bring_checkout(repo, checkout, head, tree, saved, commit);
    match crate::assets::fill_tree(repo, &repo.work_root, commit, &|_| true) {
        Ok(filled) if !filled.purged.is_empty() => moved["assets_purged"] = json!(filled.purged),
        Ok(_) => {}
        Err(why) => moved["assets"] = json!(why),
    }
    moved
}

/// BACKLOG 57: a person saving takes the lock of each lockable file the save changes, as an
/// agent's edit does, so the merge guard finds it held and a second saver is stopped first.
/// The refusal when one cannot be taken.
/// Taken twice: before anything is made, so a second saver stops first, and again once the
/// change exists, so the lock commit names it (the merge guard counts no other). The session is
/// the caller's (`BAY_SESSION`, which the agent hook gives its session's commands), so an
/// agent's own lock is its save's.
fn take_locks(repo: &Repo, changed: &[Entry], change: Option<&str>) -> Option<BayResult> {
    let targets: Vec<PathBuf> = changed
        .iter()
        .map(|entry| repo.work_root.join(&entry.path))
        .collect();
    let session = std::env::var("BAY_SESSION")
        .ok()
        .or_else(|| crate::agent_pid::session_of_caller(&repo.common_dir))
        .filter(|session| crate::session::SessionId::parse(session).is_some());
    crate::locks::take_for(
        &repo.work_root,
        &targets,
        session.as_deref(),
        change,
        crate::live::unix_now(),
    )
    .err()
    .map(crate::locks::refusal)
}

/// BACKLOG 49: with `[plugin.assets] over_limit = "offload"` in the config, each new file over
/// `[limits] max_file_mib` of the policy at `trunk` goes to the assets plugin, and the manifest
/// takes its place among the saved paths (`crate::assets`). A file Git already tracks is never
/// offloaded: moving the checkout to a merge without it would delete it, so it is judged by the
/// limit as before. Returns the paths to save, the offloaded ones, and how to put the folder
/// back when the save is then refused.
fn offload(repo: &Repo, trunk: &str, changed: &[Entry]) -> Result<Offloads, String> {
    let all: Vec<String> = changed.iter().map(|entry| entry.path.clone()).collect();
    let Some((plugin, limit)) = crate::assets::configured(repo, trunk)? else {
        return Ok((all, Vec::new(), None));
    };
    let new: Vec<String> = changed
        .iter()
        .filter(|entry| !entry.tracked)
        .map(|entry| entry.path.clone())
        .collect();
    let done = crate::assets::offload(
        &plugin,
        &repo.work_root,
        &new,
        limit,
        &crate::assets::exclude_file(repo),
    )?;
    let offloaded: Vec<String> = done.entries.into_iter().map(|entry| entry.path).collect();
    if offloaded.is_empty() {
        return Ok((all, offloaded, None));
    }
    let mut saved: Vec<String> = all
        .into_iter()
        .filter(|path| !offloaded.contains(path))
        .collect();
    if !saved.iter().any(|path| path == crate::assets::MANIFEST) {
        saved.push(crate::assets::MANIFEST.to_owned());
        saved.sort();
    }
    Ok((saved, offloaded, done.undo))
}

/// What [`offload`] returns: the paths to save, the offloaded ones, and the undo.
type Offloads = (Vec<String>, Vec<String>, Option<crate::assets::Undo>);

/// G279: the directory bay runs in is the main checkout itself (or a folder in it), never a
/// linked worktree of the repository.
fn main_checkout_only(repo: &Repo, dir: &Path) -> Result<(), String> {
    let toplevel = Git::new(dir)
        .run(&["rev-parse", "--show-toplevel"])
        .map_err(|error| error.message())?;
    let here = PathBuf::from(&toplevel).canonicalize().ok();
    let main = repo.work_root.canonicalize().ok();
    if here.is_none() || here != main {
        return Err(format!(
            "bay save runs in the repository's main checkout ({}), not in {toplevel}",
            repo.work_root.display()
        ));
    }
    Ok(())
}

/// The operation the main checkout is in the middle of: a merge, rebase, cherry-pick or revert.
fn in_progress(repo: &Repo) -> Option<&'static str> {
    IN_PROGRESS
        .iter()
        .find(|(file, _)| repo.common_dir.join(file).exists())
        .map(|(_, what)| *what)
}

/// One changed path of the main checkout: tracked (modified, deleted, staged) or untracked.
struct Entry {
    path: String,
    tracked: bool,
}

/// The working tree's changes against `HEAD`, from `git status --porcelain=v2 -z` with every
/// untracked file listed and no rename pairing: each path once, sorted. An ignored file is not
/// listed, so it is never saved. A conflicted path or a submodule is refused.
fn changes(checkout: &Git) -> Result<Vec<Entry>, String> {
    let out = checkout
        .run_bytes(
            &[
                "--no-optional-locks",
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=all",
                "--no-renames",
            ],
            &[],
            None,
        )
        .map_err(|error| error.message())?;
    let text = String::from_utf8(out)
        .map_err(|_| "git status names a path that is not UTF-8; bay save cannot name it")?;
    // Each path once, sorted; tracked when any of its entries is.
    let mut tracked = std::collections::BTreeSet::new();
    let mut untracked = std::collections::BTreeSet::new();
    let mut fields = text.split('\0');
    while let Some(field) = fields.next() {
        match field.split_once(' ') {
            // `1 XY sub mH mI mW hH hI path`
            Some(("1", rest)) => {
                let parts: Vec<&str> = rest.splitn(8, ' ').collect();
                let [_, sub, _, _, _, _, _, path] = parts[..] else {
                    return Err(format!(
                        "git status printed an entry bay cannot read: {field}"
                    ));
                };
                no_submodule(sub, path)?;
                tracked.insert(path.to_owned());
            }
            // `2 XY sub mH mI mW hH hI Xscore path`, then the original path.
            Some(("2", rest)) => {
                let parts: Vec<&str> = rest.splitn(9, ' ').collect();
                let [_, sub, _, _, _, _, _, _, path] = parts[..] else {
                    return Err(format!(
                        "git status printed an entry bay cannot read: {field}"
                    ));
                };
                no_submodule(sub, path)?;
                tracked.insert(path.to_owned());
                if let Some(original) = fields.next().filter(|original| !original.is_empty()) {
                    tracked.insert(original.to_owned());
                }
            }
            Some(("u", rest)) => {
                let path = rest.splitn(10, ' ').nth(9).unwrap_or(rest);
                return Err(format!(
                    "{path} has an unresolved conflict in the main checkout: resolve it, then \
                     save again"
                ));
            }
            Some(("?", path)) => {
                untracked.insert(path.to_owned());
            }
            // `!` (ignored) and `#` (headers) are never asked for, and an empty field ends the
            // list; none is saved.
            None | Some(_) => {}
        }
    }
    let mut entries: std::collections::BTreeMap<String, bool> =
        untracked.into_iter().map(|path| (path, false)).collect();
    entries.extend(tracked.into_iter().map(|path| (path, true)));
    Ok(entries
        .into_iter()
        .map(|(path, tracked)| Entry { path, tracked })
        .collect())
}

fn no_submodule(sub: &str, path: &str) -> Result<(), String> {
    if sub.starts_with('S') {
        return Err(format!(
            "{path} is a submodule; bay save saves files, never a submodule's state"
        ));
    }
    Ok(())
}

/// The tree of `head` with the working tree's `paths` as they are now, written through an
/// index of bay's own, so the main checkout's index is never staged in. Each path is added as
/// `git add --all` would add it (its filters and modes), or removed when it is gone.
fn snapshot_tree(
    repo: &Repo,
    checkout: &Git,
    head: &str,
    paths: &[String],
) -> Result<String, String> {
    let dir = repo.common_dir.join("bay");
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("{} could not be made: {error}", dir.display()))?;
    let index = dir.join(format!("save-{}.index", std::process::id()));
    let index_text = index.display().to_string();
    let envs = [("GIT_INDEX_FILE", index_text.as_str())];
    let list: Vec<u8> = paths
        .iter()
        .flat_map(|path| path.bytes().chain(std::iter::once(0)))
        .collect();
    let _ = std::fs::remove_file(&index);
    let written = write_snapshot(checkout, &envs, head, &list);
    let _ = std::fs::remove_file(&index);
    written.map_err(|error| error.message())
}

/// [`snapshot_tree`] in the index `envs` names: `head`'s tree, then the NUL-separated `paths`
/// added as they are in the working tree, taken literally (never as globs).
fn write_snapshot(
    checkout: &Git,
    envs: &[(&str, &str)],
    head: &str,
    paths: &[u8],
) -> Result<String, GitError> {
    checkout.run_with(&["read-tree", head], envs, None)?;
    checkout.run_with(
        &[
            "--literal-pathspecs",
            "add",
            "--all",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ],
        envs,
        Some(paths),
    )?;
    checkout.run_with(&["write-tree"], envs, None)
}

/// The normal start of the save's change (`bay start <intent> --paths <files>`), whose draft is
/// `snapshot` merged onto trunk. An intent whose change already landed claims its next
/// generation, so each save of one card is a change of its own.
fn start_saved(
    dir: &Path,
    request: &SaveRequest,
    saved: &[String],
    snapshot: &str,
    message: &str,
) -> BayResult {
    let start_request = |new: bool| StartRequest {
        intent: Some(request.intent.clone()),
        new,
        intent_revision: "1".to_owned(),
        paths: saved.to_vec(),
        inputs: Vec::new(),
        resume: None,
        reverts: None,
        fixes: None,
        lane: None,
        from: None,
        after: Vec::new(),
        saved: Some(Saved {
            commit: snapshot.to_owned(),
            message: message.to_owned(),
        }),
    };
    let started = start::start(dir, &start_request(false));
    if started.reason == Some(Reason::ChangeAlreadyLanded) {
        return start::start(dir, &start_request(true));
    }
    started
}

/// An adopted change (the intent's change is still open, after a refused save) takes the main
/// checkout's edits: it is resumed onto the current trunk, which checkpoints its view first so
/// its earlier draft stays in its chain, and its view is set to `snapshot` merged onto that
/// trunk. `Some` is the refusal: the change's declared paths do not cover the saved files, or
/// the merge conflicts.
fn replace_draft(
    dir: &Path,
    repo: &Repo,
    id: &str,
    view: &Path,
    saved: &[String],
    snapshot: &str,
) -> Result<Option<BayResult>, String> {
    let (_, record) = Chain::new(&repo.git)
        .read(id)
        .map_err(|error| error.message())?;
    let declared = inputs::paths(&record.declared_inputs);
    let uncovered: Vec<&String> = saved
        .iter()
        .filter(|path| {
            !declared.is_empty() && !declared.iter().any(|outer| inputs::within(outer, path))
        })
        .collect();
    if !uncovered.is_empty() {
        return Err(format!(
            "the intent's open change {id} declares {declared:?}, which does not cover \
             {uncovered:?}: save under another intent, or repair {id} with bay start {id}"
        ));
    }
    let resumed = start::start(
        dir,
        &StartRequest {
            intent: None,
            new: false,
            intent_revision: "1".to_owned(),
            paths: Vec::new(),
            inputs: Vec::new(),
            resume: Some(id.to_owned()),
            reverts: None,
            fixes: None,
            lane: None,
            from: None,
            after: Vec::new(),
            saved: None,
        },
    );
    if resumed.decision != Decision::Ok {
        return Ok(Some(resumed));
    }
    let base = resumed
        .detail
        .as_ref()
        .and_then(|detail| detail["base"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| format!("resume of {id} returned no base"))?;
    match merge_onto(&repo.git, &base, snapshot).map_err(|error| error.message())? {
        Merge::Conflict(conflicts, _) => Ok(Some(
            BayResult::blocked(Reason::MergeConflict)
                .with_change(id)
                .with_detail(json!({"conflicts": conflicts, "trunk": base})),
        )),
        Merge::Clean(tree) => {
            // G212: the sparse view's read-tree fetches only the declared files it writes.
            Git::new(view)
                .fetching()
                .writing_tree()
                .run(&["read-tree", "-u", "--reset", &tree])
                .map_err(|error| error.message())?;
            Ok(None)
        }
    }
}

/// A refused save: the main checkout's files are left exactly as they were, and the result
/// says so, with the saved files and, when a change was opened, how to repair it. Approval
/// pending is a queued change, and the detail says so plainly.
fn untouched(mut result: BayResult, saved: &[String]) -> BayResult {
    let mut fields = vec![
        ("saved", json!(saved)),
        (
            "edits",
            json!("left as they were in the main checkout: nothing was moved or removed"),
        ),
    ];
    if let Some(id) = result.change.clone() {
        fields.push(("change", json!(id)));
        fields.push((
            "repair",
            json!(format!(
                "the change stays open: bay start {id} opens its view for repair"
            )),
        ));
    }
    if result.reason == Some(Reason::ApprovalPending) {
        fields.push(("queued", json!(true)));
        fields.push((
            "waiting",
            json!(
                "queued: the change waits for a person's approval and merges once approved; \
                 nothing is lost meanwhile"
            ),
        ));
    }
    let detail = with_fields(result.detail.take(), &fields);
    result.with_detail(detail)
}

/// `detail` with `fields` added: an object keeps its own keys, anything else (a failure's
/// text) is kept under `why`.
fn with_fields(detail: Option<Value>, fields: &[(&str, Value)]) -> Value {
    let mut object = match detail {
        Some(Value::Object(object)) => object,
        None | Some(Value::Null) => serde_json::Map::new(),
        Some(other) => {
            let mut object = serde_json::Map::new();
            object.insert("why".to_owned(), other);
            object
        }
    };
    for (key, value) in fields {
        object.insert((*key).to_owned(), value.clone());
    }
    Value::Object(object)
}

/// G279: after the landing `commit`, the main checkout's trunk branch fast-forwards there, and
/// its files with it. Only when `HEAD` is on trunk's branch, still at `head` and an ancestor of
/// `commit`; when the saved files still hold what was saved (`tree`), so the landing holds
/// every edit; when no other tracked file changed meanwhile; and when no file the landing adds
/// would overwrite one in the working tree. Otherwise `HEAD` and every file stay, and the
/// answer says why. An untracked file the save did not include is never touched.
fn bring_checkout(
    repo: &Repo,
    checkout: &Git,
    head: &str,
    tree: &str,
    saved: &[String],
    commit: &str,
) -> Value {
    match may_move(repo, checkout, head, tree, saved, commit) {
        Ok(Some(why)) | Err(why) => json!({"moved": false, "head": head, "why": why}),
        Ok(None) => match move_checkout(repo, checkout, head, commit) {
            Ok(()) => json!({"moved": true, "from": head, "to": commit}),
            Err(MoveError::Unmoved(why)) => json!({"moved": false, "head": head, "why": why}),
            Err(MoveError::Partly(why)) => json!({
                "moved": true,
                "from": head,
                "to": commit,
                "incomplete": why,
            }),
        },
    }
}

/// Why the main checkout may not move to `commit`, or `None` when it may.
fn may_move(
    repo: &Repo,
    checkout: &Git,
    head: &str,
    tree: &str,
    saved: &[String],
    commit: &str,
) -> Result<Option<String>, String> {
    let branch = format!("refs/heads/{}", repo.trunk());
    let (code, on, _) = checkout
        .run_status(&["symbolic-ref", "-q", "HEAD"])
        .map_err(|error| error.message())?;
    if code != 0 || on != branch {
        let on = if code == 0 {
            on
        } else {
            "a detached HEAD".to_owned()
        };
        return Ok(Some(format!(
            "the main checkout is on {on}, not main's branch {branch}: HEAD stays, and the \
             merge {commit} holds the saved edits"
        )));
    }
    let now = checkout
        .run(&["rev-parse", "--verify", "-q", "HEAD^{commit}"])
        .map_err(|error| error.message())?;
    if now != head {
        return Ok(Some(format!(
            "HEAD moved from {head} to {now} during the save: HEAD and the files stay"
        )));
    }
    if !repo.git.is_ancestor(head, commit)? {
        return Ok(Some(format!(
            "HEAD {head} is not an ancestor of the merge {commit}: HEAD and the files stay"
        )));
    }
    let entries = changes(checkout)?;
    let others: Vec<&str> = entries
        .iter()
        .filter(|entry| entry.tracked && !saved.contains(&entry.path))
        .map(|entry| entry.path.as_str())
        .collect();
    if !others.is_empty() {
        return Ok(Some(format!(
            "files changed during the save that it did not include: {others:?}; HEAD and the \
             files stay, and bay save again merges them"
        )));
    }
    if snapshot_tree(repo, checkout, head, saved)? != tree {
        return Ok(Some(
            "a saved file changed during the save: HEAD and the files stay, and bay save again \
             merges the change"
                .to_owned(),
        ));
    }
    for (status, path) in name_status(&repo.git, head, commit)? {
        if status != 'A' || saved.contains(&path) {
            continue;
        }
        if in_the_way(&repo.work_root, &path) {
            return Ok(Some(format!(
                "{path} is in the working tree, untracked, and the merge adds it: HEAD and \
                 the files stay"
            )));
        }
    }
    Ok(None)
}

/// Whether a file or folder of the working tree stands where `path` would be written: at the
/// path itself, or as a file at one of its parent folders.
fn in_the_way(root: &Path, path: &str) -> bool {
    let full = root.join(path);
    if std::fs::symlink_metadata(&full).is_ok() {
        return true;
    }
    full.ancestors()
        .skip(1)
        .take_while(|parent| *parent != root)
        .any(|parent| {
            std::fs::symlink_metadata(parent).is_ok_and(|meta| !meta.file_type().is_dir())
        })
}

/// Each path that differs between `from` and `to`, with its status letter (`A`, `M`, `D`, `T`).
fn name_status(git: &Git, from: &str, to: &str) -> Result<Vec<(char, String)>, String> {
    let out = git
        .run_bytes(
            &["diff", "--name-status", "--no-renames", "-z", from, to],
            &[],
            None,
        )
        .map_err(|error| error.message())?;
    let text =
        String::from_utf8(out).map_err(|_| "git diff names a path that is not UTF-8".to_owned())?;
    let mut fields = text.split('\0').filter(|field| !field.is_empty());
    let mut listed = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        listed.push((status.chars().next().unwrap_or('M'), path.to_owned()));
    }
    Ok(listed)
}

enum MoveError {
    /// Nothing moved.
    Unmoved(String),
    /// The branch moved, and the index or files did not all follow.
    Partly(String),
}

/// Fast-forwards the main checkout's branch from `head` to `commit` by compare-and-swap, then
/// its index (keeping each unchanged entry's file stamps) and the files that differ between the
/// two. Plumbing only: no hook runs, and nothing is staged or committed in the branch.
fn move_checkout(repo: &Repo, checkout: &Git, head: &str, commit: &str) -> Result<(), MoveError> {
    let listed = name_status(&repo.git, head, commit).map_err(MoveError::Unmoved)?;
    checkout
        .run(&[
            "update-ref",
            "-m",
            "bay save: fast-forward to the landing",
            "HEAD",
            commit,
            head,
        ])
        .map_err(|error| MoveError::Unmoved(error.message()))?;
    let partly = |why: String| {
        MoveError::Partly(format!(
            "{why}; HEAD is at the merge, so git status may list main's changes until the \
             files are checked out (git checkout HEAD -- <path>)"
        ))
    };
    // `--reset`, not `-m`: the saved files already hold the landing's content, so `-m` would
    // refuse them as not up to date. Every file this resets was checked above (the saved ones
    // match the landing, and no other tracked file changed), and the refresh below restores
    // their stat information.
    checkout
        .run(&["read-tree", "--reset", commit])
        .map_err(|error| partly(error.message()))?;
    let written: Vec<u8> = listed
        .iter()
        .filter(|(status, _)| *status != 'D')
        .flat_map(|(_, path)| path.bytes().chain(std::iter::once(0)))
        .collect();
    if !written.is_empty() {
        checkout
            .run_with(
                &["checkout-index", "-f", "-z", "--stdin"],
                &[],
                Some(&written),
            )
            .map_err(|error| partly(error.message()))?;
    }
    for (_, path) in listed.iter().filter(|(status, _)| *status == 'D') {
        let full = repo.work_root.join(path);
        match std::fs::remove_file(&full) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(partly(format!("{path} could not be removed: {error}"))),
        }
        // Folders the removal emptied go, as a checkout removes them; a folder with anything
        // left in it stays.
        for parent in full.ancestors().skip(1) {
            if parent == repo.work_root || std::fs::remove_dir(parent).is_err() {
                break;
            }
        }
    }
    let _ = checkout.run_status(&["update-index", "-q", "--refresh"]);
    Ok(())
}

fn tree_of(git: &Git, commit: &str) -> Result<String, String> {
    git.run(&["rev-parse", &format!("{commit}^{{tree}}")])
        .map_err(|error| error.message())
}
