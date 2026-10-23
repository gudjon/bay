//! The check adapter contract (`INTERFACES` § 3, § 7; `DESIGN` § 7).
//!
//! A check runs on a clean materialization of exactly the candidate, never an agent's view.
//! The adapter paths come from the accepted base, materialized read-only outside the
//! candidate's tree, and the command runs from there with the candidate as its working
//! directory. The request goes on stdin; a `bay.check.result/1` comes back on stdout. An
//! exit code alone is never evidence: anything short of a valid, matching result is `unknown`.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;

use crate::clock::now_rfc3339;
use crate::evidence::{
    BASELINE, CheckId, EVIDENCE_SCHEMA, EvidenceRecord, Producer, Subject, Unit, Verdict,
};
use crate::git::{Git, GitError};
use crate::materialize::{Unmade, materialize};
use crate::policy::{CheckEntry, Policy};

/// What is being checked.
pub struct Candidate<'a> {
    pub commit: &'a str,
    pub tree: &'a str,
    /// Where the adapter paths are materialized from: the accepted base, or the candidate itself
    /// for its own new policy (§ 7).
    pub base: &'a str,
    pub base_tree: &'a str,
    /// The accepted base the candidate was merged onto, whose tree is `base_tree`: the
    /// request's `base`.
    pub onto: &'a str,
    /// The change the candidate lands, when it lands one: the request's `change` and
    /// `messages`. A batch's candidate, a trunk bisection's and the conformance kit's carry none.
    pub change: Option<&'a ChangeFacts>,
    /// The paths the candidate's tree changes against the trunk commit it was merged onto,
    /// sorted: the request's `changed_paths`.
    pub changed_paths: &'a [String],
    /// `<op id>.<attempt>`.
    pub id: &'a str,
    /// How the judging policy reached trunk (`INTERFACES` § 6).
    pub policy_provenance: crate::policy::Provenance,
    /// The version of each named input the candidate is checked under, `<kind>:<name>@<version>`
    /// (`INTERFACES` § 3): the request's `inputs`.
    pub inputs: &'a [String],
}

/// The change behind a candidate, as its check request names it (`INTERFACES` § 3).
#[derive(Clone, Debug)]
pub struct ChangeFacts {
    pub id: String,
    /// The intent's external reference and its revision.
    pub intent_reference: String,
    pub intent_revision: String,
    /// The actor's name.
    pub actor: String,
    /// The change's revision, its last draft: the change's own commits are the ones it
    /// reaches and the base does not.
    pub revision: String,
}

/// The most commits of a change the request's `messages` carries.
pub const MESSAGES_MAX: usize = 50;
/// The most bytes of commit messages the request's `messages` carries, all told. A message is
/// carried whole or not at all.
pub const MESSAGES_MAX_BYTES: usize = 64 * 1024;

/// The ref of the temporary history repository that names the request's base.
pub const HISTORY_BASE_REF: &str = crate::names::CHECK_BASE;
/// The ref of the temporary history repository that names the candidate.
pub const HISTORY_CANDIDATE_REF: &str = crate::names::CHECK_CANDIDATE;
/// The ref of the temporary history repository that names the change's revision.
pub const HISTORY_REVISION_REF: &str = crate::names::CHECK_REVISION;

/// The request's `messages`: the change's own commits, the ones `change.revision` reaches and
/// `onto` does not, oldest first, each `{"commit", "message"}`; and its `messages_truncated`,
/// whether they were cut to [`MESSAGES_MAX`] commits or [`MESSAGES_MAX_BYTES`]. When cut, the
/// newest are kept.
///
/// # Errors
///
/// Git cannot list the commits.
fn messages(
    git: &Git,
    onto: &str,
    change: &ChangeFacts,
) -> Result<(Vec<serde_json::Value>, bool), String> {
    let limit = format!("--max-count={}", MESSAGES_MAX + 1);
    let exclude = format!("^{onto}");
    let out = git
        .run_bytes(
            &[
                "log",
                "-z",
                "--format=%H%n%B",
                &limit,
                &change.revision,
                &exclude,
                "--",
            ],
            &[],
            None,
        )
        .map_err(|error| error.message())?;
    let text = String::from_utf8_lossy(&out);
    let listed: Vec<(&str, &str)> = text
        .split('\0')
        .filter(|record| !record.is_empty())
        .map(|record| record.split_once('\n').unwrap_or((record, "")))
        .collect();
    let mut truncated = listed.len() > MESSAGES_MAX;
    let (mut kept, mut bytes) = (Vec::new(), 0);
    // Newest first, as Git lists them: what is cut is the oldest.
    for (commit, message) in listed.into_iter().take(MESSAGES_MAX) {
        if bytes + message.len() > MESSAGES_MAX_BYTES {
            truncated = true;
            break;
        }
        bytes += message.len();
        kept.push(json!({"commit": commit, "message": message}));
    }
    kept.reverse();
    Ok((kept, truncated))
}

/// What a request says beyond the candidate, the same for each check of one run: `base`, and,
/// for a candidate that lands a change, `change` and `messages` (`INTERFACES` § 3).
///
/// # Errors
///
/// Git cannot list the change's commits.
fn about(
    git: &Git,
    candidate: &Candidate,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let mut about = serde_json::Map::new();
    about.insert(
        "base".to_owned(),
        json!({"commit": candidate.onto, "tree": candidate.base_tree}),
    );
    if let Some(change) = candidate.change {
        about.insert(
            "change".to_owned(),
            json!({
                "id": change.id,
                "intent": {"reference": change.intent_reference, "revision": change.intent_revision},
                "actor": change.actor,
                "revision": change.revision,
            }),
        );
        let (kept, truncated) = messages(git, candidate.onto, change)?;
        about.insert("messages".to_owned(), json!(kept));
        about.insert("messages_truncated".to_owned(), json!(truncated));
    }
    Ok(about)
}

/// The read-only history of one check run with `inputs = ["history"]` (`INTERFACES` § 3): a
/// fresh bare repository in the run's scratch, whose `objects/info/alternates` names the
/// repository's object directory, with refs at the base, the candidate and the change's
/// revision. The adapter reads every object through it; what it writes there stays in the
/// scratch and is removed with it. The object directory and its own alternates are bound
/// read-only in the sandbox (`sandbox-exec`, `bwrap`), so no write reaches the store; with no
/// sandbox (`bay.runnerSandbox = none`) read-only is not enforced, and the record says
/// `sandbox: "none"`.
pub(crate) struct History {
    /// The temporary bare repository: `GIT_DIR` for the adapter.
    pub dir: PathBuf,
    /// The repository's object directory.
    pub object_dir: PathBuf,
    /// The object directories the store itself borrows from, in the order its alternates name
    /// them (and theirs after them).
    pub alternates: Vec<PathBuf>,
    /// The repository holds [`HISTORY_REVISION_REF`]: the candidate lands a change.
    pub revision: bool,
}

impl History {
    /// Every directory the check may read and must never write.
    fn read_only(&self) -> Vec<PathBuf> {
        std::iter::once(self.object_dir.clone())
            .chain(self.alternates.iter().cloned())
            .collect()
    }

    /// The request's `git`.
    fn request(&self) -> serde_json::Value {
        let mut git = json!({
            "dir": self.dir.display().to_string(),
            "object_dir": self.object_dir.display().to_string(),
            "refs": {
                "base": HISTORY_BASE_REF,
                "candidate": HISTORY_CANDIDATE_REF,
            },
        });
        if self.revision {
            git["refs"]["revision"] = json!(HISTORY_REVISION_REF);
        }
        if !self.alternates.is_empty() {
            git["alternates"] = json!(
                self.alternates
                    .iter()
                    .map(|dir| dir.display().to_string())
                    .collect::<Vec<_>>()
            );
        }
        git
    }
}

/// The object directories `objects` borrows from: its `info/alternates`, each line a path
/// (relative ones from `objects`), and theirs after them, as Git follows them, five deep.
fn alternates_of(objects: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut next = vec![objects.to_path_buf()];
    for _ in 0..5 {
        let mut deeper = Vec::new();
        for dir in &next {
            let Ok(text) = std::fs::read_to_string(dir.join("info").join("alternates")) else {
                continue;
            };
            for line in text.lines().map(str::trim) {
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let path = real(&dir.join(line));
                if path != real(objects) && !found.contains(&path) {
                    found.push(path.clone());
                    deeper.push(path);
                }
            }
        }
        if deeper.is_empty() {
            break;
        }
        next = deeper;
    }
    found
}

/// Makes the [`History`] of a check run in `scratch`: `history.git`, a bare repository by hand
/// (no Git process), in the object format of the candidate's ids, borrowing every object from
/// `git`'s store.
///
/// # Errors
///
/// The store's location cannot be read, or the directory cannot be written.
fn make_history(git: &Git, scratch: &Path, candidate: &Candidate) -> Result<History, String> {
    let common = git.common_dir().map_err(|error| error.message())?;
    let object_dir = real(&Path::new(&common).join("objects"));
    let alternates = alternates_of(&object_dir);
    let dir = scratch.join("history.git");
    let write = |path: PathBuf, text: String| -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        std::fs::write(&path, text).map_err(|error| format!("{}: {error}", path.display()))
    };
    // A SHA-256 id is 64 hex digits; Git's repository format 1 names the object format.
    let config = if candidate.commit.len() == 64 {
        "[core]\n\trepositoryformatversion = 1\n\tbare = true\n[extensions]\n\tobjectformat = sha256\n"
    } else {
        "[core]\n\trepositoryformatversion = 0\n\tbare = true\n"
    };
    write(dir.join("config"), config.to_owned())?;
    write(
        dir.join("objects").join("info").join("alternates"),
        format!("{}\n", object_dir.display()),
    )?;
    std::fs::create_dir_all(dir.join("refs").join("heads"))
        .map_err(|error| format!("{}: {error}", dir.display()))?;
    write(dir.join("HEAD"), format!("{}\n", candidate.commit))?;
    let mut refs = vec![
        (HISTORY_BASE_REF, candidate.onto),
        (HISTORY_CANDIDATE_REF, candidate.commit),
    ];
    if let Some(change) = candidate.change {
        refs.push((HISTORY_REVISION_REF, change.revision.as_str()));
    }
    for (name, commit) in refs {
        write(dir.join(name), format!("{commit}\n"))?;
    }
    Ok(History {
        dir,
        object_dir,
        alternates,
        revision: candidate.change.is_some(),
    })
}

/// The paths `commit`'s tree adds, modifies or deletes against `base`'s, sorted. Renames are
/// a deletion and an addition; names are read NUL-separated, so Git never quotes them.
///
/// # Errors
///
/// Git cannot compare the two commits.
pub fn changed_paths(git: &Git, base: &str, commit: &str) -> Result<Vec<String>, String> {
    let out = git
        .run(&[
            "diff-tree",
            "-r",
            "-z",
            "--name-only",
            "--no-renames",
            base,
            commit,
        ])
        .map_err(|error| error.message())?;
    let mut paths: Vec<String> = out
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect();
    paths.sort_unstable();
    Ok(paths)
}

/// What bounds one run of the checks. No default: the sandbox is always chosen (ruling R14).
pub struct Limits {
    /// The pre-publication execution budget.
    pub budget: Option<Duration>,
    /// Checks that already finished on this tree under this manifest; not run again.
    pub finished: Vec<CheckId>,
    /// How often to call the heartbeat while a check runs.
    pub heartbeat: Option<Duration>,
    /// Directories the check may neither read nor write: the repository and every view in it.
    /// Its own scratch (the candidate and the adapter) is the only part of them it sees.
    pub hidden: Vec<PathBuf>,
    /// The sandbox the checks run in, and why none runs when there is none (ruling R14).
    pub sandbox: Sandbox,
    /// N-3: what a check with `baseline = "base"` is judged against. `None` judges every check
    /// whole-green, as a run with no base to compare with does (the conformance kit, bisection).
    pub baseline: Option<Baseline>,
    /// N-17: how many of the candidate's checks run at once (`[runner] parallel_checks`):
    /// 1 runs them one after another, as every run but a candidate's does.
    pub parallel: usize,
}

