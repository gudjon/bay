//! The bay skill: the `skills/bay/` directory a release carries (`DESIGN` § 15.1).
//!
//! The files are embedded at build time, so a released binary installs the skill of its own
//! version. `install` copies them to `$HOME/.claude/skills/bay/`, or to
//! `$CLAUDE_CONFIG_DIR/skills/bay/` when that is set (§ 15.3 item 4, G65), and into every other
//! Claude config directory [`config_dirs`] finds on the machine (G319); doctor compares each
//! copy with them (§ 15.2, G49). Nothing else under a skills directory is read or written
//! (§ 15.4).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::git::Git;

/// Every file of `skills/bay/`, by its path inside that directory. A unit test holds this list
/// to the source directory, so a file added there and not here fails the build's tests.
pub const FILES: &[(&str, &str)] = &[("SKILL.md", include_str!("../skills/bay/SKILL.md"))];

/// The installed copy compared with the skill this bay version installs.
#[derive(Debug, PartialEq, Eq)]
pub enum Installed {
    /// No directory at the installed path.
    Missing,
    /// The same files with the same bytes, and no other file.
    Matches,
    /// A file differs, is missing, or is not one this version installs.
    Differs,
}

/// The variable Claude Code reads for its config directory, in place of `$HOME/.claude`.
pub const CONFIG_DIR_VAR: &str = "CLAUDE_CONFIG_DIR";

/// `CLAUDE_CONFIG_DIR` from this process's environment, when set and not empty.
#[must_use]
pub fn config_dir() -> Option<OsString> {
    std::env::var_os(CONFIG_DIR_VAR).filter(|dir| !dir.is_empty())
}

/// `<config_dir>/skills/bay` when a config directory is given, else `$HOME/.claude/skills/bay`.
#[must_use]
pub fn installed_path(home: &Path, config_dir: Option<&OsStr>) -> PathBuf {
    own_config_dir(home, config_dir).join("skills").join("bay")
}

fn own_config_dir(home: &Path, config_dir: Option<&OsStr>) -> PathBuf {
    config_dir.map_or_else(|| home.join(".claude"), PathBuf::from)
}

/// The multi-valued global Git config key that names a box's other Claude config directories
/// (G319). Each value is one config directory, absolute or starting with `~/`. A directory that
/// holds config directories is not searched, so each one is named.
pub const CONFIG_DIRS_KEY: &str = "bay.claudeConfigDirs";

/// What a Claude Code config directory holds. A `$HOME/.claude-*` directory with none of these
/// (`.claude-worktrees`, say) is not one.
const CONFIG_MARKERS: &[&str] = &["settings.json", ".claude.json", "projects", "skills"];

/// G319: this machine's Claude config directories, the caller's own first (`CLAUDE_CONFIG_DIR`,
/// else `$HOME/.claude`, listed whether or not it exists yet), then every other that exists:
/// `$HOME/.claude`; each `$HOME/.claude-*` that looks like a config directory
/// ([`CONFIG_MARKERS`]); on Linux, the `CLAUDE_CONFIG_DIR` of each running `claude` of this
/// user under this `HOME`; and each value of [`CONFIG_DIRS_KEY`]. Deduplicated by real path.
/// Reads only.
#[must_use]
pub fn config_dirs(home: &Path, git: &Git) -> Vec<PathBuf> {
    let mut found = vec![own_config_dir(home, config_dir().as_deref())];
    let mut others = vec![home.join(".claude")];
    others.extend(home_config_dirs(home));
    others.extend(running_config_dirs(home));
    others.extend(named_config_dirs(home, git));
    found.extend(others.into_iter().filter(|dir| dir.is_dir()));
    let mut seen = BTreeSet::new();
    found.retain(|dir| seen.insert(dir.canonicalize().unwrap_or_else(|_| dir.clone())));
    found
}

/// Writes the skill to `<dir>/skills/bay` for each of `dirs` ([`config_dirs`]), and returns
/// each skill directory written.
///
/// # Errors
///
/// A copy cannot be written ([`install`]).
pub fn install_all(dirs: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut written = Vec::new();
    for dir in dirs {
        let path = dir.join("skills").join("bay");
        install(&path)?;
        written.push(path);
    }
    Ok(written)
}

/// Each `$HOME/.claude-*` directory that holds one of [`CONFIG_MARKERS`].
fn home_config_dirs(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(home) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".claude-"))
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && CONFIG_MARKERS
                    .iter()
                    .any(|marker| path.join(marker).exists())
        })
        .collect();
    dirs.sort();
    dirs
}

