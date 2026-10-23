//! The repository bay runs in: its common git directory, its main work tree, its config,
//! and where views live.
//!
//! The config (ruling: config location) is the `.bay/config.toml` committed at the fetched
//! trunk tracking ref, `refs/remotes/<remote>/<trunk>`: the same for the main work tree, a
//! linked worktree and every view, and never a work tree's file or a change's own commits, so
//! neither an agent's draft nor a change selects the settings it runs and lands under (as
//! policy is judged at the base). The trunk is found without reading any file a change can
//! write: the bootstrap is this repository's own git config `bay.main` and `bay.remote`
//! (written by `bay install`), else the remote's default branch, `refs/remotes/<remote>/HEAD`
//! (remote `origin` unless `bay.remote` names another). A missing bootstrap, or no config at
//! the trunk tip, is `CONFIG_MISSING`, never a silent default.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Value, json};

use crate::config::{CONFIG_PATH, Config};
use crate::git::{Git, GitError};
use crate::result::{BayResult, Reason};

/// The repository-local git config key naming the trunk (`bay install --main`).
pub const TRUNK_KEY: &str = "bay.main";
/// The repository-local git config key naming the trunk's remote (`bay install --remote`).
pub const REMOTE_KEY: &str = "bay.remote";
/// The remote when `bay.remote` is unset.
pub const DEFAULT_REMOTE: &str = "origin";

/// How the trunk was found before any file is read: the operator's choice in this repository's
/// own git config, or the remote's default branch.
#[derive(Clone, Debug)]
pub struct Bootstrap {
    pub remote: String,
    pub trunk: String,
    /// Where the trunk name came from, as a reader would look it up.
    pub source: String,
}

/// Why [`Bootstrap::read`] found no trunk.
pub enum BootstrapError {
    /// Neither place names one: what bay looked at, in order.
    Missing(Vec<String>),
    /// Git could not be read.
    Unreadable(String),
}

impl Bootstrap {
    /// Reads the bootstrap from the repository-local git config of the common directory `git`
    /// runs in (never a worktree's own config), then `refs/remotes/<remote>/HEAD`.
    ///
    /// # Errors
    ///
    /// See [`BootstrapError`].
    pub fn read(git: &Git) -> Result<Self, BootstrapError> {
        let remote = local_config(git, REMOTE_KEY)?.unwrap_or_else(|| DEFAULT_REMOTE.to_owned());
        if let Some(trunk) = local_config(git, TRUNK_KEY)? {
            return Ok(Self {
                remote,
                trunk,
                source: format!("git config {TRUNK_KEY}"),
            });
        }
        let head = format!("refs/remotes/{remote}/HEAD");
        let prefix = format!("refs/remotes/{remote}/");
        match git.run(&["symbolic-ref", "-q", &head]) {
            Ok(target) => match target.strip_prefix(&prefix) {
                Some(trunk) if !trunk.is_empty() && trunk != "HEAD" => Ok(Self {
                    trunk: trunk.to_owned(),
                    remote,
                    source: head,
                }),
                _ => Err(BootstrapError::Unreadable(format!(
                    "{head} points at {target}, not a branch of {remote}"
                ))),
            },
            Err(GitError::Failed { .. }) => Err(BootstrapError::Missing(vec![
                format!("git config {TRUNK_KEY}"),
                head,
            ])),
            Err(error) => Err(BootstrapError::Unreadable(error.message())),
        }
    }

    /// [`Bootstrap::read`] for the repository that contains `dir`; `None` when no repository
    /// holds it.
    #[must_use]
    pub fn of(dir: &Path) -> Option<Result<Self, BootstrapError>> {
        let common = Git::new(dir).common_dir().ok()?;
        Some(Self::read(&Git::new(common)))
    }

    /// The local tracking ref of the remote trunk.
    #[must_use]
    pub fn tracking_ref(&self) -> String {
        format!("refs/remotes/{}/{}", self.remote, self.trunk)
    }
}

