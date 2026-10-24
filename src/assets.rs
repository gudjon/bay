//! Large files outside Git (BACKLOG 49 phase 1, owner ruling 2026-10-21): `[plugin.assets]`.
//!
//! With `[plugin.assets] command` and `over_limit = "offload"` in `.bay/config.toml`, a new file
//! over the policy's `[limits] max_file_mib` never enters Git: `bay save` in the main checkout,
//! and `bay merge` and every checkpoint of a view, hand it to the assets plugin and write one line
//! for it in the manifest, `.bay-assets` at the repository's root, so the change carries the
//! file's identity and not its bytes. A file Git already tracks is never offloaded, and without
//! `over_limit = "offload"` a file over the limit is refused `CHANGE_TOO_LARGE`, as before.
//!
//! The manifest holds one line per file, sorted by path: `sha256:<64 hex> <size in bytes>
//! <path>`. The identity is the SHA-256 of the file's raw bytes, so what merges names exactly
//! the bytes that were saved, a revert of the line is an undo, and one file saved twice is
//! stored once.
//!
//! The plugin is a program path relative to the work tree, then its arguments (`crate::run`),
//! run with only `PATH` and `HOME` of bay's environment, within `[plugin.assets] timeout_s`
//! ([`DEFAULT_TIMEOUT_S`] unless set):
//! - `<command> put <file>` stores the file's bytes and prints their identity, `sha256:<hex>`,
//!   on stdout, exit 0. Storing bytes it already holds is not an error.
//! - `<command> get <identity> <file>` writes exactly those bytes to `<file>`, exit 0; an
//!   identity it does not hold exits non-zero.
//!
//! bay hashes every file itself: a `put` that prints another identity, or a `get` whose bytes
//! hash to another one, is refused, never trusted. A `put` that fails refuses the save before
//! anything is made (unknown is never a pass). The saved file stays in the work tree, and the
//! clone's own `info/exclude` names it, so Git leaves it alone there.
//!
//! The bytes come back wherever the tree is used (phase 2): a view gets each manifest file its
//! declared paths cover when it is made; the main checkout gets the ones it lacks at `bay
//! install` and when `bay save` moves it to a merge; a check gets the ones it sees, read-only,
//! with the plugin's program taken from the candidate's base. Each is hashed before it is used:
//! a `get` whose bytes are not the manifest's is refused, and a check whose files cannot all be
//! fetched is an `infra_failure`, never a pass. A file already there with the manifest's size is
//! kept as it is.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use crate::git::Git;
use crate::repo::Repo;

use sha2::{Digest, Sha256};

/// The manifest, at the repository's root.
pub const MANIFEST: &str = ".bay-assets";

/// How long one plugin call may take when `[plugin.assets] timeout_s` is not set.
pub const DEFAULT_TIMEOUT_S: u64 = 300;

/// `[assets]` in `.bay/policy.toml` (BACKLOG 49, owner ruling 2026-10-24): per-path retention.
/// `keep = [{ paths = "renders/", days = 30 }]` says the store may purge the bytes of a file
/// under `paths` once `days` have passed since it was saved (the address stays, so history and
/// its signatures stay whole). bay validates the line; the store reads it from main's policy and
/// applies it. bay never purges.
#[derive(Clone, Debug, Default, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AssetsPolicy {
    #[serde(default)]
    pub keep: Vec<Keep>,
}

/// One retention line.
#[derive(Clone, Debug, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Keep {
    /// A glob of the paths it covers (`crate::policy::glob_matches`).
    pub paths: String,
    /// How long the store keeps their bytes, in days.
    pub days: u64,
}

impl AssetsPolicy {
    /// A validity rule of the policy (`POLICY_INVALID`): each line names paths and at least a day.
    ///
    /// # Errors
    ///
    /// Why the table is invalid.
    pub fn validate(&self) -> Result<(), String> {
        for keep in &self.keep {
            if keep.paths.trim().is_empty() {
                return Err("[assets] keep has a line with no paths".to_owned());
            }
            if keep.days == 0 {
                return Err(format!(
                    "[assets] keep for {} says 0 days: a store keeps bytes at least a day",
                    keep.paths
                ));
            }
        }
        Ok(())
    }
}

/// One manifest line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// `sha256:<64 lower-case hex>`.
    pub id: String,
    pub size: u64,
    pub path: String,
}

