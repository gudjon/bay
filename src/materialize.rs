//! Materializing a tree for a check or a provider (`INTERFACES` § 7; plan wave 2, W4).
//!
//! The tree is written straight from the object store, file by file: `git ls-tree` names the
//! entries and one `git cat-file --batch` streams their blobs, so no tar of the whole tree is
//! ever made and no one Git call has to finish within the bound of a Git call. Nothing converts
//! a blob on its way out (no `core.autocrlf`, no `.gitattributes` `text`, `eol`, `ident` or
//! filter), so every file's blob id is the tree's; the executable bit is the tree's, and a
//! symbolic link is a link where the repository's `core.symlinks` says links are made (a file
//! holding its target otherwise, as Git writes one). A path list keeps only the entries it
//! covers; a deadline bounds the whole materialization.
//!
//! In a partial clone a blob the store lacks would be fetched on its own as `cat-file` reaches
//! it, one round trip per file. So before streaming, the absent blobs are found without
//! fetching any and fetched together, in one `git fetch --stdin` from the promisor remote per
//! chunk, the form Git's own promisor code uses (G170). A blob still absent after that is a
//! fault, never a partial tree.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::git::{Git, GitError};
use crate::version::Version;

/// Why a tree was not materialized.
#[derive(Debug)]
pub enum Unmade {
    /// The deadline passed first.
    TimedOut,
    /// The tree holds a path bay does not write (a `.git` part, a `..`, or a name this
    /// platform cannot hold): decided by the tree, so the run records it as `infra_failure`.
    Refused(String),
    /// A fault of bay's own or of the object store, never a verdict.
    Failed(String),
    /// G265: the filesystem the tree would be written to is below the box's disk floor
    /// (`bay.minFreeMiB`); nothing was written.
    DiskLow(crate::disk::Low),
}

/// Replace refs never stand in for an object of the tree: what is written is exactly the
/// tree's (`GIT_NO_REPLACE_OBJECTS`).
const NO_REPLACE: (&str, &str) = ("GIT_NO_REPLACE_OBJECTS", "1");

/// The Git that has `--no-lazy-fetch` and `GIT_NO_LAZY_FETCH` (2.45). Below it the absent
/// blobs are found by a missing-object walk of the commit, which never fetches either.
const NO_LAZY_FETCH_SINCE: &str = "2.45";

/// The most blob ids one batched fetch asks for; a larger set takes several fetches, each
/// within the deadline.
const FETCH_CHUNK: usize = 10_000;

/// Writes `commit`'s tree into `dir`, with no `.git`: only the entries `paths` covers (each a
/// file, or a directory ending in `/`) when it is given, the whole tree when it is not. Gives
/// up at `deadline`. Calls `tick` as it goes (at least once per file and per 64 KiB written),
/// so a caller holding a lease can renew it.
///
/// # Errors
///
/// [`Unmade::TimedOut`] when `deadline` passes before the last file is written;
/// [`Unmade::Refused`] when the tree holds a path bay does not write; [`Unmade::Failed`] when
/// Git cannot list or read the tree or a file cannot be written.
pub fn materialize(
    git: &Git,
    commit: &str,
    paths: Option<&[String]>,
    dir: &Path,
    deadline: Instant,
    tick: &mut dyn FnMut(),
) -> Result<(), Unmade> {
    std::fs::create_dir_all(dir).map_err(|error| failed(dir, &error))?;
    // G265: below the box's disk floor no file of the tree is written.
    if let Some(low) = crate::disk::below_floor(git, dir).map_err(Unmade::Failed)? {
        return Err(Unmade::DiskLow(low));
    }
    tick();
    let entries = list(git, commit, paths, deadline)?;
    tick();
    let links = makes_links(git)?;
    let mut blobs = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.mode == Mode::Gitlink {
            // A submodule is an empty directory, as `git archive` and a checkout leave it.
            let path = entry.place(dir)?;
            std::fs::create_dir_all(&path).map_err(|error| failed(&path, &error))?;
        } else {
            blobs.push(entry);
        }
    }
    if blobs.is_empty() {
        return Ok(());
    }
    let store = Store::open(git);
    let blobs = match &store {
        Some(store) => store.link_all(git, blobs, dir, deadline, tick)?,
        None => blobs,
    };
    if blobs.is_empty() {
        return Ok(());
    }
    prefetch(git, commit, &blobs, deadline)?;
    tick();
    stream(git, &blobs, dir, deadline, links, tick)?;
    if let Some(store) = &store {
        store.keep_all(&blobs, dir);
    }
    Ok(())
}

/// A fleet finding (2026-10-26): a check whose tree is the whole repository wrote ~4 GB for
/// every candidate and its base, about six minutes each on a runner, and the whole queue waited
/// on it. So each file's bytes are kept once, in the repository's own store
/// (`<common dir>/bay/blobs`), and a tree is made of hard links to them: a file is written once
/// per box, never once per tree.
///
/// A kept file is read-only and carries a fixed modification time, and it is used only while
/// its size and time are still those: a check that writes to a file of its tree fails to (it
/// may replace the file, never change the kept one), and one that changes a kept file anyway
/// leaves an entry no later tree uses. A link that cannot be made (another filesystem, an
/// inode's link limit) leaves that file to be written as before.
#[cfg(unix)]
struct Store {
    dir: PathBuf,
}

/// The modification time every kept file carries: 2000-01-01, which no write leaves.
#[cfg(unix)]
const KEPT_AT: Duration = Duration::from_hours(262_968);

/// How often unused kept files (no tree links them) are removed.
#[cfg(unix)]
const PRUNE_EVERY: Duration = Duration::from_secs(3600);

#[cfg(unix)]
impl Store {
    /// The repository's store, made if it is absent; `None` when it cannot be.
    fn open(git: &Git) -> Option<Self> {
        let common = git.common_dir().ok()?;
        let dir = Path::new(&common).join("bay").join("blobs");
        std::fs::create_dir_all(&dir).ok()?;
        let store = Self { dir };
        store.prune();
        Some(store)
    }