/// A key of the repository-local git config (`$GIT_COMMON_DIR/config`); `None` when unset.
fn local_config(git: &Git, key: &str) -> Result<Option<String>, BootstrapError> {
    match git.config_get_local(key) {
        Ok(value) => Ok(value.filter(|value| !value.is_empty())),
        // Exit 1 with nothing on stderr: the key is unset. Anything else is not "unset".
        Err(GitError::Failed { stderr, .. }) if stderr.trim().is_empty() => Ok(None),
        Err(error) => Err(BootstrapError::Unreadable(error.message())),
    }
}

/// When [`Repo`] fetches the trunk before it reads the config there.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Fetch {
    /// Never: a reader that must not touch the network (the agent hook, `bay doctor`,
    /// `bay install`, `bay status --fleet`).
    Never,
    /// Only when the tracking ref does not exist yet (before the first fetch).
    IfMissing,
    /// Always first: a command that fetches trunk reads the config at the tip it fetched.
    First,
}

/// What [`Repo::remote_record`] read: the remote's head of a change's chain, and its record.
#[derive(Debug, Clone)]
pub struct RemoteRecord {
    /// The remote's head of the chain.
    pub head: String,
    /// The record at `head`: the remote's view of the change.
    pub record: crate::change::ChangeRecord,
}

pub struct Repo {
    /// Git on the common directory: every view of this repository shares it.
    pub git: Git,
    pub common_dir: PathBuf,
    /// The main work tree.
    pub work_root: PathBuf,
    /// The directory bay was started in (`-C`).
    pub opened_in: PathBuf,
    /// How the trunk was found.
    pub bootstrap: Bootstrap,
    /// The config committed at the trunk tip bay read, with `trunk` and `remote` set to the
    /// bootstrap's.
    pub config: Config,
    /// What an opening fetch ([`Fetch::First`]) found, handed to the first
    /// [`Repo::fetch_trunk`] so one command fetches once.
    prefetched: Mutex<Option<Result<String, GitError>>>,
    /// When this command last fetched trunk, for the result's `freshness` (G80).
    fetched_at: Mutex<Option<String>>,
}

/// Why [`Repo::open_with`] could not open a repository.
pub enum OpenError {
    /// No repository holds the directory.
    NoRepository(String),
    /// A repository bay cannot open (a bare one, a config that does not parse or that names
    /// another trunk than the bootstrap, git that cannot be read).
    Known(PathBuf, String),
    /// No bootstrap, or no config at the trunk tip: the refusal's detail.
    ConfigMissing(PathBuf, Value),
}

impl OpenError {
    /// This error as a command's result: the `CONFIG_MISSING` refusal, or a failure.
    #[must_use]
    pub fn into_result(self) -> BayResult {
        match self {
            Self::ConfigMissing(_, detail) => {
                BayResult::blocked(Reason::ConfigMissing).with_detail(detail)
            }
            Self::NoRepository(message) | Self::Known(_, message) => BayResult::failed(message),
        }
    }

    /// This error as a message; a `CONFIG_MISSING` one starts with its code.
    #[must_use]
    pub fn message(&self) -> String {
        let coded = |reason: Reason, detail: &Value| {
            format!(
                "{}: {}",
                reason.code(),
                detail["message"].as_str().unwrap_or_default()
            )
        };
        match self {
            Self::NoRepository(message) | Self::Known(_, message) => message.clone(),
            Self::ConfigMissing(_, detail) => coded(Reason::ConfigMissing, detail),
        }
    }
}

fn missing_bootstrap(missing: &[String]) -> Value {
    json!({
        "message": format!(
            "bay cannot tell this repository's trunk: neither {} names it; run bay install --main <name>, or git remote set-head <remote> --auto",
            missing.join(" nor ")
        ),
        "missing": missing,
        "fetched": false,
    })
}

/// `fetched`: whether the tracking ref exists (a fetch of trunk, or a push, wrote it).
fn missing_config(
    bootstrap: &Bootstrap,
    read: &str,
    fetched: bool,
    fetch_error: Option<String>,
) -> Value {
    let mut message = format!(
        "no {CONFIG_PATH} committed at {read} (trunk {} from {}); commit one to trunk: bay reads the config only there, never from a work tree",
        bootstrap.trunk, bootstrap.source
    );
    if let Some(error) = &fetch_error {
        message = format!("{message}; the fetch of trunk failed: {error}");
    }
    let mut detail = json!({
        "message": message,
        "bootstrap": bootstrap.source,
        "trunk": bootstrap.trunk,
        "ref": read,
        "fetched": fetched,
    });
    if let Some(error) = fetch_error {
        detail["fetch_error"] = json!(error);
    }
    detail
}