/// N-3: the base a check with `baseline = "base"` is judged against: the trunk commit the
/// candidate was merged onto (its tree is the candidate's `base_tree`), and the records on that
/// tree that count and answer the policy's checks (signed under the registry when there is one,
/// from a listed producer), read before the run.
pub struct Baseline {
    pub commit: String,
    pub records: Vec<EvidenceRecord>,
}

/// What an adapter, a check's or an input provider's, may neither read nor write (§ 7): the
/// repository with every view in it, and every key this machine holds, so code under test can
/// never read a producer key or write a signed record.
#[must_use]
pub fn hidden_for(repo: &crate::repo::Repo, keys: &crate::signing::Keys) -> Vec<PathBuf> {
    let mut hidden = vec![repo.work_root.clone(), repo.common_dir.clone()];
    // A linked worktree bay was started in is the repository's too (a view is already under
    // the common directory).
    if let Ok(top) = Git::new(&repo.opened_in).run(&["rev-parse", "--show-toplevel"]) {
        let top = PathBuf::from(top);
        if !top.starts_with(&repo.common_dir) && !hidden.contains(&top) {
            hidden.push(top);
        }
    }
    let keys = [
        keys.agent.clone(),
        keys.executor.as_ref().map(|signer| signer.key.clone()),
        keys.producer.as_ref().map(|signer| signer.key.clone()),
    ];
    hidden.extend(keys.into_iter().flatten());
    hidden
}

/// The sandbox a check ran in: `sandbox-exec` on macOS, `bwrap` on Linux, or none when this
/// machine has neither (the evidence record says so).
pub(crate) fn sandbox_kind() -> &'static str {
    if cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").exists() {
        "sandbox-exec"
    } else if cfg!(target_os = "linux")
        // The probe is the sandbox a check gets, so a `bwrap` that cannot unshare the network
        // or mount a private `/proc` is no sandbox here.
        && Command::new("bwrap")
            .args(sandbox::bwrap_system_args(false))
            .arg("true")
            .output()
            .is_ok_and(|out| out.status.success())
    {
        "bwrap"
    } else {
        "none"
    }
}

/// The remedy for a Linux box whose `bwrap` is installed but cannot build a sandbox because
/// `AppArmor` restricts unprivileged user namespaces (Ubuntu 24.04 and later): `bwrap` is where
/// it is installed, and `restricted` is the contents of
/// `kernel.apparmor_restrict_unprivileged_userns`.
#[must_use]
pub fn apparmor_remedy(bwrap: Option<&Path>, restricted: Option<&str>) -> Option<String> {
    let bwrap = bwrap?;
    restricted
        .is_some_and(|value| value.trim() == "1")
        .then(|| {
            let bwrap = bwrap.display();
            format!(
                "bwrap is installed, but AppArmor restricts unprivileged user namespaces \
             (kernel.apparmor_restrict_unprivileged_userns = 1): add an AppArmor profile for \
             {bwrap} that allows userns (abi <abi/4.0>, profile bwrap {bwrap} \
             flags=(unconfined) {{ userns, }}) in /etc/apparmor.d/bwrap and load it with \
             apparmor_parser -r; or choose to run checks without a sandbox: \
             git config --global {UNSANDBOXED_KEY} none"
            )
        })
}

/// Where this machine says whether `AppArmor` restricts unprivileged user namespaces; in a
/// test build `BAY_TEST_USERNS_RESTRICT` names a file to read instead.
const USERNS_RESTRICT: &str = "/proc/sys/kernel/apparmor_restrict_unprivileged_userns";
const TEST_HOOKS: bool = option_env!("BAY_RELEASE_VERSION").is_none();

/// This machine's [`apparmor_remedy`], read from `PATH` and `/proc`.
#[must_use]
pub fn apparmor_remedy_here() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let bwrap = Some(on_path("bwrap")).filter(|path| path.is_absolute())?;
    let file = std::env::var_os("BAY_TEST_USERNS_RESTRICT")
        .filter(|_| TEST_HOOKS)
        .map_or_else(|| PathBuf::from(USERNS_RESTRICT), PathBuf::from);
    let restricted = std::fs::read_to_string(file).ok();
    apparmor_remedy(Some(&bwrap), restricted.as_deref())
}

/// The box's choice to run checks with no sandbox (ruling R14): global Git config, never a
/// repository's, as the update pin is the box's. Its one value is `none`.
pub const UNSANDBOXED_KEY: &str = "bay.runnerSandbox";

/// Why a check does not run on a machine with no sandbox (ruling R14).
pub const NO_SANDBOX: &str = "this machine has no check sandbox, so the check did not run; \
     to run checks without one, the box chooses it with git config --global \
     bay.runnerSandbox none (each record then says sandbox \"none\")";

/// The sandbox checks run in on this machine, and, where there is none and the box has not
/// chosen `bay.runnerSandbox = none`, why no check runs (ruling R14): a check never runs
/// unconfined unless the box chose that.
#[derive(Clone, Debug)]
pub struct Sandbox {
    /// `sandbox-exec`, `bwrap` or `none`: what each record names.
    pub kind: &'static str,
    /// Set when no check may run: each one is then `infra_failure` with this reason.
    pub refusal: Option<String>,
    /// G285: the box's read-only paths for checks (`bay.checkReadPath`), absolute. Box facts:
    /// they never enter the definition digest, and each record of a sandboxed run lists them
    /// ([`SANDBOX_READ`]). Empty until [`Sandbox::reading`] reads them.
    pub read: Vec<PathBuf>,
}

impl Sandbox {
    /// This machine's sandbox. Its global Git config is read only when there is none.
    #[must_use]
    pub fn here() -> Self {
        let kind = sandbox_kind();
        if kind == "none" {
            Self::judge(kind, box_chose_unsandboxed())
        } else {
            Self::judge(kind, Ok(false))
        }
    }

    /// The ruling itself, on every platform: with a sandbox the check runs in it; with none it
    /// runs only when the box chose that (`chosen` is `Ok(true)`). An unreadable or unknown
    /// choice is no choice.
    #[must_use]
    pub fn judge(kind: &'static str, chosen: Result<bool, String>) -> Self {
        let refusal = if kind == "none" {
            match chosen {
                Ok(true) => None,
                Ok(false) => Some(NO_SANDBOX.to_owned()),
                Err(why) => Some(format!("{why}; {NO_SANDBOX}")),
            }
        } else {
            None
        };
        Self {
            kind,
            refusal,
            read: Vec::new(),
        }
    }

    /// G285: the sandbox with the box's read paths, as `git` reads `bay.checkReadPath`. A value
    /// that is not absolute once a leading `~/` is expanded refuses every check, naming it.
    #[must_use]
    pub fn reading(mut self, git: &Git) -> Self {
        match read_paths(git) {
            Ok(paths) => self.read = paths,
            Err(why) => {
                self.refusal.get_or_insert(why);
            }
        }
        self
    }
}

/// G285: the box's read-only paths for checks: Git config, multi-valued, global or for one
/// clone, read in the clone that runs the check, as [`ENVIRONMENT_KEY`] is. A repository never
/// declares one. Each value is an absolute path, or starts with `~/` for the box's `HOME`.
pub const READ_PATH_KEY: &str = "bay.checkReadPath";

/// G285: the record field, next to `sandbox`, that lists the box's read paths of a sandboxed
/// run. Kept in [`EvidenceRecord::unknown`], so a record without it reads as before.
pub const SANDBOX_READ: &str = "sandbox_read";

/// G285: `value` of [`READ_PATH_KEY`] as a path: a leading `~/` (or `~` alone) is `home`.
///
/// # Errors
///
/// The value is not absolute after that, or starts with `~` and there is no `HOME`.
pub fn read_path(value: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    let tilde = value == "~" || value.starts_with("~/");
    let path = match (tilde, home) {
        (true, Some(home)) => match value.trim_start_matches('~').trim_start_matches('/') {
            "" => home.to_path_buf(),
            rest => home.join(rest),
        },
        (true, None) => {
            return Err(format!(
                "{READ_PATH_KEY} = \"{value}\" starts with ~ and HOME is not set"
            ));
        }
        (false, _) => PathBuf::from(value),
    };
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(format!(
            "{READ_PATH_KEY} = \"{value}\" is not an absolute path; each value is absolute or \
             starts with ~/"
        ))
    }
}

/// G285: every value of [`READ_PATH_KEY`] as `git config --get-all` reads it in `git`'s clone,
/// each made a path by [`read_path`]. Unset is none.
///
/// # Errors
///
/// A value that is not absolute, or a `git config` that fails.
pub fn read_paths(git: &Git) -> Result<Vec<PathBuf>, String> {
    let home = crate::platform::home().map(PathBuf::from);
    match git.run_status(&["config", "--get-all", READ_PATH_KEY]) {
        // `git config --get-all` exits 1 for a key that is not set.
        Ok((1, _, _)) => Ok(Vec::new()),
        Ok((0, values, _)) => values
            .lines()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| read_path(value, home.as_deref()))
            .collect(),
        Ok((_, _, stderr)) => Err(format!("{READ_PATH_KEY} could not be read: {stderr}")),
        Err(error) => Err(format!(
            "{READ_PATH_KEY} could not be read: {}",
            error.message()
        )),
    }
}

/// G285: why a box read path is not bound: it lies inside a path a check never reads (the
/// repository, a view, a key), or holds one, so binding it would show what is hidden.
fn read_path_overlap(read: &[PathBuf], hidden: &[PathBuf]) -> Option<String> {
    read.iter().find_map(|path| {
        let path_real = real(path);
        hidden
            .iter()
            .find(|hidden| {
                let hidden = real(hidden);
                path_real.starts_with(&hidden) || hidden.starts_with(&path_real)
            })
            .map(|hidden| {
                format!(
                    "{READ_PATH_KEY} = \"{}\" overlaps {}, which a check never reads (the \
                     repository, a view or a key)",
                    path.display(),
                    hidden.display()
                )
            })
    })
}