/// Each value of [`CONFIG_DIRS_KEY`] in the global Git config, `~/` read as `home`. A value
/// that is not then absolute names no directory and is not read.
fn named_config_dirs(home: &Path, git: &Git) -> Vec<PathBuf> {
    // `git config --get-all` exits 1 for a key that is not set.
    let Ok((0, values, _)) = git.run_status(&["config", "--global", "--get-all", CONFIG_DIRS_KEY])
    else {
        return Vec::new();
    };
    values
        .lines()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .strip_prefix("~/")
                .map_or_else(|| PathBuf::from(value), |rest| home.join(rest))
        })
        .filter(|path| path.is_absolute())
        .collect()
}

/// The `CLAUDE_CONFIG_DIR` of each running `claude` owned by this user whose `HOME` is `home`
/// (a session under another `HOME` is not this caller's), best effort: a process whose files
/// cannot be read is skipped. Another user's processes are never read.
#[cfg(target_os = "linux")]
fn running_config_dirs(home: &Path) -> Vec<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let Ok(me) = std::fs::metadata("/proc/self").map(|meta| meta.uid()) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let is_pid = entry
            .file_name()
            .to_str()
            .is_some_and(|name| !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit()));
        let process = entry.path();
        if !is_pid
            || !std::fs::metadata(&process).is_ok_and(|meta| meta.uid() == me)
            || !is_claude(&process)
        {
            continue;
        }
        if let Some(dir) = std::fs::read(process.join("environ"))
            .ok()
            .and_then(|environ| session_config_dir(&environ, home))
        {
            dirs.push(dir);
        }
    }
    dirs.sort();
    dirs
}

#[cfg(not(target_os = "linux"))]
fn running_config_dirs(_home: &Path) -> Vec<PathBuf> {
    Vec::new()
}

/// Whether the process at `/proc/<pid>` runs `claude`: its `comm`, or the file name of its
/// first argument.
#[cfg(target_os = "linux")]
fn is_claude(process: &Path) -> bool {
    let comm = std::fs::read_to_string(process.join("comm")).unwrap_or_default();
    if comm.trim_end() == "claude" {
        return true;
    }
    std::fs::read(process.join("cmdline")).is_ok_and(|cmdline| {
        let first = cmdline.split(|byte| *byte == 0).next().unwrap_or_default();
        String::from_utf8_lossy(first).rsplit('/').next() == Some("claude")
    })
}

/// The absolute `CLAUDE_CONFIG_DIR` in a process environment (`/proc/<pid>/environ`) whose
/// `HOME` is `home`.
#[cfg(target_os = "linux")]
fn session_config_dir(environ: &[u8], home: &Path) -> Option<PathBuf> {
    let config_prefix = format!("{CONFIG_DIR_VAR}=");
    let mut session_home = None;
    let mut config = None;
    for var in environ.split(|byte| *byte == 0) {
        let var = String::from_utf8_lossy(var);
        if let Some(value) = var.strip_prefix("HOME=") {
            session_home = Some(PathBuf::from(value));
        } else if let Some(value) = var.strip_prefix(config_prefix.as_str()) {
            config = Some(PathBuf::from(value));
        }
    }
    let config = config.filter(|dir| dir.is_absolute())?;
    (session_home.as_deref() == Some(home)).then_some(config)
}

/// Compares the copy at `dir` with [`FILES`]. Reads only.
///
/// # Errors
///
/// A file or directory under `dir` cannot be read.
pub fn compare(dir: &Path) -> Result<Installed, String> {
    // A path that holds anything but a directory (a file, a dangling link) is occupied by
    // something this bay did not install: a mismatch, never "missing" and never a fault.
    match std::fs::symlink_metadata(dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Installed::Missing);
        }
        Err(error) => return Err(format!("cannot read {}: {error}", dir.display())),
        Ok(_) if !dir.is_dir() => return Ok(Installed::Differs),
        Ok(_) => {}
    }
    let mut found = BTreeMap::new();
    read_tree(dir, Path::new(""), &mut found)?;
    let expected: BTreeMap<String, &[u8]> = FILES
        .iter()
        .map(|(path, text)| ((*path).to_owned(), text.as_bytes()))
        .collect();
    let same = found.len() == expected.len()
        && found
            .iter()
            .all(|(path, bytes)| bytes.as_deref() == expected.get(path).copied());
    Ok(if same {
        Installed::Matches
    } else {
        Installed::Differs
    })
}