impl Entry {
    fn line(&self) -> String {
        format!("{} {} {}", self.id, self.size, self.path)
    }
}

/// The entries of manifest `text`, in its order.
///
/// # Errors
///
/// A line that is not `sha256:<hex> <size> <path>`.
pub fn parse(text: &str) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let mut words = line.splitn(3, ' ');
        let (Some(id), Some(size), Some(path)) = (words.next(), words.next(), words.next()) else {
            return Err(format!(
                "{MANIFEST} line {} is not `sha256:<hex> <size> <path>`",
                number + 1
            ));
        };
        if !is_id(id) {
            return Err(format!(
                "{MANIFEST} line {}: {id} is no sha256:<hex> identity",
                number + 1
            ));
        }
        let size = size
            .parse()
            .map_err(|_| format!("{MANIFEST} line {}: {size} is no size in bytes", number + 1))?;
        entries.push(Entry {
            id: id.to_owned(),
            size,
            path: path.to_owned(),
        });
    }
    Ok(entries)
}

/// `entries` as the manifest's text: one line each, sorted by path.
#[must_use]
pub fn text(entries: &[Entry]) -> String {
    let mut sorted: Vec<&Entry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    sorted.iter().fold(String::new(), |mut text, entry| {
        let _ = writeln!(text, "{}", entry.line());
        text
    })
}

/// The three-way merge of manifest texts, path by path: a path both sides left as `base` had
/// it stays, one side's change is taken, both sides' same change once; `None` when one path
/// was changed differently on the two sides, or a text does not parse.
#[must_use]
pub fn merge3(base: &str, ours: &str, theirs: &str) -> Option<String> {
    let read = |text: &str| -> Option<std::collections::BTreeMap<String, Entry>> {
        Some(
            parse(text)
                .ok()?
                .into_iter()
                .map(|entry| (entry.path.clone(), entry))
                .collect(),
        )
    };
    let (base, ours, theirs) = (read(base)?, read(ours)?, read(theirs)?);
    let mut paths: Vec<&String> = base
        .keys()
        .chain(ours.keys())
        .chain(theirs.keys())
        .collect();
    paths.sort();
    paths.dedup();
    let mut merged = Vec::new();
    for path in paths {
        let (b, o, t) = (base.get(path), ours.get(path), theirs.get(path));
        let kept = if o == t || t == b {
            o
        } else if o == b {
            t
        } else {
            return None;
        };
        merged.extend(kept.cloned());
    }
    Some(text(&merged))
}

/// Whether `id` is `sha256:` and 64 lower-case hex digits.
#[must_use]
pub fn is_id(id: &str) -> bool {
    id.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// The identity of the bytes of `file`.
///
/// # Errors
///
/// The file cannot be read.
pub fn identity(file: &Path) -> Result<String, String> {
    use std::io::Read as _;
    let unreadable = |error: std::io::Error| format!("{} cannot be read: {error}", file.display());
    let mut hasher = Sha256::new();
    let mut reader = std::fs::File::open(file).map_err(unreadable)?;
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let read = reader.read(&mut buffer).map_err(unreadable)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!(
        "sha256:{}",
        crate::agent_settings::hex(&hasher.finalize())
    ))
}

/// The assets plugin, ready to run from a work tree.
pub struct Plugin<'a> {
    pub work_root: &'a Path,
    pub command: &'a str,
    pub timeout: Duration,
}