    /// Where `blob`'s bytes are kept: by id, and apart for an executable one.
    fn entry(&self, blob: &Entry) -> PathBuf {
        let suffix = if blob.mode == Mode::Executable {
            ".x"
        } else {
            ""
        };
        self.dir
            .join(&blob.oid[..2.min(blob.oid.len())])
            .join(format!("{}{suffix}", blob.oid))
    }

    /// Links into place under `dir` every blob of `blobs` the store holds intact; returns the
    /// rest, which are written as before. Symbolic links are never kept.
    fn link_all(
        &self,
        git: &Git,
        blobs: Vec<Entry>,
        dir: &Path,
        deadline: Instant,
        tick: &mut dyn FnMut(),
    ) -> Result<Vec<Entry>, Unmade> {
        let sizes = sizes(git, &blobs, deadline)?;
        let mut rest = Vec::new();
        let mut made_parent: Option<PathBuf> = None;
        for blob in blobs {
            remaining(deadline)?;
            let keepable = blob.mode != Mode::Symlink;
            let entry = self.entry(&blob);
            let intact = keepable
                && sizes
                    .get(blob.oid.as_str())
                    .is_some_and(|size| kept_intact(&entry, *size, &blob));
            if !intact {
                rest.push(blob);
                continue;
            }
            let place = blob.place(dir)?;
            if let Some(parent) = place.parent()
                && made_parent.as_deref() != Some(parent)
            {
                std::fs::create_dir_all(parent).map_err(|error| failed(parent, &error))?;
                made_parent = Some(parent.to_path_buf());
            }
            if std::fs::hard_link(&entry, &place).is_err() {
                rest.push(blob);
            }
            tick();
        }
        Ok(rest)
    }

    /// Keeps every file of `blobs` just written under `dir`: read-only, at [`KEPT_AT`], and
    /// linked into the store. A file that cannot be kept stays as written.
    fn keep_all(&self, blobs: &[Entry], dir: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        for blob in blobs.iter().filter(|blob| blob.mode != Mode::Symlink) {
            let Ok(place) = blob.place(dir) else {
                continue;
            };
            let kept = std::fs::File::open(&place).and_then(|file| {
                file.set_modified(std::time::UNIX_EPOCH + KEPT_AT)?;
                let mode = if blob.mode == Mode::Executable {
                    0o555
                } else {
                    0o444
                };
                file.set_permissions(std::fs::Permissions::from_mode(mode))
            });
            if kept.is_err() {
                continue;
            }
            let entry = self.entry(blob);
            if let Some(parent) = entry.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if !kept_intact_any(&entry) {
                let _ = std::fs::remove_file(&entry);
            }
            let _ = std::fs::hard_link(&place, &entry);
        }
    }

    /// At most once per [`PRUNE_EVERY`], removes every kept file no tree links any more.
    fn prune(&self) {
        use std::os::unix::fs::MetadataExt as _;
        let marker = self.dir.join(".pruned");
        let due = std::fs::metadata(&marker)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_none_or(|age| age >= PRUNE_EVERY);
        if !due || std::fs::write(&marker, b"").is_err() {
            return;
        }
        let Ok(shards) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for shard in shards.flatten() {
            let Ok(files) = std::fs::read_dir(shard.path()) else {
                continue;
            };
            for file in files.flatten() {
                if file
                    .metadata()
                    .is_ok_and(|meta| meta.is_file() && meta.nlink() == 1)
                {
                    let _ = std::fs::remove_file(file.path());
                }
            }
        }
    }
}

/// Whether the kept file at `entry` still holds `blob` as it was kept: a read-only file of
/// `size` bytes at [`KEPT_AT`], executable exactly when `blob` is.
#[cfg(unix)]
fn kept_intact(entry: &Path, size: u64, blob: &Entry) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::symlink_metadata(entry).is_ok_and(|meta| {
        let mode = meta.permissions().mode();
        meta.is_file()
            && meta.len() == size
            && meta.modified().ok() == Some(std::time::UNIX_EPOCH + KEPT_AT)
            && mode & 0o222 == 0
            && (mode & 0o111 != 0) == (blob.mode == Mode::Executable)
    })
}

/// Whether a file is kept at `entry` that no write has touched since.
#[cfg(unix)]
fn kept_intact_any(entry: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::symlink_metadata(entry).is_ok_and(|meta| {
        meta.is_file()
            && meta.modified().ok() == Some(std::time::UNIX_EPOCH + KEPT_AT)
            && meta.permissions().mode() & 0o222 == 0
    })
}

/// The size of each blob of `blobs` the store holds, by id, read without fetching any.
#[cfg(unix)]
fn sizes(
    git: &Git,
    blobs: &[Entry],
    deadline: Instant,
) -> Result<std::collections::BTreeMap<String, u64>, Unmade> {
    let out = git
        .run_bytes_for(
            &["cat-file", "--batch-check=%(objectname) %(objectsize)"],
            &[NO_REPLACE, ("GIT_NO_LAZY_FETCH", "1")],
            Some(&lines(blobs.iter().map(|blob| blob.oid.as_str()))),
            remaining(deadline)?,
        )
        .map_err(|error| from_git(&error))?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|line| {
            let (oid, size) = line.split_once(' ')?;
            Some((oid.to_owned(), size.parse().ok()?))
        })
        .collect())
}

/// Where hard links and read-only files are not the platform's, every tree is written whole.
#[cfg(not(unix))]
struct Store;

#[cfg(not(unix))]
impl Store {
    fn open(_: &Git) -> Option<Self> {
        None
    }

    fn link_all(
        &self,
        _: &Git,
        blobs: Vec<Entry>,
        _: &Path,
        _: Instant,
        _: &mut dyn FnMut(),
    ) -> Result<Vec<Entry>, Unmade> {
        Ok(blobs)
    }

    fn keep_all(&self, _: &[Entry], _: &Path) {}
}