/// Whether this box chose to run checks with no sandbox: `bay.runnerSandbox` in global Git
/// config is `none`. Unset is `false`; any other value, or a Git that does not answer, is an
/// error, so the caller refuses.
///
/// # Errors
///
/// A value other than `none`, or a `git config` that fails.
pub fn box_chose_unsandboxed() -> Result<bool, String> {
    let mut cmd = Command::new("git");
    cmd.args(["config", "--global", "--get", UNSANDBOXED_KEY]);
    let output = crate::git::run_bounded(cmd, "git config --global")
        .map_err(|error| format!("{UNSANDBOXED_KEY} could not be read: {}", error.message()))?;
    match output.status.code() {
        // `git config --get` exits 1 for a key that is not set.
        Some(1) => Ok(false),
        Some(0) => match String::from_utf8_lossy(&output.stdout).trim() {
            "none" => Ok(true),
            other => Err(format!(
                "{UNSANDBOXED_KEY} = \"{other}\" is not a choice bay knows; its one value is \
                 \"none\""
            )),
        },
        _ => Err(format!(
            "{UNSANDBOXED_KEY} could not be read: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// The box's declaration of the policy environments its executor serves (`DESIGN` § 7,
/// `INTERFACES` § 7): Git config, one value per environment, global or for one clone, read in
/// the clone that runs the operation. A repository never declares one.
pub const ENVIRONMENT_KEY: &str = "bay.checkEnvironment";

/// The environments this box serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Served {
    /// The box declares nothing: it serves every environment, as every box did before the
    /// declaration existed, so an upgrade stops no executor from running the checks it ran.
    Every,
    /// The box declares these environments and serves exactly them. Empty values are left
    /// out, so a box under maintenance withdraws every environment with one empty value; a
    /// declaration Git cannot read serves nothing.
    Declared(Vec<String>),
}

impl Served {
    /// `bay.checkEnvironment` as `git config --get-all` reads it in `git`'s clone.
    #[must_use]
    pub fn read(git: &Git) -> Self {
        match git.run_status(&["config", "--get-all", ENVIRONMENT_KEY]) {
            // `git config --get-all` exits 1 for a key that is not set.
            Ok((1, _, _)) => Self::Every,
            Ok((0, values, _)) => Self::Declared(
                values
                    .lines()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .collect(),
            ),
            _ => Self::Declared(Vec::new()),
        }
    }

    /// Whether the box serves `environment`, by its exact name.
    #[must_use]
    pub fn serves(&self, environment: &str) -> bool {
        match self {
            Self::Every => true,
            Self::Declared(declared) => declared.iter().any(|value| value == environment),
        }
    }

    /// `declared`, or `default` when the box declares nothing.
    #[must_use]
    pub const fn source(&self) -> &'static str {
        match self {
            Self::Every => "default",
            Self::Declared(_) => "declared",
        }
    }

    /// The declared values; none for a box that declares nothing.
    #[must_use]
    pub fn declared(&self) -> &[String] {
        match self {
            Self::Every => &[],
            Self::Declared(declared) => declared,
        }
    }
}

/// The environments of `policy`'s checks required of a candidate that changes `changed`, outside
/// `finished`, that `served` does not serve: sorted, each once. A pair with evidence already (a
/// resume, or reuse) needs no worker.
#[must_use]
pub fn unserved(
    policy: &Policy,
    changed: &[String],
    finished: &[CheckId],
    served: &Served,
) -> Vec<String> {
    let mut missing: Vec<String> = policy
        .checks
        .iter()
        // G245: an approval runs on no worker; a person's record answers it.
        .filter(|entry| policy.runs(entry, changed))
        .filter(|entry| !finished.contains(&CheckId::new(&entry.id, &entry.environment)))
        .filter(|entry| !served.serves(&entry.environment))
        .map(|entry| entry.environment.clone())
        .collect();
    missing.sort();
    missing.dedup();
    missing
}

/// `program` as found on this process's `PATH`, or the bare name when it is not there.
fn on_path(program: &str) -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(program))
                .find(|candidate| candidate.is_file())
        })
        .unwrap_or_else(|| PathBuf::from(program))
}

fn real(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The command that runs `executable` inside the sandbox, an allow-list (G62): it sees its
/// `scratch` (the candidate and the adapter) read-write, the directories of `read_only` (the
/// object store a history check reads, G267) and the system files of
/// [`sandbox::SYSTEM_DIRS`] and [`sandbox::SYSTEM_ETC`] read-only, and nothing else; any of
/// `hidden` inside the system files is hidden on top. The network is denied unless `network`
/// (a check's `network = true`, judged at the base). With no sandbox, a `#!` script starts as
/// [`crate::platform::script`] says (on Windows through Git for Windows' `sh.exe`), and
/// nothing is read-only.
///
/// # Errors
///
/// With no sandbox on Windows: a `#!` script bay cannot start ([`crate::platform::script`]).
pub(crate) fn sandboxed(
    kind: &str,
    executable: &Path,
    args: &[&str],
    hidden: &[PathBuf],
    (scratch, read_only): (&Path, &[PathBuf]),
    network: bool,
) -> Result<Command, String> {
    Ok(match kind {
        "sandbox-exec" => {
            let mut cmd = Command::new("/usr/bin/sandbox-exec");
            cmd.arg("-p")
                .arg(sandbox::macos_profile(
                    hidden,
                    (scratch, read_only),
                    network,
                ))
                .arg(executable)
                .args(args);
            cmd
        }
        "bwrap" => {
            // Found on the executor's own `PATH`: the check's cleared environment may not have it.
            let mut cmd = Command::new(on_path("bwrap"));
            cmd.args(sandbox::bwrap_args(hidden, (scratch, read_only), network));
            cmd.arg("--").arg(executable).args(args);
            cmd
        }
        _ => {
            let mut cmd = crate::platform::script(executable)?;
            cmd.args(args);
            cmd
        }
    })
}

#[derive(Debug, Deserialize)]
struct CheckResult {
    schema: String,
    exercised: Exercised,
    plan: Plan,
    #[serde(default)]
    checks: Vec<ResultUnit>,
}

#[derive(Debug, Deserialize)]
struct Exercised {
    commit: String,
    tree: String,
}

#[derive(Debug, Deserialize)]
struct Plan {
    #[serde(default)]
    selected: u64,
    completed: u64,
    /// `declared_empty` when the adapter's map selects nothing for this candidate (§ 7).
    #[serde(default)]
    selection: Option<String>,
}

/// The one recorded value of `plan.selection` (`INTERFACES` § 7).
pub const DECLARED_EMPTY: &str = "declared_empty";

/// What one check's run returns: its verdict, why it is not `pass`, its units, and its
/// `selection` when the selection was declared empty; or, set in `unavailable`, the adapter's
/// answer that no worker could run it, which observed nothing and is no verdict.
struct Judged {
    verdict: Verdict,
    why: Option<String>,
    units: Vec<Unit>,
    selection: Option<String>,
    unavailable: Option<String>,
    /// N-3: on a `product_failure`, how the rest of the result is judged were every failing
    /// unit pre-existing: what a check with `baseline = "base"` records when the base failed
    /// each of them too.
    excused: Option<Excused>,
}

/// N-3: the judgement of a failed result past its failing units: its other units, its plan
/// and its selection, judged as [`judge`] judges a result with no failing unit.
struct Excused {
    verdict: Verdict,
    why: Option<String>,
    selection: Option<String>,
}

impl Judged {
    const fn without_units(verdict: Verdict, why: Option<String>) -> Self {
        Self {
            verdict,
            why,
            units: Vec::new(),
            selection: None,
            unavailable: None,
            excused: None,
        }
    }
}

/// What a run of the checks returns: the record of each check it ran, and, when the adapter
/// answered that no worker could run a check, that check's environment and the adapter's
/// reason. The run stops there and writes no record for that check (`INTERFACES` § 3).
pub struct Ran {
    pub records: Vec<EvidenceRecord>,
    pub unavailable: Option<(String, String)>,
}

/// The adapter's answer that it could not reach a worker for the check (`INTERFACES` § 3):
/// `{"schema": "bay.check.result/1", "unavailable": "<reason>"}` with no `checks`. Any other
/// result without `checks` is no such answer, and is judged as a result.
fn unavailable_answer(output: &[u8]) -> Option<String> {
    let answer: serde_json::Value = serde_json::from_slice(output).ok()?;
    if answer["schema"] != "bay.check.result/1" || answer.get("checks").is_some() {
        return None;
    }
    answer["unavailable"]
        .as_str()
        .filter(|reason| !reason.is_empty())
        .map(str::to_owned)
}

#[derive(Debug, Deserialize)]
struct ResultUnit {
    id: String,
    verdict: Verdict,
    #[serde(default)]
    seconds: Option<f64>,
    #[serde(default)]
    reason: Option<String>,
}

/// The most checks of one candidate `[runner] parallel_checks` may run at once.
pub const MAX_PARALLEL_CHECKS: usize = 16;

/// How often the main thread of a parallel run looks for a finished check and renews the lease.
const PARALLEL_POLL: Duration = Duration::from_millis(50);

/// Runs every required check of `policy` on `candidate` that is not in `finished`, hands each
/// record to `keep` as soon as its check ends (so a finished unit survives a worker that dies
/// in a later one, § 7), and returns the records of this run. A check the adapter answers
/// `unavailable` for gets no record, and the run stops there: no worker answers for it.
///
/// N-17: with `limits.parallel` above 1, up to that many checks run at once, each on its own
/// thread in its own `check-{index}` directory ([`run_parallel`]). What is observed stays what
/// a serial run observes: records reach `keep` on this thread, one at a time, in the policy's
/// check order, each as soon as every check before it is kept; the budget runs from the first
/// check's start for all of them; a check's reruns stay on its own thread; and an `unavailable`
/// answer stops the run at the same check, with no record kept for any check after it. A check
/// judged against the base (N-3 `baseline = "base"`) shares this run's base records, so it
/// never runs on a thread: it runs here, serially, after the checks before it are kept.
///
/// # Errors
///
/// The candidate or the adapter cannot be materialized (a fault of bay's own, not a verdict),
/// or `keep` fails.
pub fn run_all(
    git: &Git,
    work_dir: &Path,
    policy: &Policy,
    candidate: &Candidate,
    producer: &str,
    limits: &Limits,
    hooks: &mut Hooks,
) -> Result<Ran, String> {
    let finished = limits.finished.as_slice();
    // The execution budget runs on the monotonic clock from the start of the checks.
    let started = Instant::now();
    let mut records = Vec::new();
    // N-3: the base runs this run made, so a rerun of a check compares with the same record.
    let mut on_base: Vec<EvidenceRecord> = Vec::new();
    // A quarantined check is not required (ruling 8c), a post-publication one runs after
    // publication (G91), one whose paths the candidate does not change is not required of it
    // (G146), and an approval is answered by a person's record, never run (G245): none of them
    // runs here.
    let required: Vec<(usize, &CheckEntry)> = policy
        .checks
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            !finished.contains(&CheckId::new(&entry.id, &entry.environment))
                && policy.runs(entry, candidate.changed_paths)
        })
        .collect();
    if required.is_empty() {
        return Ok(Ran {
            records,
            unavailable: None,
        });
    }
    // The request's `base`, `change` and `messages`, read once for every check of the run, and
    // only read by them.
    let told = about(git, candidate)?;
    let shared = Shared {
        git,
        policy,
        candidate,
        producer,
        limits,
        work_dir,
        started,
        about: &told,
        shows: hooks.seen.is_some(),
    };
    // What the parallel phase ran, by position in `required`, and how many of them are kept.
    let mut slots: Vec<Option<Result<Slot, String>>> = required.iter().map(|_| None).collect();
    let mut kept = 0;
    let width = limits.parallel.clamp(1, MAX_PARALLEL_CHECKS);
    if width > 1
        && let Some(unavailable) = run_parallel(
            shared,
            width,
            &required,
            hooks,
            (slots.as_mut_slice(), &mut kept, &mut records),
        )?
    {
        return Ok(Ran {
            records,
            unavailable: Some(unavailable),
        });
    }
    // The checks left, in order: what the parallel phase ran and is not yet kept, and each check
    // it did not run (every check of a serial run, a check judged against the base), run here.
    for (position, &(index, entry)) in required.iter().enumerate().skip(kept) {
        let unavailable = match slots.get_mut(position).and_then(Option::take) {
            Some(slot) => keep_slot(shared, (index, entry), slot?, hooks, &mut records)?,
            None => run_serial(shared, (index, entry), hooks, (&mut records, &mut on_base))?,
        };
        if let Some(reason) = unavailable {
            return Ok(Ran {
                records,
                unavailable: Some((entry.environment.clone(), reason)),
            });
        }
    }
    Ok(Ran {
        records,
        unavailable: None,
    })
}

/// What every check of one run reads and none writes, shared read-only by the checks a
/// parallel run runs at once.
#[derive(Clone, Copy)]
struct Shared<'a> {
    git: &'a Git,
    policy: &'a Policy,
    candidate: &'a Candidate<'a>,
    producer: &'a str,
    limits: &'a Limits,
    work_dir: &'a Path,
    /// When the first check started: the execution budget runs from here for every check.
    started: Instant,
    /// The request's `base`, `change` and `messages` ([`about`]), computed once.
    about: &'a serde_json::Map<String, serde_json::Value>,
    /// The run's `hooks.seen` is set: a check on a thread keeps its exchanges to show.
    shows: bool,
}