impl Repo {
    /// Opens the repository that contains `dir` (the main work tree, a linked worktree, or one
    /// of its views) without a fetch ([`Fetch::Never`]): the agent hook calls it on every tool
    /// call, and a command that needs the config has fetched trunk before its first read
    /// ([`Repo::config_missing`]).
    ///
    /// # Errors
    ///
    /// `dir` is not inside a work repository, its config does not parse, or there is no
    /// bootstrap or no config at the trunk tip (a message that starts `CONFIG_MISSING`).
    pub fn open(dir: &Path) -> Result<Self, String> {
        Self::open_with(dir, Fetch::Never).map_err(|error| error.message())
    }

    /// [`Repo::open`] for a command that fetches trunk: it fetches first, then reads the
    /// config at the tip it fetched. Offline, it reads the tracking ref as it is.
    ///
    /// # Errors
    ///
    /// As [`Repo::open_with`].
    pub fn open_fetched(dir: &Path) -> Result<Self, OpenError> {
        Self::open_with(dir, Fetch::First)
    }

    /// [`Repo::open`], whose error carries the common git directory when `dir` is in a
    /// repository bay cannot open, and `None` when no repository holds `dir`.
    ///
    /// # Errors
    ///
    /// As [`Repo::open`].
    pub fn open_known(dir: &Path) -> Result<Self, (Option<PathBuf>, String)> {
        Self::open_with(dir, Fetch::Never).map_err(|error| {
            let message = error.message();
            match error {
                OpenError::NoRepository(_) => (None, message),
                OpenError::Known(common, _) | OpenError::ConfigMissing(common, _) => {
                    (Some(common), message)
                }
            }
        })
    }

    /// The `CONFIG_MISSING` refusal for a command that needs the trunk or policy, when `dir`
    /// is in a work repository with no bootstrap or no config at the trunk tip. Before
    /// the first fetch it fetches trunk with the bootstrap's remote and trunk. `None` otherwise:
    /// the command opens the repository itself and reports any other fault.
    #[must_use]
    pub fn config_missing(dir: &Path) -> Option<BayResult> {
        match Self::open_with(dir, Fetch::IfMissing) {
            Err(error @ OpenError::ConfigMissing(..)) => Some(error.into_result()),
            _ => None,
        }
    }

