//! A bay-hosted trunk (`DESIGN` § 10a): `bay host` and the host hook.
//!
//! The host is a bare repository on the reftable backend (atomic, crash-safe multi-ref
//! transactions). Its `refs/heads/<trunk>` is append-only, enforced by an `update` hook defined
//! in configuration (Git ≥ 2.54) that refuses a non-fast-forward, a deletion or a recreation of
//! trunk only. Below that floor Git ignores the hook, so `bay host` refuses with
//! `TOOL_BELOW_FLOOR` and creates nothing. `refs/bay/*` and `refs/notes/bay/*` stay rewritable so retention cuts work; the
//! repository-wide `receive.denyNonFastForwards` / `receive.denyDeletes` are not used.
//!
//! Ref-prefix scoping by key class and the publication checks are kickstart item 19c.

use std::path::Path;

use serde_json::json;

use crate::git::{Git, GitError};
use crate::result::{BayResult, Reason};
use crate::version::{GIT_FLOOR_HOSTED_TRUNK, Version};

/// The `update` hook a host made before G933 carries for its trunk rule:
/// `hook.bay-trunk.command` and `hook.bay-trunk.event`. A serving origin removes it
/// ([`drop_update_hook`]); `update` still judges the trunk rule while it stays.
pub const TRUNK_HOOK: &str = "bay-trunk";
/// The host's one hook, `pre-receive`: the trunk rule ([`judge_trunk`]) on every host, and
/// the scoped judgement (`hook.rs`) once the host has a key registry.
pub const SCOPE_HOOK: &str = "bay-scope";
/// The host's trunk branch name, read by the hook.
pub const TRUNK_KEY: &str = "bay.main";

/// The branch this host serves, whatever its name (`main`, `dev`, `master`; BACKLOG 55):
/// git config `bay.main`, else the branch the host's `HEAD` names, which `bay host` sets
/// to it, else `main`. A host made before `bay.main` (with `bay.trunk`) is still guarded on its
/// own branch: `HEAD` names it.
#[must_use]
pub fn served_branch(git: &Git) -> String {
    git.run(&["config", "--get", TRUNK_KEY])
        .ok()
        .or_else(|| git.run(&["symbolic-ref", "--short", "HEAD"]).ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "main".to_owned())
}

/// `bay host`, run in `dir`. With no `object_format` the host takes the object format of
/// the repository that holds `dir`, so that repository can push to it; outside one, Git's own
/// default (`init.defaultObjectFormat`, SHA-256 from Git 3.0). SHA-1 and SHA-256 repositories
/// cannot exchange objects, so a fixed default would refuse every push from the other kind.
/// With `upstream`, the host is a bay origin of that hosted remote ([`crate::origin`]).
#[must_use]
pub fn init(
    dir: &Path,
    path: &Path,
    trunk: &str,
    object_format: Option<&str>,
    upstream: Option<&str>,
) -> BayResult {
    run_init(dir, path, trunk, object_format, upstream).unwrap_or_else(BayResult::failed)
}