/// What one check run on a thread returns ([`run_in_slot`]): the record of each of its runs, in
/// order, unsigned and not yet kept; the adapter's `unavailable` answer that ended it, if one
/// did; and its exchanges with the adapter, to show on the main thread.
struct Slot {
    runs: Vec<EvidenceRecord>,
    unavailable: Option<String>,
    exchanges: Vec<Exchange>,
}

/// N-17: the check `entry`, the `index`th of `policy`, with its reruns, as one thread runs it:
/// the body of a parallel run. It calls no hook of the run: the main thread renews the lease
/// while it runs, and keeps its records and shows its exchanges once every check before it is
/// kept. A check judged against the base never runs here ([`run_parallel`]).
///
/// # Errors
///
/// As [`run_once`].
fn run_in_slot(shared: Shared, (index, entry): (usize, &CheckEntry)) -> Result<Slot, String> {
    let mut exchanges = Vec::new();
    let mut keep = |_: &mut EvidenceRecord| -> Result<(), String> { Ok(()) };
    let mut beat = || {};
    let mut look = |exchange: &Exchange| exchanges.push(exchange.clone());
    let look: &mut dyn FnMut(&Exchange) = &mut look;
    let mut hooks = Hooks {
        keep: &mut keep,
        beat: &mut beat,
        seen: if shared.shows { Some(look) } else { None },
    };
    let (mut runs, mut unavailable) = (Vec::new(), None);
    // Flake-aware retry, as [`run_serial`]: every rerun stays in this check's own slot.
    for run in 0..=entry.retry_on_failure {
        let (mut judged, ran) = run_once(
            shared.git,
            shared.policy,
            shared.candidate,
            shared.limits,
            &mut hooks,
            (index, entry),
            (shared.work_dir, shared.started, shared.about),
        )?;
        if let Some(reason) = judged.unavailable.take() {
            unavailable = Some(reason);
            break;
        }
        let record = evidence(
            shared.candidate,
            shared.producer,
            shared.policy,
            (index, entry, run),
            judged,
            ran,
            &shared.limits.sandbox,
        );
        let verdict = record.verdict;
        runs.push(record);
        if verdict != Verdict::ProductFailure {
            break;
        }
    }
    Ok(Slot {
        runs,
        unavailable,
        exchanges,
    })
}

/// N-17: runs the checks of `required` that are not judged against the base, up to `width` at
/// once, each on a scoped thread ([`run_in_slot`]), started in order. This thread alone calls
/// `hooks`: it renews the lease every heartbeat while checks run, and keeps each finished
/// check's records in order as soon as every check before it is kept, advancing `kept`; what
/// it cannot keep yet stays in `slots` for [`run_all`]. A check that ends `unavailable` or in
/// a fault of bay's own starts no further check; those already started finish. Returns the
/// environment and reason of an `unavailable` answer once every check before it is kept.
///
/// N-3's base comparison is not run here: a check with `baseline = "base"` (when the run has a
/// base) reads and adds to the run's records on the base and keeps the base's own record, so
/// it runs serially in [`run_all`] after the checks before it are kept.
///
/// # Errors
///
/// A check's fault of bay's own, once every check before it is kept, or `keep` fails.
#[allow(clippy::type_complexity)]
fn run_parallel(
    shared: Shared,
    width: usize,
    required: &[(usize, &CheckEntry)],
    hooks: &mut Hooks,
    (slots, kept, records): (
        &mut [Option<Result<Slot, String>>],
        &mut usize,
        &mut Vec<EvidenceRecord>,
    ),
) -> Result<Option<(String, String)>, String> {
    let compares =
        |entry: &CheckEntry| shared.limits.baseline.is_some() && entry.baseline.is_some();
    std::thread::scope(|scope| -> Result<Option<(String, String)>, String> {
        let (send, receive) = std::sync::mpsc::channel();
        let mut queue = required
            .iter()
            .enumerate()
            .filter(|&(_, &(_, entry))| !compares(entry));
        let (mut active, mut stop) = (0_usize, false);
        let mut last_beat = Instant::now();
        loop {
            while !stop && active < width {
                let Some((position, &(index, entry))) = queue.next() else {
                    break;
                };
                let send = send.clone();
                scope.spawn(move || {
                    let slot = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run_in_slot(shared, (index, entry))
                    }))
                    .unwrap_or_else(|_| Err(format!("the run of the check {} failed", entry.id)));
                    let _ = send.send((position, slot));
                });
                active += 1;
            }
            if active == 0 {
                break;
            }
            if let Ok((position, slot)) = receive.recv_timeout(PARALLEL_POLL) {
                active -= 1;
                stop |= !slot
                    .as_ref()
                    .is_ok_and(|slot: &Slot| slot.unavailable.is_none());
                if let Some(place) = slots.get_mut(position) {
                    *place = Some(slot);
                }
            }
            if shared
                .limits
                .heartbeat
                .is_some_and(|every| last_beat.elapsed() >= every)
            {
                (hooks.beat)();
                last_beat = Instant::now();
            }
            // Keep, in order, every finished check whose predecessors are all kept.
            while let Some(slot) = slots.get_mut(*kept).and_then(Option::take) {
                let Some(&(index, entry)) = required.get(*kept) else {
                    break;
                };
                *kept += 1;
                if let Some(reason) = keep_slot(shared, (index, entry), slot?, hooks, records)? {
                    return Ok(Some((entry.environment.clone(), reason)));
                }
            }
        }
        Ok(None)
    })
}

/// Shows and keeps what the check `entry`, the `index`th, ran on a thread, in order, and
/// returns the `unavailable` answer that ended it, if one did.
///
/// # Errors
///
/// `keep` fails.
fn keep_slot(
    shared: Shared,
    (index, entry): (usize, &CheckEntry),
    slot: Slot,
    hooks: &mut Hooks,
    records: &mut Vec<EvidenceRecord>,
) -> Result<Option<String>, String> {
    if let Some(seen) = hooks.seen.as_deref_mut() {
        for exchange in &slot.exchanges {
            seen(exchange);
        }
    }
    for record in slot.runs {
        keep_record(shared, (index, entry), record, hooks, records)?;
    }
    Ok(slot.unavailable)
}

/// Stamps `record`, of the check `entry`, the `index`th, with its footprint, hands it to
/// `hooks.keep` and adds it to `records`.
///
/// # Errors
///
/// `keep` fails.
fn keep_record(
    shared: Shared,
    (index, entry): (usize, &CheckEntry),
    mut record: EvidenceRecord,
    hooks: &mut Hooks,
    records: &mut Vec<EvidenceRecord>,
) -> Result<(), String> {
    // NEW-10: a check with `footprint = true` stamps its read set, so a later candidate whose
    // read set is the same can carry this record (`crate::throughput`).
    if let Some(stamp) = crate::throughput::stamp(
        shared.git,
        entry,
        shared.candidate.tree,
        shared.candidate.base_tree,
    ) {
        record
            .unknown
            .insert(crate::throughput::FOOTPRINT.to_owned(), stamp);
    }
    (hooks.keep)(&mut record)?;
    records.push(record);
    if std::env::var("BAY_TEST_KILL_AT").is_ok_and(|at| at == format!("after-check-{}", index + 1))
    {
        std::process::exit(137);
    }
    Ok(())
}

/// The check `entry`, the `index`th of `policy`, with its reruns, run and kept on this thread:
/// every check of a serial run, and a check judged against the base in a parallel one. Returns
/// the adapter's `unavailable` answer that ended it, if one did.
///
/// # Errors
///
/// As [`run_once`] and [`base_answer`], or `keep` fails.
fn run_serial(
    shared: Shared,
    (index, entry): (usize, &CheckEntry),
    hooks: &mut Hooks,
    (records, on_base): (&mut Vec<EvidenceRecord>, &mut Vec<EvidenceRecord>),
) -> Result<Option<String>, String> {
    let Shared {
        git,
        policy,
        candidate,
        producer,
        limits,
        work_dir,
        started,
        about: told,
        ..
    } = shared;
    // Flake-aware retry: a `product_failure` runs again on the identical tree, freshly
    // materialized, up to `retry_on_failure` times, and stops at the first other answer.
    // Every run is its own record, so a failure and a later pass combine to `flaky`.
    for run in 0..=entry.retry_on_failure {
        let (mut judged, ran) = run_once(
            git,
            policy,
            candidate,
            limits,
            hooks,
            (index, entry),
            (work_dir, started, told),
        )?;
        if let Some(reason) = judged.unavailable.take() {
            return Ok(Some(reason));
        }
        // N-3: a check with `baseline = "base"` that failed is judged against the base: only
        // a unit the base did not fail is a failure. A run that did not fail needs no base.
        let mut compared = None;
        if let Some(base) = limits
            .baseline
            .as_ref()
            .filter(|_| entry.baseline.is_some() && judged.verdict == Verdict::ProductFailure)
        {
            let answer = base_answer(
                git,
                policy,
                candidate,
                producer,
                limits,
                hooks,
                (index, entry),
                (work_dir, started),
                (base, &mut *on_base),
            )?;
            compared = Some(against_base(&mut judged, answer, &base.commit));
        }
        let mut record = evidence(
            candidate,
            producer,
            policy,
            (index, entry, run),
            judged,
            ran,
            &limits.sandbox,
        );
        // N-3: the record names the base record it was judged against, and each unit that
        // failed on both, so its verdict is auditable.
        if let Some(compared) = compared {
            record.unknown.insert(BASELINE.to_owned(), compared);
        }
        let verdict = record.verdict;
        keep_record(shared, (index, entry), record, hooks, records)?;
        if verdict != Verdict::ProductFailure {
            break;
        }
    }
    Ok(None)
}