/// In a partial clone, fetches every blob of `blobs` the store lacks in one batched fetch per
/// [`FETCH_CHUNK`] from the promisor remote, instead of one lazy fetch per blob. A store with
/// no promisor remote is asked nothing.
///
/// # Errors
///
/// [`Unmade::TimedOut`] at `deadline`; [`Unmade::Failed`], naming how many blobs are absent,
/// when the fetch fails or leaves one absent.
fn prefetch(git: &Git, commit: &str, blobs: &[Entry], deadline: Instant) -> Result<(), Unmade> {
    let Some(remote) = promisor_remote(git)? else {
        return Ok(());
    };
    let wanted: BTreeSet<&str> = blobs.iter().map(|blob| blob.oid.as_str()).collect();
    let no_lazy = git
        .version()
        .ok()
        .and_then(|text| Version::find(&text))
        .is_some_and(|version| version >= Version::parse(NO_LAZY_FETCH_SINCE));
    let missing = absent(git, commit, &wanted, no_lazy, deadline)?;
    if missing.is_empty() {
        return Ok(());
    }
    for chunk in missing.chunks(FETCH_CHUNK) {
        git.run_bytes_for(
            &[
                "-c",
                "fetch.negotiationAlgorithm=noop",
                "fetch",
                "--no-tags",
                "--no-write-fetch-head",
                "--recurse-submodules=no",
                "--filter=blob:none",
                "--stdin",
                &remote,
            ],
            &[NO_REPLACE],
            Some(&lines(chunk.iter().map(String::as_str))),
            remaining(deadline)?,
        )
        .map_err(|error| match error {
            GitError::Timeout { .. } => Unmade::TimedOut,
            other => Unmade::Failed(format!(
                "{} blobs of the tree are absent from this partial clone, and fetching them from its promisor remote {remote} failed: {}",
                missing.len(),
                other.message()
            )),
        })?;
    }
    let fetched: BTreeSet<&str> = missing.iter().map(String::as_str).collect();
    let left = absent(git, commit, &fetched, no_lazy, deadline)?;
    if left.is_empty() {
        Ok(())
    } else {
        Err(Unmade::Failed(format!(
            "{} blobs of the tree are still absent from this partial clone after fetching them from its promisor remote {remote}",
            left.len()
        )))
    }
}

/// One id per line, for a Git call that reads ids on stdin.
fn lines<'a>(ids: impl Iterator<Item = &'a str>) -> Vec<u8> {
    let mut out = Vec::new();
    for oid in ids {
        out.extend_from_slice(oid.as_bytes());
        out.push(b'\n');
    }
    out
}

/// The remote a partial clone fetches its absent objects from: `extensions.partialClone`,
/// else the first remote whose `remote.<name>.promisor` is true; `None` in a full clone.
fn promisor_remote(git: &Git) -> Result<Option<String>, Unmade> {
    let read = |args: &[&str]| -> Result<Option<String>, Unmade> {
        match git.run_status(args).map_err(|error| from_git(&error))? {
            (0, out, _) => Ok(Some(out)),
            (1, _, _) => Ok(None),
            (_, _, err) => Err(Unmade::Failed(format!(
                "the promisor remote cannot be read: {err}"
            ))),
        }
    };
    let named = match read(&["config", "--get", "extensions.partialclone"])? {
        Some(name) => Some(name),
        None => read(&[
            "config",
            "--type=bool",
            "--get-regexp",
            r"^remote\..*\.promisor$",
        ])?
        .and_then(|out| {
            out.lines().find_map(|line| {
                let (key, value) = line.split_once(' ')?;
                let name = key.strip_prefix("remote.")?.strip_suffix(".promisor")?;
                (value == "true").then(|| name.to_owned())
            })
        }),
    };
    // A name Git would read as an option is left to the lazy fetch, which never parses it.
    Ok(named.filter(|name| !name.is_empty() && !name.starts_with('-')))
}

/// Which of `wanted` the store lacks, found without fetching any: `cat-file --batch-check`
/// under `--no-lazy-fetch` where Git has it, else a missing-object walk of `commit`.
fn absent(
    git: &Git,
    commit: &str,
    wanted: &BTreeSet<&str>,
    no_lazy: bool,
    deadline: Instant,
) -> Result<Vec<String>, Unmade> {
    type Marked = fn(&str) -> Option<&str>;
    let (out, marked): (Vec<u8>, Marked) = if no_lazy {
        let out = git
            .run_bytes_for(
                &["--no-lazy-fetch", "cat-file", "--batch-check"],
                &[NO_REPLACE, ("GIT_NO_LAZY_FETCH", "1")],
                Some(&lines(wanted.iter().copied())),
                remaining(deadline)?,
            )
            .map_err(|error| from_git(&error))?;
        (out, |line| line.strip_suffix(" missing"))
    } else {
        let out = git
            .run_bytes_for(
                &[
                    "rev-list",
                    "--objects",
                    "--missing=print",
                    "--no-walk",
                    commit,
                ],
                &[NO_REPLACE],
                None,
                remaining(deadline)?,
            )
            .map_err(|error| from_git(&error))?;
        (out, |line| line.strip_prefix('?'))
    };
    let found: BTreeSet<String> = String::from_utf8_lossy(&out)
        .lines()
        .filter_map(marked)
        .filter(|oid| wanted.contains(oid))
        .map(str::to_owned)
        .collect();
    Ok(found.into_iter().collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    File,
    Executable,
    Symlink,
    Gitlink,
}

struct Entry {
    mode: Mode,
    oid: String,
    /// The path as the tree holds it, `/`-separated bytes.
    path: Vec<u8>,
}

impl Entry {
    /// Where the entry goes under `dir`. A path with an empty, `.`, `..` or `.git` part (or,
    /// where Windows names files, a part Windows cannot hold as a plain file name, see
    /// [`windows_plain`]) is refused, never written: Git itself never checks one out.
    fn place(&self, dir: &Path) -> Result<PathBuf, Unmade> {
        let mut place = dir.to_path_buf();
        for part in self.path.split(|byte| *byte == b'/') {
            let plain = !part.is_empty()
                && part != b"."
                && part != b".."
                && !part.eq_ignore_ascii_case(b".git")
                && (cfg!(unix) || windows_plain(part));
            if !plain {
                return Err(Unmade::Refused(format!(
                    "the tree holds the path {:?}, which bay does not write",
                    String::from_utf8_lossy(&self.path)
                )));
            }
            place.push(component(part));
        }
        Ok(place)
    }
}

/// Whether Windows holds `part` as the plain file name it reads: no reserved character or
/// control byte, no trailing dot or space (Windows drops them), and no device name (`CON`,
/// `PRN`, `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`), with an extension or without.
fn windows_plain(part: &[u8]) -> bool {
    const DEVICES: [&[u8]; 4] = [b"CON", b"PRN", b"AUX", b"NUL"];
    if part
        .iter()
        .any(|byte| *byte < 0x20 || br#"<>:"/\|?*"#.contains(byte))
        || part.ends_with(b".")
        || part.ends_with(b" ")
    {
        return false;
    }
    let stem = part.split(|byte| *byte == b'.').next().unwrap_or_default();
    let stem = stem.trim_ascii_end();
    let device = DEVICES.iter().any(|name| stem.eq_ignore_ascii_case(name))
        || (stem.len() == 4
            && (stem[..3].eq_ignore_ascii_case(b"COM") || stem[..3].eq_ignore_ascii_case(b"LPT"))
            && matches!(stem[3], b'1'..=b'9'));
    !device
}

#[cfg(unix)]
fn component(part: &[u8]) -> &std::ffi::OsStr {
    use std::os::unix::ffi::OsStrExt as _;
    std::ffi::OsStr::from_bytes(part)
}

/// Where a file name is UTF-16, a tree path is listed only when it is UTF-8 (see [`parse`]).
#[cfg(not(unix))]
fn component(part: &[u8]) -> &std::ffi::OsStr {
    std::ffi::OsStr::new(std::str::from_utf8(part).unwrap_or_default())
}

fn failed(path: &Path, error: &std::io::Error) -> Unmade {
    Unmade::Failed(format!("{}: {error}", path.display()))
}

/// What is left before `deadline`, or [`Unmade::TimedOut`] when nothing is.
fn remaining(deadline: Instant) -> Result<Duration, Unmade> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(Unmade::TimedOut)
    } else {
        Ok(left)
    }
}