fn run_init(
    dir: &Path,
    path: &Path,
    trunk: &str,
    object_format: Option<&str>,
    upstream: Option<&str>,
) -> Result<BayResult, String> {
    // Git runs a hook defined in configuration only from 2.54. Below it the trunk hook would
    // be silently ignored and trunk would accept a force push, so refuse first.
    let git_line = Git::new(Path::new("."))
        .version()
        .map_err(|error| error.message())?;
    let git_version = Version::parse(&git_line);
    if git_version < Version::parse(GIT_FLOOR_HOSTED_TRUNK) {
        return Ok(
            BayResult::blocked(Reason::ToolBelowFloor).with_detail(json!({
                "tool": "git",
                "role": "bay-hosted-trunk",
                "version": git_version.to_string(),
                "floor": GIT_FLOOR_HOSTED_TRUNK,
                "recovery": "upgrade git",
            })),
        );
    }
    if path.exists()
        && std::fs::read_dir(path)
            .map_err(|error| error.to_string())?
            .next()
            .is_some()
    {
        return Err(format!("{} exists and is not empty", path.display()));
    }
    let bay =
        std::env::current_exe().map_err(|error| format!("cannot find the running bay: {error}"))?;
    let path_text = path.display().to_string();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let object_format = object_format.map(str::to_owned).or_else(|| {
        Git::new(dir)
            .run(&["rev-parse", "--show-object-format"])
            .ok()
    });
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let mut args = vec!["init", "-q", "--bare", "--ref-format=reftable"];
    let format_arg = object_format.map(|format| format!("--object-format={format}"));
    args.extend(format_arg.as_deref());
    args.extend(["-b", trunk, path_text.as_str()]);
    Git::new(parent)
        .run(&args)
        .map_err(|error| error.message())?;
    let host = Git::new(path);
    let object_format = host
        .run(&["rev-parse", "--show-object-format"])
        .map_err(|error| error.message())?;
    // G933: one `pre-receive` process a push judges the trunk rule too; an `update` hook ran a
    // process for each ref a push moves.
    let command = format!("'{}' host hook pre-receive", bay.display());
    for (key, value) in [
        (format!("hook.{SCOPE_HOOK}.command"), command.as_str()),
        (format!("hook.{SCOPE_HOOK}.event"), "pre-receive"),
        (TRUNK_KEY.to_owned(), trunk),
        ("core.logAllRefUpdates".to_owned(), "always"),
        ("receive.advertiseAtomic".to_owned(), "true"),
    ] {
        host.run(&["config", &key, value])
            .map_err(|error| error.message())?;
    }
    // G191: a commit-graph with generation numbers and changed-path Bloom filters. The host
    // is born empty, so Git's maintenance after a push writes the first graph and every one
    // after it; bay runs no maintenance of its own (G172).
    for (key, value) in crate::commit_graph::HOST_SETTINGS {
        host.run(&["config", key, value])
            .map_err(|error| error.message())?;
    }
    if let Some(url) = upstream {
        crate::origin::configure(&host, &bay, url)?;
    }
    Ok(BayResult::ok(json!({
        "host": path_text,
        "trunk": trunk,
        "ref format": "reftable",
        "object format": object_format,
        "hook": command,
        "commit_graph": "changed-path Bloom filters, written by Git's maintenance after a push",
        "upstream": upstream.map(crate::origin::shown),
        "mode": upstream.map(|_| crate::origin::Mode::Through.name()),
        // The origin takes the hosted remote's refs on this command's first pass.
        "serve": upstream.map(|_| format!("bay host serve {path_text}")),
    })))
}

/// Whether the host at `git` judges its trunk rule: in its `pre-receive` hook, or in the
/// `update` hook of a host made before G933.
#[must_use]
pub fn has_trunk_hook(git: &Git) -> bool {
    configured(git, SCOPE_HOOK, "pre-receive") || configured(git, TRUNK_HOOK, "update")
}

/// Whether `hook.<name>` runs bay's `host hook <event>` on `event`.
fn configured(git: &Git, name: &str, event: &str) -> bool {
    let events = git
        .run(&["config", "--get-all", &format!("hook.{name}.event")])
        .unwrap_or_default();
    let command = git
        .run(&["config", "--get", &format!("hook.{name}.command")])
        .unwrap_or_default();
    events.lines().any(|line| line == event) && command.contains(&format!("host hook {event}"))
}

/// G933: removes the `update` hook of a host made before G933 once its `pre-receive` hook
/// judges the trunk rule, so a push no longer runs a bay process for each ref it moves. Whether
/// it removed one.
///
/// # Errors
///
/// The git fault that left the hook in place.
pub fn drop_update_hook(git: &Git) -> Result<bool, String> {
    if !configured(git, TRUNK_HOOK, "update") || !configured(git, SCOPE_HOOK, "pre-receive") {
        return Ok(false);
    }
    git.run(&["config", "--remove-section", &format!("hook.{TRUNK_HOOK}")])
        .map_err(|error| error.message())?;
    Ok(true)
}

/// G933: the trunk rule ([`append_only`]) for the trunk ref of a push, judged in `pre-receive`.
///
/// # Errors
///
/// The refusal, or the git fault that left the push unjudged (unknown is never pass).
pub fn judge_trunk(git: &Git, input: &str) -> Result<(), String> {
    let trunk = format!("refs/heads/{}", served_branch(git));
    for line in input.lines() {
        let [old, new, name] = line.split_whitespace().collect::<Vec<_>>()[..] else {
            continue;
        };
        if name != trunk {
            continue;
        }
        let judged = append_only(git, name, old, new).map_err(|error| {
            format!("the host hook could not judge {name}: {}", error.message())
        })?;
        if let Some(why) = judged {
            return Err(why);
        }
    }
    Ok(())
}

use crate::hook::is_zero;

/// The `update` hook: `<ref> <old> <new>`. Returns the refusal, or `None` to accept.
///
/// # Errors
///
/// A git fault while judging (the caller refuses on it: unknown is never pass).
pub fn judge_update(
    git: &Git,
    refname: &str,
    old: &str,
    new: &str,
) -> Result<Option<String>, GitError> {
    let trunk = served_branch(git);
    if refname != format!("refs/heads/{trunk}") {
        return Ok(None);
    }
    append_only(git, refname, old, new)
}