/// One run of the check `entry`, the `index`th of `policy`, on `candidate`: its tree and adapter
/// freshly materialized under `work_dir` and removed after, within what is left of the budget
/// since `started`, with `about` in its request. Returns the judgement and when the check
/// itself started, after its materialization.
///
/// # Errors
///
/// The candidate or the adapter cannot be materialized, or the history of a check with
/// `inputs = ["history"]` cannot be made (a fault of bay's own, not a verdict).
fn run_once(
    git: &Git,
    policy: &Policy,
    candidate: &Candidate,
    limits: &Limits,
    hooks: &mut Hooks,
    (index, entry): (usize, &CheckEntry),
    (work_dir, started, about): (&Path, Instant, &serde_json::Map<String, serde_json::Value>),
) -> Result<(Judged, Instant), String> {
    let budget = limits.budget;
    let dir = work_dir.join(format!("check-{index}"));
    let _ = std::fs::remove_dir_all(&dir);
    let tree_dir = dir.join("candidate");
    let adapter_dir = dir.join("adapter");
    // R14: with no sandbox and no choice by the box, nothing of the check runs. G285: nor
    // when a box read path would show the repository or a key.
    let refusal = limits.sandbox.refusal.clone().or_else(|| {
        read_path_overlap(&limits.sandbox.read, &limits.hidden)
            .filter(|_| limits.sandbox.kind != "none")
    });
    let not_run = match refusal {
        Some(why) => Some(Judged::without_units(Verdict::InfraFailure, Some(why))),
        None => prepare(
            git,
            entry,
            candidate,
            (&tree_dir, &adapter_dir),
            budget.map(|budget| budget.saturating_sub(started.elapsed())),
            (&mut *hooks.beat, limits.heartbeat),
        )?,
    };
    // G267: a check that reads history gets it through a repository of its own run, which
    // borrows the store's objects and is removed with the scratch.
    let history = if not_run.is_none() && entry.reads_history() {
        Some(make_history(git, &dir, candidate)?)
    } else {
        None
    };
    let place = Place {
        tree: &tree_dir,
        adapter: &adapter_dir,
        scratch: &dir,
        hidden: &limits.hidden,
        sandbox: limits.sandbox.kind,
        read: &limits.sandbox.read,
        about,
        history: history.as_ref(),
    };
    let remaining = budget.map(|budget| budget.saturating_sub(started.elapsed()));
    let ran = Instant::now();
    let judged = match not_run {
        Some(judged) => judged,
        None => within_budget(
            hooks,
            limits.heartbeat,
            entry,
            policy,
            candidate,
            &place,
            remaining,
        ),
    };
    make_writable(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    Ok((judged, ran))
}

/// The evidence record of run `run` of the check `entry`, the `index`th of `policy`, judged
/// `judged` on `candidate` by `producer` in a run that started at `ran` under `sandbox`. A
/// sandboxed run lists the box's read paths in [`SANDBOX_READ`] (G285); with no sandbox
/// nothing is read-only, so it lists none.
fn evidence(
    candidate: &Candidate,
    producer: &str,
    policy: &Policy,
    (index, entry, run): (usize, &CheckEntry, u32),
    judged: Judged,
    ran: Instant,
    sandbox: &Sandbox,
) -> EvidenceRecord {
    let mut unknown = serde_json::Map::new();
    if sandbox.kind != "none" && !sandbox.read.is_empty() {
        unknown.insert(
            SANDBOX_READ.to_owned(),
            json!(
                sandbox
                    .read
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
            ),
        );
    }
    EvidenceRecord {
        schema: EVIDENCE_SCHEMA.to_owned(),
        id: run_id(producer, candidate.id, index, run),
        kind: "check".to_owned(),
        subject: Subject {
            tree: candidate.tree.to_owned(),
            commit: candidate.commit.to_owned(),
            base_tree: candidate.base_tree.to_owned(),
            unknown: serde_json::Map::new(),
        },
        inputs: EvidenceRecord::inputs_for(&entry.fixtures),
        check: CheckId::new(&entry.id, &entry.environment),
        policy_revision: policy.revision,
        manifest_digest: policy.manifest_digest.clone(),
        policy_provenance: Some(candidate.policy_provenance.as_str().to_owned()),
        producer: Producer {
            name: producer.to_owned(),
            version: crate::version::BAY.to_owned(),
            unknown: serde_json::Map::new(),
        },
        verdict: judged.verdict,
        at: now_rfc3339(),
        why: judged.why,
        units: judged.units,
        selection: judged.selection,
        sandbox: Some(sandbox.kind.to_owned()),
        seconds: Some((ran.elapsed().as_secs_f64() * 1000.0).round() / 1000.0),
        signature: None,
        unknown,
    }
}

/// The evidence id of run `run` of the check at `index` of the policy on `candidate`: the first
/// run keeps the id it always had, and each rerun after a `product_failure` gets its own, so no
/// two runs share an id (G29) and every run stays on the tree.
fn run_id(producer: &str, candidate: &str, index: usize, run: u32) -> String {
    if run == 0 {
        format!("{producer}/{candidate}-{}", index + 1)
    } else {
        format!("{producer}/{candidate}-{}-rerun-{run}", index + 1)
    }
}

/// N-3: whether `record` answers check `entry` on the base under the exact inputs the
/// candidate's run had: a `check` record of it about the base's tree (and commit, for a
/// commit-keyed check), made under the same manifest, so the same definition and adapter,
/// under the same provenance and fixtures, with an answer. An `unknown` or `infra_failure`
/// record is no answer, so the base is run again rather than leaving every later candidate on
/// it unknown.
fn answers_on_base(
    record: &EvidenceRecord,
    policy: &Policy,
    candidate: &Candidate,
    entry: &CheckEntry,
    base: &str,
) -> bool {
    // N-18: a changed-only check's base record is the base's own, on its own first parent.
    let commit_keyed = entry.commit_keyed();
    record.kind == crate::evidence::KIND_CHECK
        && record.check == CheckId::new(&entry.id, &entry.environment)
        && record.subject.tree == candidate.base_tree
        && (!commit_keyed || record.subject.commit == base)
        && record.manifest_digest == policy.manifest_digest
        && record.policy_provenance.as_deref() == Some(candidate.policy_provenance.as_str())
        && record.fixtures().as_deref() == Some(entry.fixtures.as_slice())
        && matches!(record.verdict, Verdict::Pass | Verdict::ProductFailure)
}

/// N-3: the record of check `entry` on the base that the candidate's failed run is judged
/// against: the base's own record when one answers it under exact inputs
/// ([`answers_on_base`]), else one run of the check on the base, materialized like a
/// candidate, with the adapter the candidate's run had, and handed to `hooks.keep` as the
/// base's evidence, so a later candidate on the same base reuses it: one run per base, never
/// one per candidate. The inner error says why the base has no answer at all (no worker ran
/// it).
///
/// # Errors
///
/// A git fault, the base cannot be materialized, or `keep` fails.
#[allow(clippy::too_many_arguments)]
fn base_answer(
    git: &Git,
    policy: &Policy,
    candidate: &Candidate,
    producer: &str,
    limits: &Limits,
    hooks: &mut Hooks,
    (index, entry): (usize, &CheckEntry),
    (work_dir, started): (&Path, Instant),
    (base, on_base): (&Baseline, &mut Vec<EvidenceRecord>),
) -> Result<Result<EvidenceRecord, String>, String> {
    if let Some(found) = on_base
        .iter()
        .chain(&base.records)
        .find(|record| answers_on_base(record, policy, candidate, entry, &base.commit))
    {
        return Ok(Ok(found.clone()));
    }
    // The base is checked as it was landed: against its own first parent, so an adapter that
    // selects by `changed_paths` selects what it did then. A root commit changes every path.
    let parent_commit =
        match git.run(&["rev-parse", "--verify", "-q", &format!("{}^1", base.commit)]) {
            Ok(parent) => Some(parent.trim().to_owned()),
            // `rev-parse --verify` fails on a root commit, which has no first parent.
            Err(GitError::Failed { .. }) => None,
            Err(error) => return Err(error.message()),
        };
    let parent = match &parent_commit {
        Some(commit) => tree_of(git, commit)?,
        None => git
            .run_with(&["mktree"], &[], Some(b""))
            .map_err(|error| error.message())?
            .trim()
            .to_owned(),
    };
    let changed = changed_paths(git, &parent, &base.commit)?;
    let id = format!("{}-baseline", candidate.id);
    let on = Candidate {
        commit: &base.commit,
        tree: candidate.base_tree,
        base: candidate.base,
        base_tree: &parent,
        onto: parent_commit.as_deref().unwrap_or(&parent),
        change: None,
        changed_paths: &changed,
        id: &id,
        policy_provenance: candidate.policy_provenance,
        inputs: candidate.inputs,
    };
    let (mut judged, ran) = run_once(
        git,
        policy,
        &on,
        limits,
        hooks,
        (index, entry),
        (work_dir, started, &about(git, &on)?),
    )?;
    if let Some(reason) = judged.unavailable.take() {
        return Ok(Err(format!(
            "no worker ran the check on the base {}: {reason}",
            base.commit
        )));
    }
    // The base's run is whole: its own failures are what the candidate is compared with.
    judged.excused = None;
    let mut record = evidence(
        &on,
        producer,
        policy,
        (index, entry, 0),
        judged,
        ran,
        &limits.sandbox,
    );
    record.unknown.insert(
        "baseline_for".to_owned(),
        serde_json::Value::String(candidate.commit.to_owned()),
    );
    (hooks.keep)(&mut record)?;
    on_base.push(record.clone());
    Ok(Ok(record))
}

/// The tree of `commit`.
fn tree_of(git: &Git, commit: &str) -> Result<String, String> {
    git.run(&["rev-parse", &format!("{commit}^{{tree}}")])
        .map(|tree| tree.trim().to_owned())
        .map_err(|error| error.message())
}

/// N-3: judges `judged`, a candidate's run that failed, against `answer`, the check's record on
/// the base `base` (or why the base has none), and returns what the record states of it:
/// `{"commit", "record", "failures"}`, the base record's id and the units that failed on both.
///
/// A failing unit is pre-existing only when the same unit (by id) failed on the base; one the
/// base passed, has no result for, or lacks is a failure of the candidate. When every failing
/// unit is pre-existing, the run is judged by the rest of its result ([`Excused`]); otherwise
/// it stays `product_failure`. A base with no answer, `unknown` or `infra_failure`, makes the
/// run `unknown`: a failure that cannot be told new or old is never a pass.
fn against_base(
    judged: &mut Judged,
    answer: Result<EvidenceRecord, String>,
    base: &str,
) -> serde_json::Value {
    let record = match answer {
        Ok(record) => record,
        Err(why) => {
            judged.verdict = Verdict::Unknown;
            judged.why = Some(why);
            judged.excused = None;
            return json!({"commit": base});
        }
    };
    let mut stated = json!({"commit": base, "record": record.id});
    if matches!(record.verdict, Verdict::Unknown | Verdict::InfraFailure) {
        let word = serde_json::to_value(record.verdict)
            .ok()
            .and_then(|word| word.as_str().map(str::to_owned))
            .unwrap_or_default();
        judged.verdict = Verdict::Unknown;
        judged.why = Some(format!(
            "the check's run on the base {base} is {word}{}; a failure on the candidate cannot be told new or pre-existing, and unknown is never pass",
            record
                .why
                .as_ref()
                .map_or_else(String::new, |why| format!(" ({why})"))
        ));
        judged.excused = None;
        return stated;
    }
    let failed_on_base = |id: &str| {
        record
            .units
            .iter()
            .any(|unit| unit.id == id && unit.verdict == Verdict::ProductFailure)
    };
    let failing: Vec<&str> = judged
        .units
        .iter()
        .filter(|unit| unit.verdict == Verdict::ProductFailure)
        .map(|unit| unit.id.as_str())
        .collect();
    let preexisting: Vec<&str> = failing
        .iter()
        .copied()
        .filter(|id| failed_on_base(id))
        .collect();
    stated["failures"] = json!(preexisting);
    if preexisting.len() == failing.len()
        && let Some(excused) = judged.excused.take()
    {
        judged.verdict = excused.verdict;
        judged.why = excused.why;
        judged.selection = excused.selection;
    }
    stated
}

/// What a run calls back into: `keep` takes each record as soon as its check ends; `beat`
/// renews the executor's lease while a check runs; `seen`, when set, is shown each exchange
/// with the adapter as it ended (the conformance kit, `bay plugin test check`).
pub struct Hooks<'a> {
    pub keep: &'a mut dyn FnMut(&mut EvidenceRecord) -> Result<(), String>,
    pub beat: &'a mut dyn FnMut(),
    pub seen: Option<&'a mut dyn FnMut(&Exchange)>,
}

/// One exchange with a check adapter, as it ended: the request bay sent, and what came back.
#[derive(Clone, Debug)]
pub struct Exchange {
    pub request: serde_json::Value,
    /// The adapter's exit status; `None` when it did not start or ran past its timeout.
    pub status: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub elapsed: Duration,
    /// The adapter ran past its timeout and was killed.
    pub timed_out: bool,
    /// Why there is no status.
    pub error: Option<String>,
}

impl Exchange {
    fn new(
        request: &serde_json::Value,
        ran: &Result<(ExitStatus, Vec<u8>, String), String>,
        elapsed: Duration,
        timeout: Duration,
    ) -> Self {
        let (status, stdout, stderr, error) = match ran {
            Ok((status, stdout, stderr)) => (Some(*status), stdout.clone(), stderr.clone(), None),
            Err(why) => (None, Vec::new(), String::new(), Some(why.clone())),
        };
        Self {
            request: request.clone(),
            timed_out: status.is_none() && elapsed >= timeout,
            status,
            stdout,
            stderr,
            elapsed,
            error,
        }
    }
}

