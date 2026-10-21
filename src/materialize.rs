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
    prefetch(git, commit, &blobs, deadline)?;
    tick();
    stream(git, &blobs, dir, deadline, links, tick)
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

/// Whether `entry` (a tree path) is one `paths` keeps: a directory entry (ending in `/`) keeps
/// what lies under it, a file entry keeps that file.
fn kept(paths: &[String], path: &[u8]) -> bool {
    paths.iter().any(|keep| {
        let keep = keep.as_bytes();
        if keep.ends_with(b"/") {
            path.starts_with(keep)
        } else {
            path == keep
        }
    })
}

/// The blob and submodule entries of `commit`'s tree that `paths` keeps, in tree order.
fn list(
    git: &Git,
    commit: &str,
    paths: Option<&[String]>,
    deadline: Instant,
) -> Result<Vec<Entry>, Unmade> {
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
    if let Some(paths) = paths {
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
        // A name that is not UTF-8 cannot be written where file names are UTF-16.
        if !cfg!(unix) && std::str::from_utf8(&entry.path).is_err() {
            return Err(Unmade::Refused(format!(
                "the tree holds the path {:?}, which is not UTF-8 and which bay does not write here",
                String::from_utf8_lossy(&entry.path)
            )));
        }
        if paths.is_none_or(|paths| kept(paths, &entry.path)) {
            entries.push(entry);
        }
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
    let written = write_all(
        &mut BufReader::with_capacity(1 << 16, stdout),
        blobs,
        dir,
        deadline,
        links,
        tick,
    );
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