/// The trunk rule for the trunk ref `refname`: never deleted, never recreated once it held
/// merged changes, and moved only by fast-forward.
fn append_only(git: &Git, refname: &str, old: &str, new: &str) -> Result<Option<String>, GitError> {
    if is_zero(new) {
        return Ok(Some(format!(
            "{refname} is append-only: deleting it is refused"
        )));
    }
    if is_zero(old) {
        let landed = git.run(&[
            "for-each-ref",
            "--count=1",
            "--format=%(refname)",
            crate::names::LANDED,
            crate::compact::INDEX_PREFIX,
        ])?;
        if !landed.is_empty() {
            return Ok(Some(format!(
                "{refname} is append-only: it held merged changes, so recreating it is refused"
            )));
        }
        return Ok(None);
    }
    let fast_forward = git.is_ancestor(old, new).map_err(|why| GitError::Failed {
        args: "merge-base --is-ancestor".to_owned(),
        stderr: why,
    })?;
    Ok((!fast_forward)
        .then(|| format!("{refname} is append-only: a non-fast-forward update is refused")))
}

/// How long the scoped hook may judge one push: `bay.hookTimeoutS` on the host, default 30 s.
pub const HOOK_TIMEOUT_KEY: &str = "bay.hookTimeoutS";

/// Judges a push within the host's time bound. A judgement that runs past it refuses the push
/// with `HOOK_TIMEOUT`; the publisher retries the same candidate (`INTERFACES` § 2).
fn pre_receive_bounded(git: &Git, dir: &Path, input: String) -> u8 {
    let bound = std::time::Duration::from_secs(
        git.run(&["config", "--get", HOOK_TIMEOUT_KEY])
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(30),
    );
    let slow = test_slow_push(git, dir, &input);
    let started = std::time::Instant::now();
    let (done, judged) = std::sync::mpsc::channel();
    let dir = dir.to_path_buf();
    std::thread::spawn(move || {
        if slow {
            std::thread::sleep(bound + std::time::Duration::from_secs(1));
        }
        let git = Git::new(&dir);
        // 0.7.0: an origin sends a push its checks pass to its hosted remote before it takes it.
        let judged = judge_trunk(&git, &input)
            .and_then(|()| crate::hook::pre_receive(&git, &input))
            .and_then(|()| {
                crate::origin::forward(&git, &dir, &input, bound.saturating_sub(started.elapsed()))
            });
        let _ = done.send(judged);
    });
    match judged.recv_timeout(bound) {
        Ok(Ok(())) => 0,
        Ok(Err(why)) => {
            eprintln!("bay: {why}");
            1
        }
        Err(_) => {
            eprintln!(
                "bay: HOOK_TIMEOUT: the hook on main exceeded its {}s bound",
                bound.as_secs()
            );
            1
        }
    }
}

/// Fault injection for G35: with `BAY_TEST_HOOK_SLOW_PUSHES=<n>`, the first `n` pushes that
/// update trunk are judged too slowly. The count is kept in the host's git directory.
fn test_slow_push(git: &Git, dir: &Path, input: &str) -> bool {
    let Some(limit) = std::env::var("BAY_TEST_HOOK_SLOW_PUSHES")
        .ok()
        .and_then(|text| text.parse::<u32>().ok())
    else {
        return false;
    };
    let trunk = served_branch(git);
    let trunk_ref = format!("refs/heads/{trunk}");
    if !input
        .lines()
        .any(|line| line.split_whitespace().nth(2) == Some(trunk_ref.as_str()))
    {
        return false;
    }
    let counter = dir.join("bay-test-slow-pushes");
    let seen: u32 = std::fs::read_to_string(&counter)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0);
    if seen >= limit {
        return false;
    }
    let _ = std::fs::write(&counter, (seen + 1).to_string());
    true
}

/// Runs a host hook event. Returns the process exit code: 0 accepts, 1 refuses.
#[must_use]
pub fn hook(event: &str, args: &[String]) -> u8 {
    crate::git::judging_a_push();
    let dir = std::env::var("GIT_DIR")
        .map_or_else(|_| std::path::PathBuf::from("."), std::path::PathBuf::from);
    let git = Git::new(&dir);
    if event == "pre-receive" {
        let mut input = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
        return pre_receive_bounded(&git, &dir, input);
    }
    if event == "post-receive" {
        let mut input = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
        crate::origin::post_receive(&dir, &input);
        return 0;
    }
    let ("update", [refname, old, new]) = (event, args) else {
        eprintln!("bay: unknown host hook call: {event} {}", args.join(" "));
        return 1;
    };
    match judge_update(&git, refname, old, new) {
        Ok(None) => 0,
        Ok(Some(why)) => {
            eprintln!("bay: {why}");
            1
        }
        Err(error) => {
            eprintln!(
                "bay: the host hook could not judge {refname}: {}",
                error.message()
            );
            1
        }
    }
}