impl Plugin<'_> {
    /// Runs `<command> <args>`: its exit code, its stdout (trimmed) and its stderr's first line.
    fn exec(&self, args: &[&std::ffi::OsStr]) -> Result<(i32, String, String), String> {
        let mut cmd = crate::run::command(self.work_root, self.command)
            .ok_or("[plugin.assets] command is empty")??;
        cmd.args(args);
        let ran = crate::run::captured(cmd, None, self.timeout)
            .map_err(|fault| format!("the assets plugin failed: {fault}"))?;
        let said = String::from_utf8_lossy(&ran.stderr);
        Ok((
            ran.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&ran.stdout).trim().to_owned(),
            said.lines().next().unwrap_or("").trim().to_owned(),
        ))
    }

    /// Runs `<command> <args>`; its stdout, trimmed. A non-zero exit is an error.
    fn run(&self, args: &[&std::ffi::OsStr]) -> Result<String, String> {
        let (code, stdout, said) = self.exec(args)?;
        if code != 0 {
            return Err(format!("the assets plugin exited {code}: {said}"));
        }
        Ok(stdout)
    }

    /// Stores `file` and returns its identity, as bay hashed it.
    ///
    /// # Errors
    ///
    /// The plugin fails, or names another identity than the file's bytes have.
    pub fn put(&self, file: &Path) -> Result<String, String> {
        let id = identity(file)?;
        let said = self.run(&["put".as_ref(), file.as_os_str()])?;
        if said != id {
            return Err(format!(
                "the assets plugin named {said:?} for {}, whose bytes are {id}",
                file.display()
            ));
        }
        Ok(id)
    }

    /// Writes the bytes of `id` to `file`; [`Got::Purged`] when the store purged them, the
    /// address kept ([`EXIT_PURGED`]), and nothing is written.
    ///
    /// # Errors
    ///
    /// The plugin fails, or the bytes it wrote are not `id`'s.
    pub fn get(&self, id: &str, file: &Path) -> Result<Got, String> {
        let (code, _, said) = self.exec(&["get".as_ref(), id.as_ref(), file.as_os_str()])?;
        match code {
            0 => {}
            EXIT_PURGED => {
                let _ = std::fs::remove_file(file);
                return Ok(Got::Purged);
            }
            _ => return Err(format!("the assets plugin exited {code}: {said}")),
        }
        let got = identity(file)?;
        if got != id {
            let _ = std::fs::remove_file(file);
            return Err(format!(
                "the assets plugin wrote bytes {got} for {id}; they were removed"
            ));
        }
        Ok(Got::Bytes)
    }

    /// Whether the store holds the bytes of `id` (durability before a merge): exit 0 is yes,
    /// [`EXIT_ABSENT`] no.
    ///
    /// # Errors
    ///
    /// The plugin fails, or answers another code: unknown is never a yes.
    pub fn exists(&self, id: &str) -> Result<bool, String> {
        let (code, _, said) = self.exec(&["exists".as_ref(), id.as_ref()])?;
        match code {
            0 => Ok(true),
            EXIT_ABSENT | EXIT_PURGED => Ok(false),
            _ => Err(format!(
                "the assets plugin exited {code} for exists {id}: {said}"
            )),
        }
    }

    /// An optional action (`purge`, a range `get`): `None` when the plugin does not offer it
    /// ([`EXIT_UNSUPPORTED`]), else whether it succeeded.
    fn optional(&self, args: &[&std::ffi::OsStr]) -> Result<Option<String>, String> {
        let (code, stdout, said) = self.exec(args)?;
        match code {
            0 => Ok(Some(stdout)),
            EXIT_UNSUPPORTED => Ok(None),
            _ => Err(format!("the assets plugin exited {code}: {said}")),
        }
    }

    /// Purges the bytes of `id`, keeping its address: a later `get` answers [`EXIT_PURGED`].
    /// bay never purges on its own; this is the store operator's action, held to the contract by
    /// `bay plugin test assets`. `None` when the plugin does not offer it.
    ///
    /// # Errors
    ///
    /// The plugin fails.
    pub fn purge(&self, id: &str) -> Result<Option<()>, String> {
        Ok(self.optional(&["purge".as_ref(), id.as_ref()])?.map(|_| ()))
    }

    /// Writes bytes `start..end` of `id` to `file` (a host's preview; bay itself reads whole
    /// files). `None` when the plugin does not offer ranges.
    ///
    /// # Errors
    ///
    /// The plugin fails.
    pub fn get_range(
        &self,
        id: &str,
        file: &Path,
        start: u64,
        end: u64,
    ) -> Result<Option<()>, String> {
        let (start, end) = (start.to_string(), end.to_string());
        Ok(self
            .optional(&[
                "get".as_ref(),
                id.as_ref(),
                file.as_os_str(),
                start.as_ref(),
                end.as_ref(),
            ])?
            .map(|_| ()))
    }
}

/// What a `get` brought back.
#[derive(Debug, PartialEq, Eq)]
pub enum Got {
    /// The bytes, hashed.
    Bytes,
    /// The store purged them; their address stays in the manifest.
    Purged,
}

/// `exists`: the store does not hold the bytes.
pub const EXIT_ABSENT: i32 = 1;
/// An optional action the plugin does not offer (`purge`, a range `get`).
pub const EXIT_UNSUPPORTED: i32 = 2;
/// `get` (and `exists`) of bytes the store purged, the address kept.
pub const EXIT_PURGED: i32 = 3;