    /// Opens the repository that contains `dir`, fetching trunk as `fetch` says.
    ///
    /// # Errors
    ///
    /// See [`OpenError`].
    #[allow(clippy::too_many_lines)]
    pub fn open_with(dir: &Path, fetch: Fetch) -> Result<Self, OpenError> {
        let here = Git::new(dir);
        let common = here
            .common_dir()
            .map_err(|error| OpenError::NoRepository(error.message()))?;
        let common_dir = PathBuf::from(common);
        let known = |message: String| OpenError::Known(common_dir.clone(), message);
        let work_root = if common_dir.file_name().is_some_and(|name| name == ".git") {
            common_dir
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| known("the git directory has no parent".to_owned()))?
        } else {
            return Err(known(format!(
                "{} is a bare repository; run bay in a work repository",
                common_dir.display()
            )));
        };
        let git = Git::new(&common_dir);
        let bootstrap = match Bootstrap::read(&git) {
            Ok(bootstrap) => bootstrap,
            Err(BootstrapError::Missing(missing)) => {
                return Err(OpenError::ConfigMissing(
                    common_dir.clone(),
                    missing_bootstrap(&missing),
                ));
            }
            Err(BootstrapError::Unreadable(message)) => return Err(known(message)),
        };
        let tracking = bootstrap.tracking_ref();
        let tip_of = |git: &Git| {
            git.ref_target(&format!("{tracking}^{{commit}}"))
                .map_err(|error| error.message())
        };
        let mut prefetched = None;
        let mut fetch_error = None;
        let mut have = tip_of(&git).map_err(known)?;
        if fetch == Fetch::First || (fetch == Fetch::IfMissing && have.is_none()) {
            let fetched = fetch_into(&git, &bootstrap, &work_root);
            if let Err(error) = &fetched {
                fetch_error = Some(error.message());
            }
            prefetched = Some(fetched);
            // G200: only a fetch moves the tracking ref here, so only after one is it read again.
            have = tip_of(&git).map_err(known)?;
        }
        let Some(tip) = have else {
            return Err(OpenError::ConfigMissing(
                common_dir.clone(),
                missing_config(&bootstrap, &tracking, false, fetch_error),
            ));
        };
        let text = committed_config(&git, &tip)
            .map_err(|message| known(format!("{tracking}:{CONFIG_PATH}: {message}")))?;
        let Some(text) = text else {
            return Err(OpenError::ConfigMissing(
                common_dir.clone(),
                missing_config(&bootstrap, &tracking, true, None),
            ));
        };
        let mut config = Config::parse(&text)
            .map_err(|message| known(format!("{tracking}:{CONFIG_PATH}: {message}")))?;
        // The config cannot move the trunk it was read from: a committed `trunk` or `remote`
        // other than the bootstrap's is refused, never followed.
        for (key, named, used) in [
            ("main", config.trunk.as_deref(), &bootstrap.trunk),
            ("remote", config.remote.as_deref(), &bootstrap.remote),
        ] {
            if let Some(named) = named
                && named != used
            {
                return Err(known(format!(
                    "{tracking}:{CONFIG_PATH} names {key} \"{named}\", but this repository's {key} is \"{used}\" (main from {}); run bay install --{key} {named}",
                    bootstrap.source
                )));
            }
        }
        config.trunk = Some(bootstrap.trunk.clone());
        config.remote = Some(bootstrap.remote.clone());
        Ok(Self {
            git,
            opened_in: dir.to_path_buf(),
            common_dir,
            work_root,
            bootstrap,
            config,
            fetched_at: Mutex::new(
                prefetched
                    .as_ref()
                    .is_some_and(Result::is_ok)
                    .then(crate::clock::now_rfc3339),
            ),
            prefetched: Mutex::new(prefetched),
        })
    }

    #[must_use]
    pub fn views_dir(&self) -> PathBuf {
        self.common_dir.join("bay").join("views")
    }

    #[must_use]
    pub fn view_path(&self, id: &str) -> PathBuf {
        self.views_dir().join(id)
    }

    #[must_use]
    pub fn remote(&self) -> &str {
        &self.bootstrap.remote
    }

    #[must_use]
    pub fn trunk(&self) -> &str {
        &self.bootstrap.trunk
    }

    /// The local tracking ref of the remote trunk.
    #[must_use]
    pub fn trunk_tracking_ref(&self) -> String {
        self.bootstrap.tracking_ref()
    }

    /// Fetches the remote trunk and returns its tip. A fetch that succeeds is recorded for the
    /// coordination cache (`crate::coord`), which the command writes when it finishes. The
    /// first call after [`Repo::open_fetched`] returns what that opening fetch found.
    ///
    /// # Errors
    ///
    /// The fetch fails or the remote has no trunk.
    pub fn fetch_trunk(&self) -> Result<String, GitError> {
        let opened = self.prefetched.lock().ok().and_then(|mut slot| slot.take());
        if let Some(opened) = opened {
            return opened;
        }
        let fetched = fetch_into(&self.git, &self.bootstrap, &self.work_root);
        if fetched.is_ok()
            && let Ok(mut at) = self.fetched_at.lock()
        {
            *at = Some(crate::clock::now_rfc3339());
        }
        fetched
    }

    /// The trunk a result reflects: the tracking ref's tip, and when this command fetched it,
    /// or that it read the local cache without a fetch (G80, `INTERFACES` § 1). `None` before
    /// the first fetch, when there is no tracking ref.
    #[must_use]
    pub fn freshness(&self) -> Option<crate::result::Freshness> {
        let trunk = self
            .git
            .ref_target(&self.bootstrap.tracking_ref())
            .ok()
            .flatten()?;
        let fetched_at = self
            .fetched_at
            .lock()
            .ok()
            .and_then(|at| at.clone())
            .unwrap_or_else(|| crate::status::CACHED.to_owned());
        Some(crate::result::Freshness { trunk, fetched_at })
    }

    /// Fetches `refs/bay/registry` when the remote has one. A remote without a registry leaves
    /// the local one as it is.
    pub fn fetch_registry(&self) {
        let name = crate::registry::REGISTRY_REF;
        let _ = self.git.run(&[
            "fetch",
            "-q",
            "--no-tags",
            self.remote(),
            &format!("+{name}:{name}"),
        ]);
    }

    /// Fetches every other writer's evidence notes ref from the remote, so an operation
    /// resumed on this machine sees the records its earlier attempts wrote elsewhere (§ 7).
    /// The refs of `keys`' own names (its producer and its reviewer) are never fetched: only
    /// this machine writes them, a record written here whose push failed would be lost, and a
    /// remote copy this machine did not write is a collision its push reports. Best effort:
    /// offline, the local records are what this machine can see.
    pub fn fetch_evidence(&self, keys: &crate::signing::Keys) {
        let own: Vec<String> = keys
            .own_names()
            .into_iter()
            .map(crate::evidence::notes_ref)
            .collect();
        let pattern = format!("{}*", crate::evidence::EVIDENCE_PREFIX);
        let Ok(listed) = self.git.run(&["ls-remote", self.remote(), &pattern]) else {
            return;
        };
        let refspecs: Vec<String> = listed
            .lines()
            .filter_map(|line| line.split_whitespace().nth(1))
            .filter(|name| !own.iter().any(|own| own == name))
            .map(|name| format!("+{name}:{name}"))
            .collect();
        if refspecs.is_empty() {
            return;
        }
        let mut args = vec!["fetch", "-q", "--no-tags", self.remote()];
        args.extend(refspecs.iter().map(String::as_str));
        let _ = self.git.run(&args);
    }

    /// Fetches `refs` (full ref names) from the remote into the same names. A ref the
    /// remote lacks is an error.
    ///
    /// # Errors
    ///
    /// The fetch fails.
    pub fn fetch_refs(&self, refs: &[String]) -> Result<(), GitError> {
        let mut args = vec!["fetch", "-q", "--no-tags", self.remote()];
        let refspecs: Vec<String> = refs.iter().map(|name| format!("+{name}:{name}")).collect();
        args.extend(refspecs.iter().map(String::as_str));
        self.git.run(&args).map(|_| ())
    }

    /// The remote's head of `change`'s chain, read by the one rule every reader of a remote
    /// chain follows (G199, G289, G294): this store's chain ref is never moved back, and no
    /// commit it holds is lost.
    ///
    /// - A fast-forward fetch into the chain ref is tried first: it creates a chain this store
    ///   lacks, and moves one the remote's head descends from.
    /// - When git refuses it, the remote's head is fetched into `refs/bay/fetch/<id>`, read and
    ///   the ref removed again; `FETCH_HEAD` is never read, since a fetch that fails can leave it
    ///   holding another change's chain. The local chain is then created at the remote's head,
    ///   or moved to it only when it descends from the local head, each by compare-and-swap.
    /// - A local chain ahead of the remote's (the remote's head is its ancestor) is kept where
    ///   it is. Its commits are work not yet pushed, such as an offline request, and a push
    ///   takes them on as a fast-forward: its writer's, or `receipts::sync`'s.
    /// - A local chain apart from the remote's (neither descends from the other) can never be
    ///   pushed without force, and no bay process forces one: a process left it that died
    ///   between its write and its push, or offline work the remote moved on from. It is set
    ///   aside under `refs/bay/kept/<id>/<sha>` first, then the chain ref moves to the
    ///   remote's head by compare-and-swap: nothing is lost, and no compare-and-swap on the
    ///   ref keeps losing to it (G294). A live process never leaves its own lost write there
    ///   to be set aside: it takes it back itself (`publish::Ctx`).
    /// - A retention cut (§ 4), the remote's one rewrite of a chain, made only once the change
    ///   closed and its window passed, replaces the local chain, again by compare-and-swap on
    ///   the head read.
    ///
    /// No caller forces a chain ref over its local copy, and the executor never does either:
    /// it goes on from the remote's head. The sync at the start of each executor pass pushes
    /// local work ahead of the remote first.
    ///
    /// # Errors
    ///
    /// The remote cannot be read, or lacks the chain.
    pub fn remote_chain(&self, change: &str) -> Result<String, GitError> {
        let git = &self.git;
        let name = crate::change::change_ref(change);
        if git
            .run(&[
                "fetch",
                "-q",
                "--no-tags",
                self.remote(),
                &format!("{name}:{name}"),
            ])
            .is_ok()
            && let Some(head) = git.ref_target(&name)?
        {
            return Ok(head);
        }
        let fetched = format!("{}{change}", crate::names::FETCH);
        git.run(&[
            "fetch",
            "-q",
            "--no-tags",
            self.remote(),
            &format!("+{name}:{fetched}"),
        ])?;
        let remote = git.run(&["rev-parse", "--verify", &fetched])?;
        let _ = git.run(&["update-ref", "-d", &fetched, &remote]);
        match git.ref_target(&name)? {
            None => {
                let _ = git.run(&["update-ref", &name, &remote, ""]);
            }
            Some(local) if local == remote => {}
            Some(local)
                if git.is_ancestor(&local, &remote).unwrap_or(false)
                    || crate::change::Chain::new(git)
                        .record_at(&remote)
                        .is_ok_and(|record| record.retention_cut && record.id == change) =>
            {
                let _ = git.run(&["update-ref", &name, &remote, &local]);
            }
            // Ahead of the remote's, or not readable: kept where it is.
            Some(local) if git.is_ancestor(&remote, &local).unwrap_or(true) => {}
            // Apart: set aside first, so moving the chain ref never drops it.
            Some(local) => {
                let kept = format!("{}{change}/{local}", crate::names::KEPT);
                if git.run(&["update-ref", &kept, &local]).is_ok() {
                    let _ = git.run(&["update-ref", &name, &remote, &local]);
                }
            }
        }
        Ok(remote)
    }

    /// [`Repo::remote_chain`] and the record at the remote's head: the remote's view of the
    /// change, whatever this store's chain ref keeps.
    ///
    /// # Errors
    ///
    /// The remote cannot be read, lacks the chain, or its record does not parse.
    pub fn remote_record(&self, change: &str) -> Result<RemoteRecord, String> {
        let head = self.remote_chain(change).map_err(|error| error.message())?;
        let record = crate::change::Chain::new(&self.git)
            .record_at(&head)
            .map_err(|error| error.message())?;
        Ok(RemoteRecord { head, record })
    }

    /// Every change chain of the remote, read by [`Repo::remote_chain`]'s rule in one batch:
    /// one fast-forward fetch of them all. Only when git refuses some ref is each chain whose
    /// local head differs from the remote's read on its own, so a batch the remote and this
    /// store agree on costs one fetch, however many chains there are (G200b).
    ///
    /// # Errors
    ///
    /// The remote cannot be read.
    pub fn fetch_chains(&self) -> Result<(), GitError> {
        let prefix = crate::change::CHANGES_PREFIX;
        let Err(refused) = self.git.run(&[
            "fetch",
            "-q",
            "--no-tags",
            self.remote(),
            &format!("{prefix}*:{prefix}*"),
        ]) else {
            return Ok(());
        };
        let Ok(listed) = self
            .git
            .run(&["ls-remote", self.remote(), &format!("{prefix}*")])
        else {
            return Err(refused);
        };
        let local = self
            .git
            .run(&["for-each-ref", "--format=%(objectname) %(refname)", prefix])?;
        for (head, name) in listed.lines().filter_map(|line| line.split_once('\t')) {
            let Some(change) = name.strip_prefix(prefix) else {
                continue;
            };
            let same = local
                .lines()
                .any(|line| line.split_once(' ') == Some((head, name)));
            if !same {
                let _ = self.remote_chain(change);
            }
        }
        Ok(())
    }

    /// The view directory's change id, when `dir` is one of this repository's views.
    #[must_use]
    pub fn view_change(&self, dir: &Path) -> Option<String> {
        let toplevel = Git::new(dir).run(&["rev-parse", "--show-toplevel"]).ok()?;
        let toplevel = PathBuf::from(toplevel).canonicalize().ok()?;
        let views = self.views_dir().canonicalize().ok()?;
        (toplevel.parent()? == views).then(|| toplevel.file_name()?.to_str().map(str::to_owned))?
    }
}