/// Runs one check within its own timeout and what is left of the execution budget.
fn within_budget(
    hooks: &mut Hooks,
    heartbeat: Option<Duration>,
    entry: &CheckEntry,
    policy: &Policy,
    candidate: &Candidate,
    place: &Place,
    remaining: Option<Duration>,
) -> Judged {
    let own = Duration::from_secs(entry.timeout_s);
    match remaining {
        Some(remaining) if remaining.is_zero() => Judged::without_units(
            Verdict::Unknown,
            Some("the execution budget was exhausted before this check started".to_owned()),
        ),
        Some(remaining) if remaining < own => {
            let mut judged = run_one(hooks, heartbeat, entry, policy, candidate, place, remaining);
            judged.why = judged.why.map(|why| {
                if why.contains("timed out") {
                    format!("the execution budget was exhausted: {why}")
                } else {
                    why
                }
            });
            judged
        }
        _ => run_one(hooks, heartbeat, entry, policy, candidate, place, own),
    }
}

/// Where one check runs.
struct Place<'a> {
    tree: &'a Path,
    adapter: &'a Path,
    scratch: &'a Path,
    hidden: &'a [PathBuf],
    sandbox: &'a str,
    /// G285: the box's read paths (`bay.checkReadPath`), bound read-only where they exist.
    read: &'a [PathBuf],
    /// The request's `base`, `change` and `messages` ([`about`]).
    about: &'a serde_json::Map<String, serde_json::Value>,
    /// The read-only history of a check with `inputs = ["history"]`.
    history: Option<&'a History>,
}

/// The `bay.check.request/1` bay sends for `entry` on `candidate` (`INTERFACES` § 3). Fields
/// are only ever added to a request of one schema, and an adapter ignores a field it does not
/// know, so `base`, `change`, `messages`, `messages_truncated`, `git` and `materialized` keep
/// it at `/1`:
/// - `materialized`: `"changed"`, only for a check with `materialize = "changed"` (N-18): its
///   tree holds only the request's `changed_paths` that the candidate still has, and the
///   entry's `paths`.
/// - `base`: `{"commit", "tree"}`, the accepted base the candidate was merged onto.
/// - `change`: `{"id", "intent": {"reference", "revision"}, "actor", "revision"}`, and
///   `messages`: the change's own commits between the base and its revision, oldest first,
///   each `{"commit", "message"}`, at most [`MESSAGES_MAX`] and [`MESSAGES_MAX_BYTES`], with
///   `messages_truncated` saying whether they were cut. Absent when the candidate lands no one
///   change (a batch's, a trunk bisection's).
/// - `git`: only for a check with `inputs = ["history"]` ([`History`]): `{"dir", "object_dir",
///   "alternates"?, "refs": {"base", "candidate", "revision"?}}`, so that
///   `GIT_DIR=<dir> git log <base>` reads the repository's history, and no write reaches it.
///
/// A check whose verdict reads `base`, `change` or `messages` declares `inputs = ["commit"]` or
/// `["history"]`, so its evidence is keyed by subject commit and never stands for another
/// commit with the same tree.
fn request(
    entry: &CheckEntry,
    policy: &Policy,
    candidate: &Candidate,
    place: &Place,
) -> serde_json::Value {
    let mut requires = json!({"environment": entry.environment});
    if !entry.fixtures.is_empty() {
        // § 3: the fixtures the policy pins, by digest (ruling R18).
        requires["fixtures"] = json!(entry.fixtures);
    }
    let mut request = json!({
        "schema": "bay.check.request/1",
        "candidate": {"commit": candidate.commit, "tree": candidate.tree},
        "changed_paths": candidate.changed_paths,
        "plan": {"id": entry.id, "version": policy.revision.to_string()},
        "requires": requires,
        "timeout_s": entry.timeout_s,
        "inputs": candidate.inputs,
    });
    // N-18: the tree holds only `changed_paths` (and the entry's `paths`), so an adapter knows
    // that a file absent from it may still be in the candidate.
    if entry.changed_only() {
        request["materialized"] = json!("changed");
    }
    if let Some(fields) = request.as_object_mut() {
        fields.extend(place.about.clone());
        if let Some(history) = place.history {
            fields.insert("git".to_owned(), history.request());
        }
    }
    request
}

fn run_one(
    hooks: &mut Hooks,
    heartbeat: Option<Duration>,
    entry: &CheckEntry,
    policy: &Policy,
    candidate: &Candidate,
    place: &Place,
    timeout: Duration,
) -> Judged {
    let (tree_dir, adapter_dir) = (place.tree, place.adapter);
    let mut words = entry.command.split_whitespace();
    let executable = adapter_dir.join(words.next().unwrap_or_default());
    let request = request(entry, policy, candidate, place);
    let args: Vec<&str> = words.collect();
    // Ruling 8b: a named variable the executor does not set is an infrastructure failure;
    // the check never runs without what its policy declares.
    let mut declared = Vec::new();
    for name in &entry.env {
        let Some(value) = std::env::var_os(name) else {
            return Judged::without_units(
                Verdict::InfraFailure,
                Some(format!(
                    "the policy declares {name} in env, and this runner does not set it"
                )),
            );
        };
        declared.push((name, value));
    }
    let scratch_tmp = place.scratch.join("tmp");
    let scratch_home = place.scratch.join("home");
    let _ = std::fs::create_dir_all(&scratch_tmp);
    let _ = std::fs::create_dir_all(&scratch_home);
    // G285: the box's read paths join the history's, read-only; one that does not exist on
    // this box is not bound (`bwrap` refuses to bind a missing path; `bay doctor` names it).
    let mut read_only = place.history.map(History::read_only).unwrap_or_default();
    read_only.extend(place.read.iter().filter(|path| path.exists()).cloned());
    let mut cmd = match sandboxed(
        place.sandbox,
        &executable,
        &args,
        place.hidden,
        (place.scratch, read_only.as_slice()),
        entry.network,
    ) {
        Ok(cmd) => cmd,
        Err(why) => return Judged::without_units(Verdict::InfraFailure, Some(why)),
    };
    // `INTERFACES` § 7: the check starts from a fixed baseline and nothing else, so evidence
    // names everything it could read: `PATH` from the policy, `HOME` and `TMPDIR` inside the
    // sandbox's scratch, `LANG=C.UTF-8`, bay's own `BAY_ADAPTER_DIR`, and the declared names.
    // The working directory is the candidate tree, the root an adapter judges its paths from:
    // the tree's absolute path usually holds a `.git` part, a path inside it never does (G241).
    cmd.current_dir(tree_dir)
        .env_clear()
        .env("BAY_ADAPTER_DIR", adapter_dir)
        .env("HOME", &scratch_home)
        .env("TMPDIR", &scratch_tmp)
        .env("LANG", "C.UTF-8")
        // The policy states `path` or takes its stated default, so `PATH` is always set: a
        // shell never falls back to a search path no policy names.
        .env("PATH", crate::platform::search_path(&entry.path))
        // `policy::load_at` refuses an `env` name of the baseline, so none overrides it.
        .envs(declared);
    let (output, stderr) = match exchange(hooks, cmd, &request, timeout, heartbeat) {
        Ok(output) => output,
        Err(why) => return Judged::without_units(Verdict::Unknown, Some(why)),
    };
    // § 7, G26: a check that reached outside its adapter and candidate is an infrastructure
    // failure, never a pass.
    let reached_out = reaches_out(&stderr, place);
    let outside = || {
        Judged::without_units(
            Verdict::InfraFailure,
            Some("the check read or wrote outside its plugin and candidate".to_owned()),
        )
    };
    if !reached_out && let Some(reason) = unavailable_answer(&output) {
        return Judged {
            unavailable: Some(reason),
            ..Judged::without_units(Verdict::Unknown, None)
        };
    }
    let result: CheckResult = match serde_json::from_slice(&output) {
        Ok(result) => result,
        Err(_) if reached_out => return outside(),
        Err(error) => {
            return Judged::without_units(
                Verdict::Unknown,
                Some(format!("no bay.check.result/1 on stdout: {error}")),
            );
        }
    };
    if reached_out {
        return outside();
    }
    judge(&result, entry, candidate)
}

/// Runs the adapter with `request` on stdin within `timeout`, shows the exchange to
/// `hooks.seen`, and returns its stdout and stderr. A spawn failure, a timeout, or a crash (an
/// exit other than 0 with nothing on stdout or stderr) is an error the caller records as
/// `unknown`.
fn exchange(
    hooks: &mut Hooks,
    cmd: Command,
    request: &serde_json::Value,
    timeout: Duration,
    heartbeat: Option<Duration>,
) -> Result<(Vec<u8>, String), String> {
    let started = Instant::now();
    let ran = run_with_timeout(
        cmd,
        &request.to_string(),
        timeout,
        &mut *hooks.beat,
        heartbeat,
    );
    if let Some(seen) = hooks.seen.as_deref_mut() {
        seen(&Exchange::new(request, &ran, started.elapsed(), timeout));
    }
    let (status, output, stderr) = ran?;
    if !status.success() && output.is_empty() && stderr.is_empty() {
        return Err(format!(
            "the check crashed ({status}) and returned no result"
        ));
    }
    Ok((output, stderr))
}

/// Whether the check's stderr names a hidden path. A path inside the check's own scratch is
/// never one, though the scratch sits under the repository's git directory (G241): a shell's
/// error about the check's own adapter is no reach outside. Nor is the object directory a
/// history check reads (G267): a Git refusing to write there names it.
fn reaches_out(stderr: &str, place: &Place) -> bool {
    let mut own = forms(place.scratch);
    if let Some(history) = place.history {
        for dir in history.read_only() {
            own.extend(forms(&dir));
        }
    }
    place.hidden.iter().any(|hidden| {
        forms(hidden)
            .iter()
            .any(|form| names_path_outside(stderr, form, &own))
    })
}

/// Each form a message may name `path` by: as given, its real path, and, for a path under
/// macOS's `/private/var`, `/private/tmp` or `/private/etc`, the short form through the
/// `/var`, `/tmp` or `/etc` link, which a shell prints when its `TMPDIR` or argument used it.
fn forms(path: &Path) -> Vec<String> {
    let mut forms = Vec::new();
    for given in [path.to_path_buf(), real(path)] {
        let given = given.display().to_string();
        let short = ["/var", "/tmp", "/etc"].iter().find_map(|link| {
            let rest = given.strip_prefix("/private")?.strip_prefix(link)?;
            (rest.is_empty() || rest.starts_with('/')).then(|| format!("{link}{rest}"))
        });
        forms.extend(Some(given).into_iter().chain(short));
    }
    forms.retain(|form| !form.is_empty());
    forms.sort();
    forms.dedup();
    forms
}

/// Whether `text` names `path` or a path inside it: an occurrence of `path` that starts and
/// ends on a path boundary, so `/box/repo` is named by `/box/repo` and `/box/repo/x` but not by
/// its sibling `/box/repo-remote.git` or by `/other/box/repo`.
#[cfg(test)]
fn names_path(text: &str, path: &str) -> bool {
    names_path_outside(text, path, &[])
}