/// The assets plugin `config` names, run from `work_root`, whatever `over_limit` says: getting
/// bytes back needs only the plugin.
#[must_use]
pub fn plugin_of<'a>(config: &'a crate::config::Config, work_root: &'a Path) -> Option<Plugin<'a>> {
    config.plugin.assets.as_ref().map(|assets| Plugin {
        work_root,
        command: &assets.command,
        timeout: Duration::from_secs(assets.timeout_s.unwrap_or(DEFAULT_TIMEOUT_S)),
    })
}

/// The manifest of `commit`, empty when it has none.
///
/// # Errors
///
/// The manifest cannot be read or parsed.
pub fn manifest_at(git: &Git, commit: &str) -> Result<Vec<Entry>, String> {
    match git
        .file_at(commit, MANIFEST)
        .map_err(|error| error.message())?
    {
        Some(text) => parse(&text),
        None => Ok(Vec::new()),
    }
}

/// Writes into `root` each file of `commit`'s manifest that `covered` takes and `root` lacks
/// (a file there with the manifest's size is kept), through `plugin`'s `get`, hashed before it
/// is moved into place; names each in `exclude` when given. Returns the paths written.
///
/// # Errors
///
/// The manifest cannot be read, or a `get` fails or writes other bytes: nothing of that file
/// is left behind.
pub fn fill(
    plugin: &Plugin<'_>,
    git: &Git,
    commit: &str,
    root: &Path,
    covered: &dyn Fn(&str) -> bool,
    exclude: Option<&Path>,
) -> Result<Filled, String> {
    let wanted: Vec<Entry> = manifest_at(git, commit)?
        .into_iter()
        .filter(|entry| covered(&entry.path))
        .collect();
    let mut filled = Filled::default();
    for entry in &wanted {
        let file = root.join(&entry.path);
        if std::fs::metadata(&file).is_ok_and(|meta| meta.is_file() && meta.len() == entry.size) {
            continue;
        }
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|error| format!("{} cannot be made: {error}", dir.display()))?;
        }
        let name = file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("asset");
        let tmp = file.with_file_name(format!(".{name}.{}.get", std::process::id()));
        match plugin.get(&entry.id, &tmp) {
            Ok(Got::Bytes) => {}
            // A takedown: the address stays in the manifest, and the file is named, not made.
            Ok(Got::Purged) => {
                filled.purged.push(entry.path.clone());
                continue;
            }
            Err(why) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("{} could not be fetched: {why}", entry.path));
            }
        }
        crate::durable::rename(&tmp, &file).map_err(|error| {
            let _ = std::fs::remove_file(&tmp);
            format!("{} cannot be written: {error}", file.display())
        })?;
        filled.written.push(entry.path.clone());
    }
    if let Some(exclude) = exclude {
        exclude_paths(exclude, &wanted)?;
    }
    Ok(filled)
}

/// Durability before a merge (BACKLOG 49, owner ruling 2026-10-24): the manifest lines `tree`
/// adds to `trunk`'s whose bytes the store does not hold, asked with `exists`, through the
/// plugin of `repo`'s config. A tree that adds lines while the config names no plugin holds
/// none of them.
///
/// # Errors
///
/// A manifest cannot be read, or the plugin cannot answer: unknown is never held.
pub fn not_held_at_merge(repo: &Repo, trunk: &str, tree: &str) -> Result<Vec<String>, String> {
    let before = manifest_at(&repo.git, trunk)?;
    let added: Vec<Entry> = manifest_at(&repo.git, tree)?
        .into_iter()
        .filter(|entry| !before.contains(entry))
        .collect();
    if added.is_empty() {
        return Ok(Vec::new());
    }
    let Some(plugin) = plugin_of(&repo.config, &repo.work_root) else {
        return Ok(added.into_iter().map(|entry| entry.path).collect());
    };
    let mut missing = Vec::new();
    for entry in added {
        if !plugin.exists(&entry.id)? {
            missing.push(entry.path);
        }
    }
    Ok(missing)
}

/// What [`fill`] did: the files it wrote, and those the store purged (named, never written).
#[derive(Debug, Default)]
pub struct Filled {
    pub written: Vec<String>,
    pub purged: Vec<String>,
}