fn from_git(error: &GitError) -> Unmade {
    match error {
        GitError::Timeout { .. } => Unmade::TimedOut,
        other => Unmade::Failed(other.message()),
    }
}

/// The most paths one `git ls-tree` names as pathspecs. A longer list (a changed-only check's
/// changed paths, N-18) lists the whole tree's entries instead, and [`Keep`] filters them, so no
/// command line outgrows what the system passes.
const PATHSPEC_MAX: usize = 1_000;

/// What a path list keeps: a directory entry (ending in `/`) keeps what lies under it, a file
/// entry keeps that file. Files are looked up by name, so a list of many changed paths filters
/// a large tree in one pass.
struct Keep<'a> {
    files: std::collections::HashSet<&'a [u8]>,
    dirs: Vec<&'a [u8]>,
}

impl<'a> Keep<'a> {
    fn new(paths: &'a [String]) -> Self {
        let (dirs, files): (Vec<&[u8]>, Vec<&[u8]>) = paths
            .iter()
            .map(String::as_bytes)
            .partition(|keep| keep.ends_with(b"/"));
        Self {
            files: files.into_iter().collect(),
            dirs,
        }
    }

    /// Whether `path` (a tree path) is one the list keeps.
    fn keeps(&self, path: &[u8]) -> bool {
        self.files.contains(path) || self.dirs.iter().any(|dir| path.starts_with(dir))
    }
}