/// [`names_path`], leaving out each mention of `path` that is the start of one of `own`: a
/// path inside the check's own scratch, which may sit under `path`.
fn names_path_outside(text: &str, path: &str, own: &[String]) -> bool {
    // What may stand next to a path in a message: whitespace, quotes and punctuation.
    let delimiter = |c: char| c.is_whitespace() || "'\"`:,;()[]<>=".contains(c);
    if path.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(found) = text[from..].find(path) {
        let start = from + found;
        let end = start + path.len();
        let before = text[..start].chars().next_back();
        let mut after = text[end..].chars();
        let opens = before.is_none_or(delimiter);
        let closes = match after.next() {
            None | Some('/') => true,
            // A full stop ends a sentence, never a name such as `repo.git`.
            Some('.') => after.next().is_none_or(delimiter),
            Some(c) => delimiter(c),
        };
        let inside_own = own
            .iter()
            .any(|own| text[start..].starts_with(own.as_str()));
        if opens && closes && !inside_own {
            return true;
        }
        from = start + text[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// The units as the evidence record keeps them: a time only when it is a real one, a reason
/// only on a unit that did not pass, and none of the adapter's other fields.
fn recorded_units(result: &CheckResult) -> Vec<Unit> {
    result
        .checks
        .iter()
        .map(|unit| Unit {
            id: unit.id.clone(),
            verdict: unit.verdict,
            seconds: unit
                .seconds
                .filter(|seconds| seconds.is_finite() && *seconds >= 0.0),
            reason: unit
                .reason
                .clone()
                .filter(|_| unit.verdict != Verdict::Pass),
            unknown: serde_json::Map::new(),
        })
        .collect()
}

/// G240: a reason is shown whole or not at all; one too long to show makes the run an infra
/// failure of the adapter, never a cut message.
fn overlong_reason(result: &CheckResult) -> Option<Judged> {
    let unit = result.checks.iter().find(|unit| {
        unit.reason
            .as_ref()
            .is_some_and(|reason| reason.len() > crate::evidence::REASON_MAX_BYTES)
    })?;
    Some(Judged::without_units(
        Verdict::InfraFailure,
        Some(format!(
            "unit {} sent a reason of {} bytes; at most {} are shown",
            unit.id,
            unit.reason.as_ref().map_or(0, String::len),
            crate::evidence::REASON_MAX_BYTES
        )),
    ))
}

fn judge(result: &CheckResult, entry: &CheckEntry, candidate: &Candidate) -> Judged {
    if result.schema != "bay.check.result/1" {
        return Judged::without_units(Verdict::Unknown, Some(format!("schema {}", result.schema)));
    }
    if result.exercised.commit != candidate.commit || result.exercised.tree != candidate.tree {
        return Judged::without_units(
            Verdict::Unknown,
            Some("the result is for a different commit or tree; rejected whole".to_owned()),
        );
    }
    if let Some(too_long) = overlong_reason(result) {
        return too_long;
    }
    let units = recorded_units(result);
    let failed = units
        .iter()
        .any(|unit| unit.verdict == Verdict::ProductFailure);
    let mut judged = judge_units(result, entry, units);
    if failed {
        // A failing unit is the run's verdict. N-3: the rest of the result is judged all the
        // same, so a check with `baseline = "base"` whose every failure is pre-existing records
        // what its other units, its plan and its selection say, never a pass by default.
        judged.excused = Some(Excused {
            verdict: judged.verdict,
            why: judged.why.take(),
            selection: judged.selection.take(),
        });
        judged.verdict = Verdict::ProductFailure;
    }
    judged
}

/// [`judge`] past the failing units: the plan, the selection and every unit that did not
/// fail. A failing unit is judged by the caller.
fn judge_units(result: &CheckResult, entry: &CheckEntry, units: Vec<Unit>) -> Judged {
    let with = |verdict: Verdict, why: Option<String>, units: Vec<Unit>| Judged {
        verdict,
        why,
        units,
        selection: None,
        unavailable: None,
        excused: None,
    };
    // § 7: `declared_empty` is valid only with `selected` = 0 and `expected_minimum` = 0; any
    // other combination is `unknown`.
    let declared = result.plan.selection.as_deref();
    if let Some(selection) = declared
        && (selection != DECLARED_EMPTY || result.plan.selected > 0 || entry.minimum() > 0)
    {
        return with(
            Verdict::Unknown,
            Some(format!(
                "selection {selection} with {} selected under the policy's expected_minimum {}; {DECLARED_EMPTY} needs both at zero",
                result.plan.selected,
                entry.minimum()
            )),
            units,
        );
    }
    let collected = u64::try_from(units.len()).unwrap_or(u64::MAX);
    // A selection of tests with no result for any of them is no empty selection: the units
    // the adapter says it ran are missing, so the run is `unknown`, never `declared_empty`.
    if collected == 0 && result.plan.selected > 0 {
        return with(
            Verdict::Unknown,
            Some(format!(
                "selected {} and reported no unit",
                result.plan.selected
            )),
            units,
        );
    }
    if collected < entry.minimum() {
        return with(
            Verdict::Unknown,
            Some(format!(
                "collected {collected} of the policy's expected_minimum {}",
                entry.minimum()
            )),
            units,
        );
    }
    if result.plan.completed < result.plan.selected {
        return with(
            Verdict::Unknown,
            Some(format!(
                "completed {} of {} selected",
                result.plan.completed, result.plan.selected
            )),
            units,
        );
    }
    if units
        .iter()
        .any(|unit| unit.verdict == Verdict::InfraFailure)
    {
        return with(Verdict::InfraFailure, None, units);
    }
    if units.iter().any(|unit| unit.verdict == Verdict::Unknown) {
        return with(
            Verdict::Unknown,
            Some("a unit has no result".to_owned()),
            units,
        );
    }
    // § 7: a pass on an empty selection under an explicit zero minimum is recorded as
    // `declared_empty`, so it is reported as a coverage gap, never as tested code.
    let empty = declared.is_some() || collected == 0;
    Judged {
        verdict: Verdict::Pass,
        why: None,
        units,
        selection: empty.then(|| DECLARED_EMPTY.to_owned()),
        unavailable: None,
        excused: None,
    }
}

/// Runs the adapter with the request on stdin; returns its exit status, stdout and stderr. A
/// spawn failure or a timeout is an error, which the caller records as `unknown`, as it does a
/// crash: an exit other than 0 with nothing on stdout or stderr. The sandbox around it is
/// `cmd`'s own; it runs as any adapter does, in a process group of its own that is killed
/// whole at the bound (`crate::run::bounded`).
fn run_with_timeout(
    cmd: Command,
    input: &str,
    timeout: Duration,
    beat: &mut dyn FnMut(),
    heartbeat: Option<Duration>,
) -> Result<(ExitStatus, Vec<u8>, String), String> {
    let ran = crate::run::bounded(
        cmd,
        Some(input.as_bytes()),
        timeout,
        crate::run::Streams::Captured,
        beat,
        heartbeat,
    )
    .map_err(|fault| match fault {
        crate::run::Fault::Spawn(error) => format!("the check did not start: {error}"),
        crate::run::Fault::Timeout(_) => {
            format!("the check timed out after {}s", timeout.as_secs())
        }
        other => other.to_string(),
    })?;
    let stderr = String::from_utf8_lossy(&ran.stderr).into_owned();
    Ok((ran.status, ran.stdout, stderr))
}

/// Materializes what `entry` sees of the candidate (its `paths`, or the whole tree) into
/// `tree_dir` and its adapter paths from the base into `adapter_dir`, read-only, both within
/// the entry's `materialize_timeout_s` and what is left of the execution budget (`left`),
/// renewing the lease (`beat`, every `heartbeat`) as it goes. Returns what is on record when
/// the check does not run: `infra_failure` naming the bound when it is exceeded or naming a
/// path of the tree bay does not write, `unknown` when the budget runs out first; never a pass.
///
/// # Errors
///
/// Either tree cannot be materialized for another reason (a fault of bay's own).
fn prepare(
    git: &Git,
    entry: &CheckEntry,
    candidate: &Candidate,
    (tree_dir, adapter_dir): (&Path, &Path),
    left: Option<Duration>,
    (beat, heartbeat): (&mut dyn FnMut(), Option<Duration>),
) -> Result<Option<Judged>, String> {
    let budget_out = |when: &str| {
        Some(Judged::without_units(
            Verdict::Unknown,
            Some(format!("the execution budget was exhausted {when}")),
        ))
    };
    if left.is_some_and(|left| left.is_zero()) {
        return Ok(budget_out("before this check started"));
    }
    let seconds = entry.materialize_timeout_s.0;
    let bound = materialize_bound(seconds);
    let by_budget = left.is_some_and(|left| left < bound);
    let now = Instant::now();
    let within = match left {
        Some(left) if by_budget => left,
        _ => bound,
    };
    // A bound beyond what the clock can hold is never reached: the latest deadline it holds
    // instead, never an overflow.
    let deadline = now
        .checked_add(within)
        .or_else(|| now.checked_add(Duration::from_secs(u64::from(u32::MAX))))
        .unwrap_or(now);
    let mut last_beat = Instant::now();
    let mut tick = || {
        if heartbeat.is_some_and(|every| last_beat.elapsed() >= every) {
            beat();
            last_beat = Instant::now();
        }
    };
    // N-18: a changed-only check gets the candidate's changed paths and its `paths`, never the
    // rest of the tree; its adapter paths still come from the base, as for any check.
    let kept = entry.materialized(candidate.changed_paths);
    let made = materialize(
        git,
        candidate.commit,
        kept.as_deref(),
        tree_dir,
        deadline,
        &mut tick,
    )
    .and_then(|()| {
        materialize(
            git,
            candidate.base,
            Some(&entry.adapter),
            adapter_dir,
            deadline,
            &mut tick,
        )
    });
    // BACKLOG 49 phase 2: the files the manifest keeps outside Git, by their bytes.
    let made = made.map(|()| {
        crate::assets::fill_check(
            git,
            candidate.commit,
            candidate.base,
            tree_dir,
            kept.as_deref(),
            deadline,
        )
    });
    match made {
        Ok(Err(why)) => Ok(Some(Judged::without_units(
            Verdict::InfraFailure,
            Some(why),
        ))),
        Ok(Ok(())) => {
            make_read_only(adapter_dir);
            Ok(None)
        }
        Err(Unmade::TimedOut) if by_budget => Ok(budget_out(
            "while materializing the candidate and the plugin; the check did not run",
        )),
        Err(Unmade::TimedOut) => Ok(Some(Judged::without_units(
            Verdict::InfraFailure,
            Some(format!(
                "materializing the candidate and the plugin exceeded the check's materialize_timeout_s ({seconds}s); the check did not run"
            )),
        ))),
        Err(Unmade::Refused(why)) => Ok(Some(Judged::without_units(
            Verdict::InfraFailure,
            Some(format!("{why}; the check did not run")),
        ))),
        Err(Unmade::Failed(why)) => Err(why),
        // G265: a box below its disk floor runs no check; never a pass.
        Err(Unmade::DiskLow(low)) => Ok(Some(Judged::without_units(
            Verdict::InfraFailure,
            Some(format!("{}; the check did not run", low.message())),
        ))),
    }
}

/// `materialize_timeout_s` as a duration. Under test a second of it is a millisecond, so a
/// test can exceed the smallest valid bound without a tree that takes a second to write.
fn materialize_bound(seconds: u64) -> Duration {
    #[cfg(test)]
    if tests::MILLISECOND_BOUNDS.with(std::cell::Cell::get) {
        return Duration::from_millis(seconds);
    }
    Duration::from_secs(seconds)
}

#[cfg(unix)]
fn walk(dir: &Path, mode: u32, files_too: bool) {
    use std::os::unix::fs::PermissionsExt as _;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, mode, files_too);
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode | 0o111));
        } else if files_too && let Ok(meta) = path.metadata() {
            let exec = meta.permissions().mode() & 0o111;
            let _ = std::fs::set_permissions(
                &path,
                std::fs::Permissions::from_mode((mode & 0o666) | exec),
            );
        }
    }
}

#[cfg(unix)]
fn make_read_only(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    walk(dir, 0o555, true);
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555));
}