/// [`fill`] for one of `repo`'s own trees (its main checkout or a view): the plugin of its
/// config, run from its main checkout, the files `covered` takes, named in the clone's
/// `info/exclude`. A config with no assets plugin, or a commit with no manifest, writes nothing.
///
/// # Errors
///
/// As [`fill`].
pub fn fill_tree(
    repo: &Repo,
    root: &Path,
    commit: &str,
    covered: &dyn Fn(&str) -> bool,
) -> Result<Filled, String> {
    let Some(plugin) = plugin_of(&repo.config, &repo.work_root) else {
        return Ok(Filled::default());
    };
    fill(
        &plugin,
        &repo.git,
        commit,
        root,
        covered,
        Some(&exclude_file(repo)),
    )
}

/// A check's files (phase 2): each manifest file of `commit` that `kept` covers (all, when it
/// is `None`) written into the check's `tree_dir`, through the assets plugin the config at
/// `base` names, its program materialized from `base` into a scratch directory beside
/// `tree_dir`, as the hold plugin's is. The check sees them read-only, with the rest of its tree.
///
/// # Errors
///
/// The config or manifest cannot be read, the plugin cannot be materialized, or a `get` fails.
pub fn fill_check(
    git: &Git,
    commit: &str,
    base: &str,
    tree_dir: &Path,
    kept: Option<&[String]>,
    deadline: std::time::Instant,
) -> Result<(), String> {
    let covered = |path: &str| {
        kept.is_none_or(|kept| kept.iter().any(|outer| crate::inputs::within(outer, path)))
    };
    if !manifest_at(git, commit)?
        .iter()
        .any(|entry| covered(&entry.path))
    {
        return Ok(());
    }
    let config = crate::config::Config::at(git, base)?;
    let Some(assets) = config.plugin.assets.as_ref() else {
        return Err(format!(
            "the candidate's {MANIFEST} names files, and the config at its base names no [plugin.assets] to fetch them"
        ));
    };
    let program = assets
        .command
        .split_whitespace()
        .next()
        .ok_or("[plugin.assets] command is empty")?
        .to_owned();
    let scratch = tree_dir.with_extension("assets-plugin");
    let _ = std::fs::remove_dir_all(&scratch);
    crate::materialize::materialize(git, base, Some(&[program]), &scratch, deadline, &mut || {})
        .map_err(|unmade| format!("the assets plugin could not be materialized: {unmade:?}"))?;
    let plugin = Plugin {
        work_root: &scratch,
        command: &assets.command,
        timeout: Duration::from_secs(assets.timeout_s.unwrap_or(DEFAULT_TIMEOUT_S)),
    };
    let filled = fill(&plugin, git, commit, tree_dir, &covered, None);
    let _ = std::fs::remove_dir_all(&scratch);
    // A purged file (a takedown) is left out of the check's tree, its line kept in `.bay-assets`
    // (owner ruling 2026-10-24): one purge never blocks every later merge. A change's own new
    // files are never among them: the store must hold them before it merges (`exists`).
    let filled = filled?;
    for path in filled.written {
        let file = tree_dir.join(&path);
        if let Ok(meta) = std::fs::metadata(&file) {
            let mut permissions = meta.permissions();
            permissions.set_readonly(true);
            let _ = std::fs::set_permissions(&file, permissions);
        }
    }
    Ok(())
}

/// The assets plugin of `repo`'s config and which new files it stores ([`Rule`]): those over
/// the policy's file limit at `trunk` (`over_limit = "offload"`), and those its `offload` globs
/// name; `None` when it stores none.
///
/// # Errors
///
/// The policy cannot be read.
pub fn configured<'a>(
    repo: &'a Repo,
    trunk: &str,
) -> Result<Option<(Plugin<'a>, Rule<'a>)>, String> {
    let Some(assets) = repo.config.plugin.assets.as_ref() else {
        return Ok(None);
    };
    let limit_bytes = if assets.over_limit == crate::config::OverLimit::Offload {
        crate::policy::limits_at(&repo.git, trunk)?
            .max_file_mib
            .map(|mib| mib.saturating_mul(1024 * 1024))
    } else {
        None
    };
    if limit_bytes.is_none() && assets.offload.is_empty() {
        return Ok(None);
    }
    let plugin = Plugin {
        work_root: &repo.work_root,
        command: &assets.command,
        timeout: Duration::from_secs(assets.timeout_s.unwrap_or(DEFAULT_TIMEOUT_S)),
    };
    Ok(Some((
        plugin,
        Rule {
            limit_bytes,
            globs: &assets.offload,
        },
    )))
}

