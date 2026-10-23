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
//! clone's own `info/exclude` names it, so Git leaves it alone there. Phase 2 gives checks the
//! bytes by identity and lets views fetch them on demand.

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
    /// Runs `<command> <args>`; its stdout, trimmed.
    fn run(&self, args: &[&std::ffi::OsStr]) -> Result<String, String> {
        let mut cmd = crate::run::command(self.work_root, self.command)
            .ok_or("[plugin.assets] command is empty")??;
        cmd.args(args);
        let ran = crate::run::captured(cmd, None, self.timeout)
            .map_err(|fault| format!("the assets plugin failed: {fault}"))?;
        if !ran.status.success() {
            let said = String::from_utf8_lossy(&ran.stderr);
            return Err(format!(
                "the assets plugin exited {}: {}",
                ran.status.code().unwrap_or(-1),
                said.lines().next().unwrap_or("").trim()
            ));
        }
        Ok(String::from_utf8_lossy(&ran.stdout).trim().to_owned())
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

    /// Writes the bytes of `id` to `file`.
    ///
    /// # Errors
    ///
    /// The plugin fails, or the bytes it wrote are not `id`'s.
    pub fn get(&self, id: &str, file: &Path) -> Result<(), String> {
        self.run(&["get".as_ref(), id.as_ref(), file.as_os_str()])?;
        let got = identity(file)?;
        if got != id {
            let _ = std::fs::remove_file(file);
            return Err(format!(
                "the assets plugin wrote bytes {got} for {id}; they were removed"
            ));
        }
        Ok(())
    }
}

/// The assets plugin of `repo`'s config and the policy's file limit at `trunk` in bytes, when
/// the config offloads and the policy sets one.
///
/// # Errors
///
/// The policy cannot be read.
pub fn configured<'a>(repo: &'a Repo, trunk: &str) -> Result<Option<(Plugin<'a>, u64)>, String> {
    let Some(assets) = repo
        .config
        .plugin
        .assets
        .as_ref()
        .filter(|assets| assets.over_limit == crate::config::OverLimit::Offload)
    else {
        return Ok(None);
    };
    let Some(max_mib) = crate::policy::limits_at(&repo.git, trunk)?.max_file_mib else {
        return Ok(None);
    };
    let plugin = Plugin {
        work_root: &repo.work_root,
        command: &assets.command,
        timeout: Duration::from_secs(assets.timeout_s.unwrap_or(DEFAULT_TIMEOUT_S)),
    };
    Ok(Some((plugin, max_mib.saturating_mul(1024 * 1024))))
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
    let Some((plugin, limit)) = configured(repo, &repo.trunk_tracking_ref())? else {
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
    let done = offload(&plugin, view, &new, limit, &exclude_file(repo))?;
    Ok(done.entries.into_iter().map(|entry| entry.path).collect())
}

/// What [`offload`] did to a save.
#[derive(Debug, Default)]
pub struct Offloaded {
    /// The paths whose bytes went to the plugin, each with its manifest entry.
    pub entries: Vec<Entry>,
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
    limit_bytes: u64,
    exclude: &Path,
) -> Result<Offloaded, String> {
    let mut offloaded = Offloaded::default();
    for path in paths {
        let file = work_root.join(path);
        let Ok(meta) = std::fs::metadata(&file) else {
            continue;
        };
        if !meta.is_file() || meta.len() <= limit_bytes {
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
    let current = match std::fs::read_to_string(&manifest) {
        Ok(text) => parse(&text)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(format!("{MANIFEST} cannot be read: {error}")),
    };
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
    use super::{Entry, is_id, parse, text};

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