/// The blob and submodule entries of `commit`'s tree that `paths` keeps, in tree order. An
/// empty list keeps nothing, and Git is not asked.
fn list(
    git: &Git,
    commit: &str,
    paths: Option<&[String]>,
    deadline: Instant,
) -> Result<Vec<Entry>, Unmade> {
    if paths.is_some_and(<[String]>::is_empty) {
        return Ok(Vec::new());
    }
    let keep = paths.map(Keep::new);
    // Literal pathspecs: a policy path is a name, never a pattern; the listing is filtered
    // again below, so a prefix never stands for a sibling (`board` for `board2/`).
    let mut args = vec![
        "--literal-pathspecs",
        "ls-tree",
        "-r",
        "-z",
        "--full-tree",
        commit,
    ];
    if let Some(paths) = paths.filter(|paths| paths.len() <= PATHSPEC_MAX) {
        args.push("--");
        args.extend(paths.iter().map(|path| path.trim_end_matches('/')));
    }
    let out = git
        .run_bytes_for(&args, &[NO_REPLACE], None, remaining(deadline)?)
        .map_err(|error| from_git(&error))?;
    let mut entries = Vec::new();
    for record in out
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let entry = parse(record).ok_or_else(|| {
            Unmade::Failed(format!(
                "git ls-tree gave the entry {:?}",
                String::from_utf8_lossy(record)
            ))
        })?;
        if keep.as_ref().is_some_and(|keep| !keep.keeps(&entry.path)) {
            continue;
        }
        // A name that is not UTF-8 cannot be written where file names are UTF-16.
        if !cfg!(unix) && std::str::from_utf8(&entry.path).is_err() {
            return Err(Unmade::Refused(format!(
                "the tree holds the path {:?}, which is not UTF-8 and which bay does not write here",
                String::from_utf8_lossy(&entry.path)
            )));
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// One `<mode> SP <type> SP <oid> TAB <path>` record of `git ls-tree -z`.
fn parse(record: &[u8]) -> Option<Entry> {
    let tab = record.iter().position(|byte| *byte == b'\t')?;
    let head = std::str::from_utf8(&record[..tab]).ok()?;
    let mut words = head.split(' ');
    let (mode, kind, oid) = (words.next()?, words.next()?, words.next()?);
    let bits = u32::from_str_radix(mode, 8).ok()?;
    let mode = match (bits & 0o170_000, kind) {
        (0o100_000, "blob") if bits & 0o111 != 0 => Mode::Executable,
        (0o100_000, "blob") => Mode::File,
        (0o120_000, "blob") => Mode::Symlink,
        (0o160_000, "commit") => Mode::Gitlink,
        _ => return None,
    };
    let plain_oid = !oid.is_empty() && oid.bytes().all(|byte| byte.is_ascii_hexdigit());
    let path = &record[tab + 1..];
    (plain_oid && words.next().is_none()).then(|| Entry {
        mode,
        oid: oid.to_owned(),
        path: path.to_vec(),
    })
}

/// Whether the repository makes symbolic links: `core.symlinks`, true when unset, as Git
/// takes it.
fn makes_links(git: &Git) -> Result<bool, Unmade> {
    let (code, out, err) = git
        .run_status(&["config", "--type=bool", "--get", "core.symlinks"])
        .map_err(|error| from_git(&error))?;
    match (code, out.as_str()) {
        (0, "false") => Ok(false),
        (0, "true") | (1, "") => Ok(true),
        _ => Err(Unmade::Failed(format!(
            "core.symlinks cannot be read: {err}"
        ))),
    }
}

/// Streams every blob of `blobs` out of one `git cat-file --batch` into its place under
/// `dir`; the call is killed at `deadline`.
fn stream(
    git: &Git,
    blobs: &[Entry],
    dir: &Path,
    deadline: Instant,
    links: bool,
    tick: &mut dyn FnMut(),
) -> Result<(), Unmade> {
    // After the prefetch every blob is here; a store whose promisor remote bay cannot name is
    // left to Git's own fetch.
    let mut cmd = git.fetching().command();
    cmd.env(NO_REPLACE.0, NO_REPLACE.1)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|error| Unmade::Failed(format!("failed to run git: {error}")))?;
    let started = Instant::now();
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let status = child.wait().ok();
        crate::git::trace(
            &cmd,
            started,
            status.as_ref().and_then(std::process::ExitStatus::code),
        );
        return Err(Unmade::Failed("git cat-file has no pipes".to_owned()));
    };
    let mut ids = Vec::with_capacity(blobs.len() * 65);
    for blob in blobs {
        ids.extend_from_slice(blob.oid.as_bytes());
        ids.push(b'\n');
    }
    // A child killed at the deadline closes its stdin; the feeder then stops on the error.
    let feeder = std::thread::spawn(move || stdin.write_all(&ids));
    let child = Arc::new(Mutex::new(child));
    let done = Arc::new(AtomicBool::new(false));
    let watchdog = {
        let (child, done) = (Arc::clone(&child), Arc::clone(&done));
        std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                if Instant::now() >= deadline {
                    if let Ok(mut child) = child.lock() {
                        let _ = child.kill();
                    }
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };
    // A fleet finding (2026-10-26): the heartbeat may wait on the network for minutes. Run on
    // the thread that reads cat-file, it let cat-file's pipe fill, and a runner holding the
    // head froze. So the files are written on a thread of their own, and `tick` runs here.
    let written = std::thread::scope(|scope| {
        let reader = scope.spawn(move || {
            write_all(
                &mut BufReader::with_capacity(1 << 16, stdout),
                blobs,
                dir,
                deadline,
                links,
                &mut || {},
            )
        });
        while !reader.is_finished() {
            tick();
            std::thread::sleep(Duration::from_millis(20));
        }
        reader
            .join()
            .unwrap_or_else(|_| Err(Unmade::Failed("the file writer stopped".to_owned())))
    });
    done.store(true, Ordering::SeqCst);
    let _ = watchdog.join();
    if let Ok(mut child) = child.lock() {
        let _ = child.kill();
        let status = child.wait().ok();
        crate::git::trace(
            &cmd,
            started,
            status.as_ref().and_then(std::process::ExitStatus::code),
        );
    }
    let _ = feeder.join();
    match written {
        Err(Unmade::Failed(_)) if Instant::now() >= deadline => Err(Unmade::TimedOut),
        other => other,
    }
}

/// Reads each blob of `blobs`, in order, from `reader` (a `git cat-file --batch`) and writes
/// it into place. Symbolic links are made last, so no file is ever written through one.
fn write_all(
    reader: &mut impl BufRead,
    blobs: &[Entry],
    dir: &Path,
    deadline: Instant,
    links: bool,
    tick: &mut dyn FnMut(),
) -> Result<(), Unmade> {
    let mut later = Vec::new();
    let mut header = Vec::new();
    let mut made_parent: Option<PathBuf> = None;
    let mut buffer = vec![0_u8; 1 << 16];
    for blob in blobs {
        remaining(deadline)?;
        tick();
        let size = header_of(reader, &mut header, blob)?;
        let place = blob.place(dir)?;
        if let Some(parent) = place.parent()
            && made_parent.as_deref() != Some(parent)
        {
            std::fs::create_dir_all(parent).map_err(|error| failed(parent, &error))?;
            made_parent = Some(parent.to_path_buf());
        }
        if blob.mode == Mode::Symlink && links {
            let mut target = Vec::new();
            reader
                .take(size)
                .read_to_end(&mut target)
                .map_err(|error| failed(&place, &error))?;
            if target.len() as u64 != size {
                return Err(ended_early());
            }
            later.push((place, target));
        } else {
            let mut file = create(&place, blob.mode == Mode::Executable)?;
            copy(reader, &mut file, size, &mut buffer, deadline, &place, tick)?;
        }
        let mut newline = [0_u8; 1];
        reader.read_exact(&mut newline).map_err(|_| ended_early())?;
    }
    for (place, target) in later {
        remaining(deadline)?;
        link(&target, &place)?;
    }
    Ok(())
}

fn ended_early() -> Unmade {
    Unmade::Failed("git cat-file ended before the last blob".to_owned())
}

/// Reads the `<oid> SP blob SP <size> LF` header of `blob` and returns its size.
fn header_of(reader: &mut impl BufRead, header: &mut Vec<u8>, blob: &Entry) -> Result<u64, Unmade> {
    header.clear();
    let read = reader
        .read_until(b'\n', header)
        .map_err(|error| Unmade::Failed(format!("git cat-file: {error}")))?;
    if read == 0 {
        return Err(ended_early());
    }
    let text = String::from_utf8_lossy(header);
    let mut words = text.trim_end().split(' ');
    match (words.next(), words.next(), words.next(), words.next()) {
        (Some(oid), Some("blob"), Some(size), None) if oid == blob.oid => size
            .parse()
            .map_err(|_| Unmade::Failed(format!("git cat-file gave the header {text:?}"))),
        _ => Err(Unmade::Failed(format!(
            "the blob {} of {:?} cannot be read: {}",
            blob.oid,
            String::from_utf8_lossy(&blob.path),
            text.trim_end()
        ))),
    }
}

/// Copies exactly `size` bytes from `reader` into `file`, giving up at `deadline`.
fn copy(
    reader: &mut impl Read,
    file: &mut std::fs::File,
    size: u64,
    buffer: &mut [u8],
    deadline: Instant,
    place: &Path,
    tick: &mut dyn FnMut(),
) -> Result<(), Unmade> {
    let mut left = size;
    while left > 0 {
        let want = usize::try_from(left).map_or(buffer.len(), |left| left.min(buffer.len()));
        let got = reader
            .read(&mut buffer[..want])
            .map_err(|_| ended_early())?;
        if got == 0 {
            return Err(ended_early());
        }
        file.write_all(&buffer[..got])
            .map_err(|error| failed(place, &error))?;
        left -= got as u64;
        remaining(deadline)?;
        tick();
    }
    Ok(())
}

/// A new file at `place` (never an existing one, never through a link), executable when the
/// tree says so.
fn create(place: &Path, executable: bool) -> Result<std::fs::File, Unmade> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(if executable { 0o755 } else { 0o644 });
    }
    #[cfg(not(unix))]
    let _ = executable;
    options.open(place).map_err(|error| failed(place, &error))
}