/// Which new files the plugin stores: those over the limit (`over_limit = "offload"` with
/// `[limits] max_file_mib`), and those an `offload` glob names, whatever their size (BACKLOG 62).
#[derive(Clone, Copy, Debug)]
pub struct Rule<'a> {
    pub limit_bytes: Option<u64>,
    pub globs: &'a [String],
}

impl Rule<'_> {
    /// Whether the new file at `path`, of `size` bytes, is stored by the plugin.
    #[must_use]
    pub fn offloads(&self, path: &str, size: u64) -> bool {
        self.limit_bytes.is_some_and(|limit| size > limit)
            || self
                .globs
                .iter()
                .any(|glob| crate::policy::glob_matches(glob, path))
    }
}

/// The clone's `info/exclude`, shared by its main checkout and its views.
#[must_use]
pub fn exclude_file(repo: &Repo) -> std::path::PathBuf {
    repo.common_dir.join("info").join("exclude")
}

/// Before a view is frozen (`bay merge`, a checkpoint): offloads each file the view adds that
/// Git does not track yet and that is over the policy's limit at main's tracking ref, so the
/// freeze, which adds the view's files, writes the manifest and leaves the file out. Returns
/// the offloaded paths.
///
/// # Errors
///
/// The view's files cannot be listed, the policy cannot be read, or a put fails.
pub fn offload_view(repo: &Repo, view: &Path) -> Result<Vec<String>, String> {
    let Some((plugin, rule)) = configured(repo, &repo.trunk_tracking_ref())? else {
        return Ok(Vec::new());
    };
    let listed = Git::new(view)
        .run_bytes(
            &[
                "--no-optional-locks",
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
            ],
            &[],
            None,
        )
        .map_err(|error| error.message())?;
    let text = String::from_utf8_lossy(&listed);
    let new: Vec<String> = text
        .split('\0')
        .filter_map(|entry| entry.strip_prefix("?? "))
        .map(str::to_owned)
        .collect();
    let done = offload(&plugin, view, &new, rule, &exclude_file(repo))?;
    // The freeze that follows takes the manifest from the view's folder.
    if let Some(undo) = &done.undo {
        undo.keep();
    }
    Ok(done.entries.into_iter().map(|entry| entry.path).collect())
}

/// What [`offload`] did to a save.
#[derive(Debug, Default)]
pub struct Offloaded {
    /// The paths whose bytes went to the plugin, each with its manifest entry.
    pub entries: Vec<Entry>,
    /// The manifest and `info/exclude` as they were before, to put back when the save is then
    /// refused, so a refusal leaves the folder as it found it.
    pub undo: Option<Undo>,
}

/// Each file [`offload`] wrote, with its text before (`None`: there was none). Dropped
/// without [`Undo::keep`], it puts each file back, so every way a save ends short of a merge
/// (a refusal, an error) leaves the folder as it found it.
#[derive(Debug)]
pub struct Undo {
    files: Vec<(std::path::PathBuf, Option<String>)>,
    kept: std::cell::Cell<bool>,
}

impl Undo {
    /// Keeps what was written: the save went through, or the files are used at once.
    pub fn keep(&self) {
        self.kept.set(true);
    }
}

