//! The bay skill: the `skills/bay/` directory a release carries (`DESIGN` § 15.1).
//!
//! The files are embedded at build time, so a released binary installs the skill of its own
//! version. `install` copies them to `$HOME/.claude/skills/bay/`, or to
//! `$CLAUDE_CONFIG_DIR/skills/bay/` when that is set (§ 15.3 item 4, G65); doctor compares that
//! copy with them (§ 15.2, G49). Nothing else under the skills directory is read or written
//! (§ 15.4), and no other config directory is looked for.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

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
    config_dir
        .map_or_else(|| home.join(".claude"), PathBuf::from)
        .join("skills")
        .join("bay")
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
}