#[cfg(unix)]
fn make_writable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
    walk(dir, 0o755, false);
    let _ = std::fs::set_permissions(dir.join("adapter"), std::fs::Permissions::from_mode(0o755));
    walk(&dir.join("adapter"), 0o755, false);
}

/// Where there are no Unix modes, the adapter's files are read-only by the file attribute;
/// a directory has no such attribute that stops writes into it.
#[cfg(not(unix))]
fn set_read_only(dir: &Path, read_only: bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            set_read_only(&path, read_only);
        } else if let Ok(meta) = path.metadata() {
            let mut permissions = meta.permissions();
            permissions.set_readonly(read_only);
            let _ = std::fs::set_permissions(&path, permissions);
        }
    }
}

#[cfg(not(unix))]
fn make_read_only(dir: &Path) {
    set_read_only(dir, true);
}

#[cfg(not(unix))]
fn make_writable(dir: &Path) {
    set_read_only(dir, false);
}

#[cfg(test)]
mod tests {
    use super::{NO_SANDBOX, Sandbox};

    thread_local! {
        /// Set by a test that needs a bound smaller than one second: then a second of
        /// `materialize_timeout_s` is a millisecond (see `materialize_bound`).
        pub(super) static MILLISECOND_BOUNDS: std::cell::Cell<bool> =
            const { std::cell::Cell::new(false) };
    }

    /// G285: a box read path is absolute, or `~/` under HOME; anything else is refused by name,
    /// and one that would show a hidden path (the repository, a key) is refused too.
    #[test]
    fn a_box_read_path_is_absolute_and_never_overlaps_a_hidden_path() {
        use std::path::{Path, PathBuf};
        let home = Some(Path::new("/home/me"));
        assert_eq!(
            super::read_path("~/.store", home).unwrap(),
            PathBuf::from("/home/me/.store")
        );
        assert_eq!(
            super::read_path("~", home).unwrap(),
            PathBuf::from("/home/me")
        );
        assert_eq!(
            super::read_path("/opt/store", None).unwrap(),
            PathBuf::from("/opt/store")
        );
        let refused = super::read_path("store", home).unwrap_err();
        assert!(
            refused.contains("\"store\" is not an absolute path"),
            "{refused}"
        );
        assert!(super::read_path("~other/store", home).is_err());
        assert!(super::read_path("~/.store", None).is_err());
        let hidden = [
            PathBuf::from("/nonexistent/repo"),
            PathBuf::from("/nonexistent/home/.ssh/key"),
        ];
        let inside = [PathBuf::from("/nonexistent/repo/node_modules")];
        assert!(super::read_path_overlap(&inside, &hidden).is_some());
        let holds = [PathBuf::from("/nonexistent/home")];
        assert!(super::read_path_overlap(&holds, &hidden).is_some());
        let apart = [PathBuf::from("/nonexistent/store")];
        assert!(super::read_path_overlap(&apart, &hidden).is_none());
    }

    /// The `AppArmor` remedy is named only for an installed bwrap under the restriction, and
    /// names that bwrap.
    #[test]
    fn the_apparmor_remedy_is_named_only_where_it_applies() {
        let bwrap = std::path::Path::new("/opt/tools/bwrap");
        let remedy = super::apparmor_remedy(Some(bwrap), Some("1\n")).unwrap();
        assert!(remedy.contains("userns"), "{remedy}");
        assert!(
            remedy.contains("profile bwrap /opt/tools/bwrap"),
            "{remedy}"
        );
        assert!(remedy.contains(super::UNSANDBOXED_KEY), "{remedy}");
        assert!(super::apparmor_remedy(Some(bwrap), Some("0\n")).is_none());
        assert!(
            super::apparmor_remedy(None, Some("1\n")).is_none(),
            "install bwrap first"
        );
        assert!(super::apparmor_remedy(Some(bwrap), None).is_none());
    }

    /// § 7, G26: a mention of a hidden path that is the check's own scratch (which sits under
    /// the repository's git directory) is no reach outside; any other mention still is.
    #[test]
    fn the_checks_own_scratch_is_no_reach_outside() {
        let hidden = "/box/repo";
        let own = vec!["/box/repo/.git/bay/check/c1/check-0".to_owned()];
        let names = |text: &str| super::names_path_outside(text, hidden, &own);
        assert!(!names(
            "sh: /box/repo/.git/bay/check/c1/check-0/adapter/scripts/bay-check: not found"
        ));
        assert!(names("sh: .: cannot open /box/repo/scripts/helper.sh"));
        assert!(names(
            "/box/repo/.git/bay/check/c1/check-0/adapter: ok; then /box/repo/secret: denied"
        ));
        assert!(
            names("/box/repo/.git/config: denied"),
            "the rest of .git stays hidden"
        );
    }

    /// § 7, G26 on macOS: a hidden path under `/private/var` is also named by its `/var` form,
    /// which is how a shell prints it; a path that only shares the prefix gets no short form.
    #[test]
    fn a_private_path_is_also_named_by_its_short_form() {
        let forms = |path: &str| super::forms(std::path::Path::new(path));
        let hidden = forms("/private/var/folders/x/T/box/repo");
        assert!(
            hidden.contains(&"/var/folders/x/T/box/repo".to_owned()),
            "{hidden:?}"
        );
        assert!(hidden.contains(&"/private/var/folders/x/T/box/repo".to_owned()));
        assert!(forms("/private/tmp").contains(&"/tmp".to_owned()));
        assert!(!forms("/private/variable/x").contains(&"/variable/x".to_owned()));
        assert_eq!(forms("/box/repo"), vec!["/box/repo".to_owned()]);
    }

    /// § 7, G26: a hidden path is named by whole components only. A sibling that shares its
    /// prefix (`repo` and `repo-remote.git`) is not the hidden path.
    #[test]
    fn a_hidden_path_is_named_by_whole_components_only() {
        let hidden = "/box/repo";
        for named in [
            "cat: /box/repo: Permission denied",
            "cat: '/box/repo/secret.txt': No such file or directory",
            "fatal: not a git repository: /box/repo.",
            "/box/repo",
            "open(\"/box/repo/.git\")",
        ] {
            assert!(super::names_path(named, hidden), "{named}");
        }
        for other in [
            "To /box/repo-remote.git",
            "fatal: '/box/repo-remote.git' does not appear to be a git repository",
            "/box/repo.git",
            "/box/repository",
            "/other/box/repo",
            "/box/repo2/x /box/repos",
        ] {
            assert!(!super::names_path(other, hidden), "{other}");
        }
        // The first occurrence is a sibling and a later one is the path itself.
        assert!(super::names_path(
            "/box/repo-remote.git and /box/repo/x",
            hidden
        ));
    }

    #[test]
    fn no_sandbox_refuses_unless_the_box_chose_none() {
        // R14 on every platform: no sandbox and no choice is refused.
        let refused = Sandbox::judge("none", Ok(false));
        assert_eq!(refused.kind, "none");
        assert_eq!(refused.refusal.as_deref(), Some(NO_SANDBOX));
        // An unreadable or unknown choice is no choice, and the refusal says why.
        let unknown = Sandbox::judge("none", Err("bay.runnerSandbox = \"off\"".to_owned()));
        let why = unknown.refusal.unwrap();
        assert!(why.contains("\"off\"") && why.contains(NO_SANDBOX), "{why}");
        // The box's `bay.runnerSandbox = none` runs the check, recorded as `none`.
        assert!(Sandbox::judge("none", Ok(true)).refusal.is_none());
        // With a sandbox the choice does not matter.
        assert!(Sandbox::judge("bwrap", Ok(false)).refusal.is_none());
        assert!(
            Sandbox::judge("sandbox-exec", Err(String::new()))
                .refusal
                .is_none()
        );
    }

    /// Ruling R14 on a real run: an adapter that passes, run where there is no sandbox and the
    /// box did not choose `bay.runnerSandbox = none`, is `infra_failure`, never a pass;
    /// with that choice the same adapter passes and its record says `none`. The adapter is a
    /// shell script, so this runs where a shell runs adapters (plan W5 brings `sh.exe`).
    #[cfg(unix)]
    #[test]
    fn a_passing_adapter_with_no_sandbox_is_infra_failure() {
        let chose_none = unix::run_under(Sandbox::judge("none", Ok(true)));
        assert_eq!(
            chose_none.verdict,
            crate::evidence::Verdict::Pass,
            "{:?}",
            chose_none.why
        );
        assert_eq!(chose_none.sandbox.as_deref(), Some("none"));

        let refused = unix::run_under(Sandbox::judge("none", Ok(false)));
        assert_eq!(refused.verdict, crate::evidence::Verdict::InfraFailure);
        assert_eq!(refused.why.as_deref(), Some(NO_SANDBOX));
        assert_eq!(refused.sandbox.as_deref(), Some("none"));
        assert_eq!(refused.units.len(), 0);
    }

    #[cfg(unix)]
    mod unix {
        use super::super::{Candidate, Hooks, Limits, Sandbox, run_all};
        use crate::evidence::EvidenceRecord;
        use crate::git::Git;
        use std::os::unix::fs::PermissionsExt as _;

        const POLICY: &str = "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\n\
            command = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\n\
            expected_minimum = 1\ntimeout_s = 60\n";

        const ADAPTER: &str = r#"#!/bin/sh
req=$(cat)
commit=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"}.*/\1/p')
tree=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"}.*/\2/p')
printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"selected":1,"completed":1},"checks":[{"id":"t1","verdict":"pass"}]}' "$commit" "$tree"
"#;

        /// A repository whose one required check passes every candidate it is given.
        fn passing_repo() -> (tempfile::TempDir, Git, String, String) {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            std::fs::create_dir_all(root.join(".bay")).unwrap();
            std::fs::create_dir_all(root.join("scripts")).unwrap();
            std::fs::write(root.join(".bay/policy.toml"), POLICY).unwrap();
            let adapter = root.join("scripts/bay-check");
            std::fs::write(&adapter, ADAPTER).unwrap();
            std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755)).unwrap();
            let git = Git::new(root);
            git.run(&["init", "-q"]).unwrap();
            git.run(&["add", "-A"]).unwrap();
            git.run(&[
                "-c",
                "user.name=bay",
                "-c",
                "user.email=bay@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "seed",
            ])
            .unwrap();
            let commit = git.run(&["rev-parse", "HEAD"]).unwrap().trim().to_owned();
            let tree = git.run(&["rev-parse", "HEAD^{tree}"]).unwrap();
            (dir, git, commit, tree.trim().to_owned())
        }

        /// The one record of running the passing check under `sandbox`.
        pub(super) fn run_under(sandbox: Sandbox) -> EvidenceRecord {
            let (dir, git, commit, tree) = passing_repo();
            let policy = crate::policy::load_at(&git, &commit).unwrap().unwrap();
            let candidate = Candidate {
                commit: &commit,
                tree: &tree,
                base: &commit,
                base_tree: &tree,
                onto: &commit,
                change: None,
                changed_paths: &[],
                id: "op.1",
                policy_provenance: crate::policy::Provenance::Bay,
                inputs: &[],
            };
            let limits = Limits {
                budget: None,
                finished: Vec::new(),
                heartbeat: None,
                hidden: Vec::new(),
                sandbox,
                baseline: None,
                parallel: 1,
            };
            let mut keep = |_: &mut EvidenceRecord| Ok(());
            let mut records = run_all(
                &git,
                &dir.path().join("scratch"),
                &policy,
                &candidate,
                "local",
                &limits,
                &mut Hooks {
                    keep: &mut keep,
                    beat: &mut || {},
                    seen: None,
                },
            )
            .unwrap()
            .records;
            assert_eq!(records.len(), 1);
            records.remove(0)
        }
    }
}

/// Materialization through a whole run: `paths`, `materialize_timeout_s`, a large tree.
#[cfg(all(test, unix))]
mod materializing;

pub mod sandbox;