impl Drop for Undo {
    /// Puts each file back as it was, unless kept. The bytes the plugin stored stay: a store
    /// keeps what it is given, and a later save of the same file names them again.
    fn drop(&mut self) {
        if self.kept.get() {
            return;
        }
        for (path, before) in &self.files {
            match before {
                Some(text) => {
                    let _ = std::fs::write(path, text);
                }
                None => {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
}

/// Offloads each of `paths` (relative to `work_root`, files that exist) over `limit_bytes`:
/// puts it, writes its line in `work_root`'s manifest (replacing a line of the same path), and
/// names it in `exclude` (the clone's `info/exclude`), so Git leaves the file alone there. The
/// manifest is written only when a line changed.
///
/// # Errors
///
/// A put fails, or the manifest cannot be read or written. Nothing is written then.
pub fn offload(
    plugin: &Plugin<'_>,
    work_root: &Path,
    paths: &[String],
    rule: Rule<'_>,
    exclude: &Path,
) -> Result<Offloaded, String> {
    let mut offloaded = Offloaded::default();
    for path in paths {
        let file = work_root.join(path);
        let Ok(meta) = std::fs::metadata(&file) else {
            continue;
        };
        if !meta.is_file() || !rule.offloads(path, meta.len()) {
            continue;
        }
        let id = plugin.put(&file)?;
        offloaded.entries.push(Entry {
            id,
            size: meta.len(),
            path: path.clone(),
        });
    }
    if offloaded.entries.is_empty() {
        return Ok(offloaded);
    }
    let manifest = work_root.join(MANIFEST);
    let before = match std::fs::read_to_string(&manifest) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("{MANIFEST} cannot be read: {error}")),
    };
    // A view's sparse set may leave the manifest out of the folder: it is still the tree's, so
    // its lines are read from the commit the folder stands on, never taken as none.
    let current = match &before {
        Some(text) => parse(text)?,
        None => manifest_at(&Git::new(work_root), "HEAD")?,
    };
    offloaded.undo = Some(Undo {
        files: vec![
            (manifest.clone(), before),
            (exclude.to_path_buf(), std::fs::read_to_string(exclude).ok()),
        ],
        kept: std::cell::Cell::new(false),
    });
    let mut next: Vec<Entry> = current
        .into_iter()
        .filter(|entry| !offloaded.entries.iter().any(|new| new.path == entry.path))
        .collect();
    next.extend(offloaded.entries.iter().cloned());
    std::fs::write(&manifest, text(&next))
        .map_err(|error| format!("{MANIFEST} cannot be written: {error}"))?;
    exclude_paths(exclude, &offloaded.entries)?;
    Ok(offloaded)
}

/// Adds each entry's path to `exclude` (`info/exclude`) as an anchored, literal pattern, once.
fn exclude_paths(exclude: &Path, entries: &[Entry]) -> Result<(), String> {
    let current = std::fs::read_to_string(exclude).unwrap_or_default();
    let mut text = current.clone();
    for entry in entries {
        let pattern = format!("/{}", glob_escape(&entry.path));
        if !current.lines().any(|line| line == pattern) {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&pattern);
            text.push('\n');
        }
    }
    if text == current {
        return Ok(());
    }
    if let Some(dir) = exclude.parent() {
        std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    }
    std::fs::write(exclude, text)
        .map_err(|error| format!("{} cannot be written: {error}", exclude.display()))
}

/// `path` with the characters a gitignore pattern reads specially escaped.
fn glob_escape(path: &str) -> String {
    path.chars().fold(String::new(), |mut out, c| {
        if matches!(c, '*' | '?' | '[' | ']' | '\\' | '!' | '#') {
            out.push('\\');
        }
        out.push(c);
        out
    })
}

#[cfg(test)]
mod tests {
    use super::{Entry, is_id, merge3, parse, text};

    /// Two changes that each add a file merge; a path both changed differently does not.
    #[test]
    fn manifests_merge_path_by_path() {
        let line = |c: char, path: &str| format!("sha256:{} 1 {path}\n", c.to_string().repeat(64));
        let base = line('a', "m/a.mov");
        let ours = format!("{base}{}", line('b', "m/b.mov"));
        let theirs = format!("{base}{}", line('c', "m/c.mov"));
        let merged = merge3(&base, &ours, &theirs).unwrap();
        assert_eq!(
            merged,
            format!("{base}{}{}", line('b', "m/b.mov"), line('c', "m/c.mov"))
        );
        assert!(merge3(&base, &line('b', "m/a.mov"), &line('c', "m/a.mov")).is_none());
        assert_eq!(
            merge3(&base, &base, "").unwrap(),
            "",
            "a removal on one side is taken"
        );
    }

    #[test]
    fn the_manifest_reads_back_what_it_wrote_sorted_by_path() {
        let id = format!("sha256:{}", "a".repeat(64));
        let entries = vec![
            Entry {
                id: id.clone(),
                size: 7,
                path: "video/b clip.mov".to_owned(),
            },
            Entry {
                id: id.clone(),
                size: 3,
                path: "art/a.psd".to_owned(),
            },
        ];
        let written = text(&entries);
        assert!(
            written.starts_with(&format!("{id} 3 art/a.psd\n")),
            "{written}"
        );
        let read = parse(&written).unwrap();
        assert_eq!(read[1].path, "video/b clip.mov");
        assert!(parse("sha256:xyz 3 a").is_err());
        assert!(parse(&format!("{id} three a")).is_err());
        assert!(!is_id(&format!("sha256:{}", "A".repeat(64))));
    }
}