/// Fetches the trunk `bootstrap` names into its tracking ref and returns its tip.
fn fetch_into(git: &Git, bootstrap: &Bootstrap, work_root: &Path) -> Result<String, GitError> {
    let tracking = bootstrap.tracking_ref();
    let refspec = format!("+refs/heads/{}:{tracking}", bootstrap.trunk);
    git.run(&["fetch", "-q", "--no-tags", &bootstrap.remote, &refspec])?;
    crate::coord::fetched(work_root);
    git.run(&["rev-parse", "--verify", &tracking])
}

/// The config text committed at `commit`; `None` when that commit's tree has no
/// `.bay/config.toml`. A tree git cannot read is an error, never "no config".
fn committed_config(git: &Git, commit: &str) -> Result<Option<String>, String> {
    // Read once per process (plan B2), shared with `Config::at` at the same commit.
    crate::memo::at(
        crate::config::COMMITTED,
        git,
        commit,
        Result::is_ok,
        || -> Result<Option<String>, String> {
            let listed = git
                .run(&["ls-tree", "-z", "--full-tree", commit, "--", CONFIG_PATH])
                .map_err(|error| error.message())?;
            let entry = listed.trim_end_matches('\0');
            if entry.is_empty() {
                return Ok(None);
            }
            let Some(("blob", blob)) = entry
                .split('\t')
                .next()
                .and_then(|head| head.split_once(' ').map(|(_, rest)| rest))
                .and_then(|rest| rest.split_once(' '))
            else {
                return Err(format!("{commit}:{CONFIG_PATH} is not a file"));
            };
            git.run(&["cat-file", "blob", blob])
                .map(Some)
                .map_err(|error| error.message())
        },
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Fetch, Repo};

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t.invalid"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn write_config(dir: &Path, text: &str) {
        std::fs::create_dir_all(dir.join(".bay")).unwrap();
        std::fs::write(dir.join(".bay/config.toml"), text).unwrap();
    }

    const TRUNK: &str =
        "[policy]\nevidence_reuse = \"exact-inputs\"\n\n[runner]\ncommand = \"trunk-executor\"\n";
    const DRAFT: &str = "main = \"decoy\"\n\n[policy]\nevidence_reuse = \"off\"\n\n[runner]\ncommand = \"draft-executor\"\n";

    #[test]
    fn every_setting_is_the_trunks_never_a_drafts() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        write_config(&root, TRUNK);
        git(&root, &["add", ".bay"]);
        git(&root, &["commit", "-q", "-m", "trunk"]);
        git(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        let head = [
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ];
        git(&root, &head);
        // A draft on the main work tree's disk, and one committed in a change's own branch.
        write_config(&root, DRAFT);
        let change = dir.path().join("change");
        let path = change.to_str().unwrap();
        git(&root, &["worktree", "add", "-q", "-b", "change", path]);
        write_config(&change, DRAFT);
        git(&change, &["commit", "-q", "-am", "draft"]);
        write_config(&change, "main = \"decoy-on-disk\"\n");

        for dir in [&root, &change] {
            let repo = Repo::open_with(dir, Fetch::Never)
                .unwrap_or_else(|error| panic!("{}: {}", dir.display(), error.message()));
            assert_eq!(repo.trunk(), "main");
            assert_eq!(repo.config.trunk(), "main");
            assert_eq!(repo.trunk_tracking_ref(), "refs/remotes/origin/main");
            assert!(repo.config.evidence_reuse(), "{}", dir.display());
            let command = repo
                .config
                .executor
                .as_ref()
                .and_then(|executor| executor.command.as_deref());
            assert_eq!(command, Some("trunk-executor"), "{}", dir.display());
        }

        // The repository's own git config names the trunk before the remote's default branch.
        git(
            &root,
            &["update-ref", "refs/remotes/origin/release", "HEAD~0"],
        );
        git(&root, &["config", "bay.main", "release"]);
        let repo = Repo::open_with(&change, Fetch::Never)
            .unwrap_or_else(|error| panic!("{}", error.message()));
        assert_eq!(repo.trunk_tracking_ref(), "refs/remotes/origin/release");
        assert_eq!(repo.bootstrap.source, "git config bay.main");
    }
}
