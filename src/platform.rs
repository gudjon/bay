//! What differs between Unix and Windows where bay starts a process or names a path (`BACKLOG`
//! Launch requirements, plan W1). On Unix each of these is what bay did before it built for
//! Windows: the home directory is `$HOME`, a program is found by the system's own lookup, and a
//! detached process leads a process group of its own.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// This user's home directory: `HOME`, and on Windows, where `HOME` is usually unset,
/// `USERPROFILE`. `None` when neither is set, so a caller names what it cannot find.
#[must_use]
pub fn home() -> Option<OsString> {
    let home = std::env::var_os("HOME");
    if cfg!(windows) {
        home.filter(|home| !home.is_empty())
            .or_else(|| std::env::var_os("USERPROFILE").filter(|home| !home.is_empty()))
    } else {
        home
    }
}

/// The program to start for `name`. On Windows a bare name is looked up on `PATH` with each
/// extension of `PATHEXT` (so `claude` finds `claude.cmd`, which `Command` alone does not);
/// a name not found is returned as it is, so starting it fails as not found. On Unix the name
/// is returned as it is.
#[must_use]
pub fn program(name: &str) -> PathBuf {
    if cfg!(windows) {
        resolve(
            name,
            std::env::var_os("PATH").as_deref(),
            std::env::var_os("PATHEXT").as_deref(),
        )
        .unwrap_or_else(|| PathBuf::from(name))
    } else {
        PathBuf::from(name)
    }
}

/// The Windows lookup of `name` on `path` with the extensions of `pathext` (`;`-separated,
/// `.COM;.EXE;.BAT;.CMD` when unset): in each directory of `path` in order, the name as given
/// when it already has an extension of `pathext`, else the name with each extension in turn.
/// A name with a directory in it is not looked up.
#[must_use]
pub fn resolve(name: &str, path: Option<&OsStr>, pathext: Option<&OsStr>) -> Option<PathBuf> {
    if name.is_empty() || name.contains(['/', '\\']) {
        return None;
    }
    let pathext = pathext
        .and_then(OsStr::to_str)
        .filter(|text| !text.trim().is_empty())
        .unwrap_or(".COM;.EXE;.BAT;.CMD");
    let extensions: Vec<&str> = pathext
        .split(';')
        .map(str::trim)
        .filter(|ext| ext.starts_with('.') && ext.len() > 1)
        .collect();
    let has_extension = Path::new(name)
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| {
            extensions
                .iter()
                .any(|known| known[1..].eq_ignore_ascii_case(ext))
        });
    let candidates: Vec<String> = if has_extension {
        vec![name.to_owned()]
    } else {
        extensions
            .iter()
            .map(|ext| format!("{name}{}", ext.to_ascii_lowercase()))
            .collect()
    };
    std::env::split_paths(path?)
        .filter(|dir| !dir.as_os_str().is_empty())
        .find_map(|dir| {
            candidates
                .iter()
                .map(|candidate| dir.join(candidate))
                .find(|candidate| candidate.is_file())
        })
}

/// Detaches `cmd` from the caller as a process of its own that outlives it: on Unix the leader
/// of a new process group, on Windows a detached process in a new process group.
pub fn detach(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    detach_windows(cmd);
}

/// [`detach`] for a process bay started detached on Windows only: on Unix nothing changes.
pub fn detach_windows(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

#[cfg(test)]
mod tests {
    use super::resolve;
    use std::ffi::OsStr;

    fn dirs() -> (tempfile::TempDir, tempfile::TempDir) {
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
    }

    fn joined(dirs: &[&std::path::Path]) -> std::ffi::OsString {
        std::env::join_paths(dirs).unwrap()
    }

    #[test]
    fn a_bare_name_is_found_through_pathext_in_path_order() {
        let (first, second) = dirs();
        std::fs::write(second.path().join("claude.cmd"), "").unwrap();
        let path = joined(&[first.path(), second.path()]);
        assert_eq!(
            resolve(
                "claude",
                Some(&path),
                Some(OsStr::new(".COM;.EXE;.BAT;.CMD"))
            ),
            Some(second.path().join("claude.cmd"))
        );
        // An earlier directory wins, and within it the earlier extension.
        std::fs::write(first.path().join("claude.bat"), "").unwrap();
        std::fs::write(first.path().join("claude.exe"), "").unwrap();
        assert_eq!(
            resolve(
                "claude",
                Some(&path),
                Some(OsStr::new(".COM;.EXE;.BAT;.CMD"))
            ),
            Some(first.path().join("claude.exe"))
        );
    }

    #[test]
    fn an_unset_pathext_takes_the_windows_default() {
        let (dir, _) = dirs();
        std::fs::write(dir.path().join("claude.cmd"), "").unwrap();
        let path = joined(&[dir.path()]);
        assert_eq!(
            resolve("claude", Some(&path), None),
            Some(dir.path().join("claude.cmd"))
        );
    }

    #[test]
    fn a_name_with_a_known_extension_is_looked_up_as_given() {
        let (dir, _) = dirs();
        std::fs::write(dir.path().join("git.exe"), "").unwrap();
        let path = joined(&[dir.path()]);
        let pathext = Some(OsStr::new(".EXE;.CMD"));
        assert_eq!(
            resolve("git.exe", Some(&path), pathext),
            Some(dir.path().join("git.exe"))
        );
        assert_eq!(resolve("git.cmd", Some(&path), pathext), None);
    }

    #[test]
    fn nothing_found_is_none_never_a_guess() {
        let (dir, _) = dirs();
        // A bare file with no extension is not what Windows would run.
        std::fs::write(dir.path().join("claude"), "").unwrap();
        // A directory is not a program.
        std::fs::create_dir(dir.path().join("claude.cmd")).unwrap();
        let path = joined(&[dir.path()]);
        assert_eq!(resolve("claude", Some(&path), None), None);
        assert_eq!(resolve("claude", None, None), None);
        assert_eq!(resolve("", Some(&path), None), None);
        assert_eq!(resolve("bin/claude", Some(&path), None), None);
    }
}