/// Writes [`FILES`] to `dir` and removes every other entry in it, so a second run replaces the
/// same directory. Each file is written to a temporary name beside it and renamed into place.
///
/// # Errors
///
/// A directory cannot be created, or a file cannot be written, renamed or removed.
pub fn install(dir: &Path) -> Result<(), String> {
    let io = |what: &str, path: &Path, error: std::io::Error| {
        format!("cannot {what} {}: {error}", path.display())
    };
    // A non-directory at the skill path (a file, a dangling link) is replaced, so the recovery
    // doctor names for it ("run install again") works.
    if std::fs::symlink_metadata(dir).is_ok() && !dir.is_dir() {
        std::fs::remove_file(dir).map_err(|error| io("remove", dir, error))?;
    }
    std::fs::create_dir_all(dir).map_err(|error| io("create", dir, error))?;
    for (path, text) in FILES {
        let target = dir.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|error| io("create", parent, error))?;
        }
        let tmp = target.with_extension("bay-install-tmp");
        std::fs::write(&tmp, text).map_err(|error| io("write", &tmp, error))?;
        crate::durable::rename(&tmp, &target).map_err(|error| io("place", &target, error))?;
    }
    remove_extra(dir, Path::new(""))
}

fn remove_extra(dir: &Path, prefix: &Path) -> Result<(), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
        let path = entry.path();
        let relative = prefix.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let removed = if kind.is_dir() {
            if FILES
                .iter()
                .any(|(file, _)| Path::new(file).starts_with(&relative))
            {
                remove_extra(&path, &relative)?;
                continue;
            }
            std::fs::remove_dir_all(&path)
        } else if FILES
            .iter()
            .any(|(file, _)| Path::new(file) == relative.as_path())
        {
            continue;
        } else {
            std::fs::remove_file(&path)
        };
        removed.map_err(|error| format!("cannot remove {}: {error}", path.display()))?;
    }
    Ok(())
}

/// Every entry under `dir` that is not a directory, by its path relative to the copy's root,
/// with its bytes when it is a plain file.
fn read_tree(
    dir: &Path,
    prefix: &Path,
    found: &mut BTreeMap<String, Option<Vec<u8>>>,
) -> Result<(), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
        let path = entry.path();
        let relative = prefix.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if kind.is_dir() {
            read_tree(&path, &relative, found)?;
        } else {
            // A link or special file has no bytes here, so it never matches.
            let bytes = if kind.is_file() {
                Some(
                    std::fs::read(&path)
                        .map_err(|error| format!("cannot read {}: {error}", path.display()))?,
                )
            } else {
                None
            };
            found.insert(relative.to_string_lossy().into_owned(), bytes);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use super::{FILES, Installed, compare, install};

    fn source_files(dir: &Path, prefix: &Path, out: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let relative = prefix.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                source_files(&entry.path(), &relative, out);
            } else {
                out.insert(relative.to_string_lossy().into_owned());
            }
        }
    }

    #[test]
    fn the_embedded_list_is_the_source_directory() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/bay");
        let mut on_disk = BTreeSet::new();
        source_files(&root, Path::new(""), &mut on_disk);
        let embedded: BTreeSet<String> = FILES.iter().map(|(path, _)| (*path).to_owned()).collect();
        assert_eq!(on_disk, embedded);
        for (path, text) in FILES {
            assert_eq!(std::fs::read_to_string(root.join(path)).unwrap(), *text);
        }
    }

    #[test]
    fn install_then_compare_matches() {
        let dir = tempfile::TempDir::new().unwrap();
        let copy = dir.path().join("bay");
        assert_eq!(compare(&copy).unwrap(), Installed::Missing);
        install(&copy).unwrap();
        assert_eq!(compare(&copy).unwrap(), Installed::Matches);
        std::fs::create_dir_all(copy.join("old")).unwrap();
        std::fs::write(copy.join("old/notes.md"), "x").unwrap();
        assert_eq!(compare(&copy).unwrap(), Installed::Differs);
        install(&copy).unwrap();
        assert_eq!(compare(&copy).unwrap(), Installed::Matches);
    }

    /// G319: a running session's config directory counts only under this `HOME`, and only
    /// when it is absolute.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_session_config_dir_counts_only_under_this_home() {
        let environ = b"PATH=/bin\0HOME=/h\0CLAUDE_CONFIG_DIR=/h/.claude-two\0";
        assert_eq!(
            super::session_config_dir(environ, Path::new("/h")),
            Some(Path::new("/h/.claude-two").to_path_buf())
        );
        assert_eq!(
            super::session_config_dir(environ, Path::new("/other")),
            None
        );
        let relative = b"HOME=/h\0CLAUDE_CONFIG_DIR=two\0";
        assert_eq!(super::session_config_dir(relative, Path::new("/h")), None);
    }
}