#[cfg(unix)]
fn link(target: &[u8], place: &Path) -> Result<(), Unmade> {
    use std::os::unix::ffi::OsStrExt as _;
    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), place)
        .map_err(|error| failed(place, &error))
}

#[cfg(windows)]
fn link(target: &[u8], place: &Path) -> Result<(), Unmade> {
    let target = std::str::from_utf8(target).map_err(|_| {
        Unmade::Failed(format!(
            "{}: a link target that is not UTF-8",
            place.display()
        ))
    })?;
    std::os::windows::fs::symlink_file(target.replace('/', "\\"), place)
        .map_err(|error| failed(place, &error))
}

#[cfg(not(any(unix, windows)))]
fn link(_: &[u8], place: &Path) -> Result<(), Unmade> {
    Err(Unmade::Failed(format!(
        "{}: this platform makes no symbolic links",
        place.display()
    )))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::materialize;
    use crate::git::Git;

    fn repository(dir: &std::path::Path) -> Git {
        std::fs::create_dir_all(dir).unwrap();
        let git = Git::new(dir);
        git.run(&["init", "-q"]).unwrap();
        git
    }

    fn commit_all(git: &Git) -> String {
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
            "tree",
        ])
        .unwrap();
        git.run(&["rev-parse", "HEAD"]).unwrap()
    }

    /// Adds `bytes` at `path` with `mode`, exactly as given: no filter or conversion on the
    /// way in either.
    fn add(git: &Git, path: &str, bytes: &[u8], mode: &str) {
        let oid = git
            .run_with(
                &["hash-object", "-w", "--no-filters", "--stdin"],
                &[],
                Some(bytes),
            )
            .unwrap();
        let info = format!("{mode},{oid},{path}");
        git.run(&["update-index", "--add", "--cacheinfo", &info])
            .unwrap();
    }

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(600)
    }

    /// Every file of the materialization has the tree's blob id, whatever the repository's
    /// `core.autocrlf` and the tree's `.gitattributes` would convert at a checkout: a CRLF
    /// file, an LF file under `eol=crlf`, an `ident` file and an executable keep their bytes,
    /// and the executable keeps its bit. A link is a link where `core.symlinks` makes them,
    /// and a file holding its target where it does not.
    #[test]
    fn every_file_has_the_trees_blob_id() {
        let dir = tempfile::tempdir().unwrap();
        let git = repository(&dir.path().join("repo"));
        git.run(&["config", "core.autocrlf", "true"]).unwrap();
        let attributes: &[u8] = b"*.txt text eol=crlf\n*.id ident\n*.sh text eol=crlf\n";
        add(&git, ".gitattributes", attributes, "100644");
        add(&git, "lf.txt", b"one\ntwo\n", "100644");
        add(&git, "crlf.bin", b"one\r\ntwo\r\n", "100644");
        add(&git, "keyword.id", b"$Id$\n", "100644");
        add(&git, "tools/run.sh", b"#!/bin/sh\necho run\n", "100755");
        add(&git, "link", b"lf.txt", "120000");
        let commit = commit_all(&git);
        let out = dir.path().join("out");
        materialize(&git, &commit, None, &out, far(), &mut || {}).unwrap();
        let listing = git.run(&["ls-tree", "-r", &commit]).unwrap();
        assert_eq!(listing.lines().count(), 6);
        for line in listing.lines() {
            let (head, path) = line.split_once('\t').unwrap();
            let (mode, oid) = (&head[..6], head.rsplit(' ').next().unwrap());
            let place = out.join(path);
            if mode == "120000" {
                #[cfg(unix)]
                assert_eq!(
                    std::fs::read_link(&place).unwrap(),
                    std::path::Path::new("lf.txt")
                );
                continue;
            }
            let file = place.display().to_string();
            let written = git.run(&["hash-object", "--no-filters", &file]).unwrap();
            assert_eq!(written, oid, "{path}");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let exec = std::fs::metadata(&place).unwrap().permissions().mode() & 0o111;
                assert_eq!(exec != 0, mode == "100755", "{path}");
            }
        }
        git.run(&["config", "core.symlinks", "false"]).unwrap();
        let plain = dir.path().join("plain");
        materialize(&git, &commit, None, &plain, far(), &mut || {}).unwrap();
        let meta = std::fs::symlink_metadata(plain.join("link")).unwrap();
        assert!(!meta.is_symlink());
        assert_eq!(std::fs::read(plain.join("link")).unwrap(), b"lf.txt");
    }

    /// A second tree of the same files is made of links to the first one's kept bytes, never
    /// written again: the same inode, read-only, with the tree's bytes.
    #[cfg(unix)]
    #[test]
    fn a_file_is_written_once_and_linked_into_every_later_tree() {
        use std::os::unix::fs::MetadataExt as _;
        let dir = tempfile::tempdir().unwrap();
        let git = repository(&dir.path().join("repo"));
        add(&git, "a.txt", b"one\n", "100644");
        add(&git, "tools/run.sh", b"#!/bin/sh\n", "100755");
        let commit = commit_all(&git);
        let (first, second) = (dir.path().join("first"), dir.path().join("second"));
        materialize(&git, &commit, None, &first, far(), &mut || {}).unwrap();
        materialize(&git, &commit, None, &second, far(), &mut || {}).unwrap();
        for path in ["a.txt", "tools/run.sh"] {
            let (one, two) = (
                std::fs::metadata(first.join(path)).unwrap(),
                std::fs::metadata(second.join(path)).unwrap(),
            );
            assert_eq!(one.ino(), two.ino(), "{path} is linked, not written again");
            assert!(two.permissions().readonly(), "{path}");
        }
        assert_eq!(std::fs::read(second.join("a.txt")).unwrap(), b"one\n");
        assert!(
            std::fs::metadata(second.join("tools/run.sh"))
                .unwrap()
                .mode()
                & 0o111
                != 0
        );
    }

    /// A kept file that a check changed anyway is never linked into a later tree: that tree
    /// gets the tree's own bytes.
    #[cfg(unix)]
    #[test]
    fn a_kept_file_changed_by_a_check_is_never_reused() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let git = repository(&dir.path().join("repo"));
        add(&git, "a.txt", b"one\n", "100644");
        let commit = commit_all(&git);
        let first = dir.path().join("first");
        materialize(&git, &commit, None, &first, far(), &mut || {}).unwrap();
        let file = first.join("a.txt");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(&file, b"two\n").unwrap();
        let second = dir.path().join("second");
        materialize(&git, &commit, None, &second, far(), &mut || {}).unwrap();
        assert_eq!(std::fs::read(second.join("a.txt")).unwrap(), b"one\n");
    }

    /// A heartbeat that waits, as one on a slow network does, never holds up the reading of
    /// cat-file: the tree is written before the deadline however long each beat takes.
    #[test]
    fn a_slow_heartbeat_never_stops_the_files_being_written() {
        let dir = tempfile::tempdir().unwrap();
        let git = repository(&dir.path().join("repo"));
        for n in 0..40 {
            add(
                &git,
                &format!("f{n}.txt"),
                format!("{n}\n").as_bytes(),
                "100644",
            );
        }
        let commit = commit_all(&git);
        let out = dir.path().join("out");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut slow = || std::thread::sleep(Duration::from_millis(400));
        materialize(&git, &commit, None, &out, deadline, &mut slow).unwrap();
        assert_eq!(std::fs::read(out.join("f39.txt")).unwrap(), b"39\n");
    }

    /// A path list keeps what it covers: a directory entry what lies under it, never a sibling
    /// with the same prefix; a file entry that file, never a directory of that name.
    #[test]
    fn a_path_list_keeps_only_what_it_covers() {
        let dir = tempfile::tempdir().unwrap();
        let git = repository(&dir.path().join("repo"));
        for path in [
            "board/a",
            "board/b/c",
            "boardx/d",
            "board.md",
            "top.md",
            "notes/f",
            "other/e",
        ] {
            add(&git, path, path.as_bytes(), "100644");
        }
        let commit = commit_all(&git);
        let out = dir.path().join("out");
        let paths = ["board/", "top.md", "notes"].map(str::to_owned);
        materialize(&git, &commit, Some(&paths), &out, far(), &mut || {}).unwrap();
        for (path, present) in [
            ("board/a", true),
            ("board/b/c", true),
            ("top.md", true),
            ("boardx", false),
            ("board.md", false),
            ("notes", false),
            ("other", false),
        ] {
            assert_eq!(out.join(path).exists(), present, "{path}");
        }
    }

    /// N-18: an empty path list (a changed-only check on a deletion) writes nothing, and a list
    /// longer than one command line names keeps exactly what it covers, by a whole listing.
    #[test]
    fn an_empty_list_writes_nothing_and_a_long_one_keeps_what_it_covers() {
        let dir = tempfile::tempdir().unwrap();
        let git = repository(&dir.path().join("repo"));
        for path in ["a/one", "a/two", "b/three", "top.md"] {
            add(&git, path, path.as_bytes(), "100644");
        }
        let commit = commit_all(&git);
        let empty = dir.path().join("empty");
        let none: [String; 0] = [];
        materialize(&git, &commit, Some(&none), &empty, far(), &mut || {}).unwrap();
        assert_eq!(std::fs::read_dir(&empty).unwrap().count(), 0);
        // Absent names pad the list past what one listing names; they keep nothing.
        let mut paths: Vec<String> = (0..=super::PATHSPEC_MAX)
            .map(|n| format!("gone/{n}"))
            .collect();
        paths.extend(["a/one", "b/"].map(str::to_owned));
        let out = dir.path().join("out");
        materialize(
            &git,
            &commit,
            Some(paths.as_slice()),
            &out,
            far(),
            &mut || {},
        )
        .unwrap();
        for (path, present) in [
            ("a/one", true),
            ("a/two", false),
            ("b/three", true),
            ("top.md", false),
            ("gone", false),
        ] {
            assert_eq!(out.join(path).exists(), present, "{path}");
        }
    }

    /// G265: below the box's disk floor (`bay.minFreeMiB`) no file is written, and the refusal
    /// names the free space, the floor and the path it measured.
    #[cfg(unix)]
    #[test]
    fn below_the_disk_floor_nothing_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let git = repository(&dir.path().join("repo"));
        add(&git, "a", b"a", "100644");
        let commit = commit_all(&git);
        git.run(&["config", "bay.minFreeMiB", "1099511627776"])
            .unwrap();
        let out = dir.path().join("out");
        let made = materialize(&git, &commit, None, &out, far(), &mut || {});
        match made {
            Err(super::Unmade::DiskLow(low)) => {
                assert_eq!(low.floor_mib, 1 << 40);
                assert!(low.free_mib < low.floor_mib, "{low:?}");
                assert_eq!(low.path, out);
            }
            other => panic!("{other:?}"),
        }
        assert!(!out.join("a").exists());
        // A floor the filesystem is above writes the tree.
        git.run(&["config", "bay.minFreeMiB", "0"]).unwrap();
        materialize(&git, &commit, None, &out, far(), &mut || {}).unwrap();
        assert!(out.join("a").exists());
    }

    /// A deadline already past materializes nothing and says so.
    #[test]
    fn a_deadline_that_passed_is_timed_out() {
        let dir = tempfile::tempdir().unwrap();
        let git = repository(&dir.path().join("repo"));
        add(&git, "a", b"a", "100644");
        let commit = commit_all(&git);
        let out = dir.path().join("out");
        let made = materialize(&git, &commit, None, &out, Instant::now(), &mut || {});
        assert!(matches!(made, Err(super::Unmade::TimedOut)), "{made:?}");
    }

    /// A replace ref in the repository never stands in for a blob of the tree: the file
    /// written is the tree's blob, not its replacement.
    #[test]
    fn a_replace_ref_does_not_change_what_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let git = repository(&dir.path().join("repo"));
        add(&git, "a.txt", b"the tree's bytes\n", "100644");
        let commit = commit_all(&git);
        let original = git.run(&["rev-parse", &format!("{commit}:a.txt")]).unwrap();
        let other = git
            .run_with(
                &["hash-object", "-w", "--no-filters", "--stdin"],
                &[],
                Some(b"other bytes\n"),
            )
            .unwrap();
        git.run(&["replace", &original, &other]).unwrap();
        let out = dir.path().join("out");
        materialize(&git, &commit, None, &out, far(), &mut || {}).unwrap();
        assert_eq!(
            std::fs::read(out.join("a.txt")).unwrap(),
            b"the tree's bytes\n"
        );
    }

    /// A bare remote that serves filters, holding `count` files, and a blob-less clone of it
    /// with no checkout: every blob of the tree is absent from the clone.
    fn blobless(dir: &std::path::Path, count: usize) -> (Git, String, std::path::PathBuf) {
        let git = repository(&dir.join("seed"));
        for at in 0..count {
            add(
                &git,
                &format!("d/f{at:03}"),
                format!("{at}\n").as_bytes(),
                "100644",
            );
        }
        let commit = commit_all(&git);
        let bare = dir.join("remote.git");
        git.run(&["clone", "-q", "--bare", ".", bare.to_str().unwrap()])
            .unwrap();
        Git::new(&bare)
            .run(&["config", "uploadpack.allowFilter", "true"])
            .unwrap();
        let clone = dir.join("clone");
        Git::new(dir)
            .run(&[
                "clone",
                "-q",
                "--filter=blob:none",
                "--no-checkout",
                &format!("file://{}", bare.display()),
                clone.to_str().unwrap(),
            ])
            .unwrap();
        (Git::new(&clone), commit, bare)
    }

    /// The packs fetched from the promisor remote: each fetch from it, lazy or batched, keeps
    /// one pack marked `.promisor`.
    fn promisor_packs(clone: &Git) -> usize {
        std::fs::read_dir(clone.dir().join(".git/objects/pack"))
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|ext| ext == "promisor")
            })
            .count()
    }

    /// G170: in a blob-less clone the tree's absent blobs arrive in one batched fetch, not one
    /// fetch each, and every file still has the tree's blob id.
    #[test]
    fn a_blobless_tree_is_fetched_in_one_batch() {
        let dir = tempfile::tempdir().unwrap();
        let (clone, commit, _) = blobless(dir.path(), 40);
        let before = promisor_packs(&clone);
        let out = dir.path().join("out");
        materialize(&clone, &commit, None, &out, far(), &mut || {}).unwrap();
        assert_eq!(promisor_packs(&clone) - before, 1, "one batched fetch");
        for line in clone.run(&["ls-tree", "-r", &commit]).unwrap().lines() {
            let (head, path) = line.split_once('\t').unwrap();
            let file = out.join(path).display().to_string();
            let written = clone.run(&["hash-object", "--no-filters", &file]).unwrap();
            assert_eq!(written, head.rsplit(' ').next().unwrap(), "{path}");
        }
    }

    /// Below Git 2.45 the absent blobs are found by a missing-object walk: it finds the same
    /// ones as `--no-lazy-fetch`, and neither fetches.
    #[test]
    fn both_ways_of_finding_absent_blobs_agree_and_fetch_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (clone, commit, _) = blobless(dir.path(), 6);
        let listing = clone.run(&["ls-tree", "-r", &commit]).unwrap();
        let wanted: std::collections::BTreeSet<&str> = listing
            .lines()
            .map(|line| line.split_once('\t').unwrap().0.rsplit(' ').next().unwrap())
            .collect();
        let before = promisor_packs(&clone);
        let walked = super::absent(&clone, &commit, &wanted, false, far()).unwrap();
        let asked = super::absent(&clone, &commit, &wanted, true, far()).unwrap();
        assert_eq!(walked.len(), 6);
        assert_eq!(walked, asked);
        assert_eq!(promisor_packs(&clone), before);
    }

    /// With the promisor remote gone, absent blobs are a fault that names how many are
    /// absent, and no file of the tree is written.
    #[test]
    fn absent_blobs_with_no_remote_write_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (clone, commit, bare) = blobless(dir.path(), 5);
        std::fs::remove_dir_all(&bare).unwrap();
        let out = dir.path().join("out");
        let made = materialize(&clone, &commit, None, &out, far(), &mut || {});
        match made {
            Err(super::Unmade::Failed(why)) => assert!(why.contains("5 blobs"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(!out.join("d").exists(), "no partial tree");
    }

    /// Windows holds no device name, reserved character or trailing dot or space as a plain
    /// file name; anything else is a plain name.
    #[test]
    fn a_name_windows_cannot_hold_is_not_plain() {
        for part in [
            "CON",
            "nul",
            "Nul.txt",
            "aux.tar.gz",
            "COM1",
            "lpt9.log",
            "con .txt",
            "a:b",
            "a\\b",
            "a?",
            "a*",
            "a<b",
            "a|b",
            "a\"b",
            "dot.",
            "space ",
            "tab\t",
        ] {
            assert!(!super::windows_plain(part.as_bytes()), "{part}");
        }
        for part in [
            "CONSOLE", "nul_", "COM0", "COM10", "lpt", "a.txt", ".hidden", "x y",
        ] {
            assert!(super::windows_plain(part.as_bytes()), "{part}");
        }
    }
}
