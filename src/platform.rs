//! What differs between Unix and Windows where bay starts a process or names a path (`BACKLOG`
//! Launch requirements, plan W1). On Unix each of these is what bay did before it built for
//! Windows: the home directory is `$HOME`, a program is found by the system's own lookup, and a
//! detached process leads a process group of its own.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

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

/// Waits for `child` on a thread of its own, so a long-running parent keeps no exited child
/// (G575, a fleet finding, 2026-10-29: a runner held 32 zombies, one for each hourly
/// `bay update` it started). A short-lived parent exits first and leaves the child to init.
pub fn reap(mut child: Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// A search path from `dirs`, joined with this platform's separator: `;` on Windows, where a
/// directory holds a `:` after its drive letter, `:` elsewhere.
#[must_use]
pub fn search_path(dirs: &[String]) -> String {
    dirs.join(if cfg!(windows) { ";" } else { ":" })
}

/// How an adapter's program starts, read from its first line (plan W6).
#[derive(Debug, PartialEq, Eq)]
pub enum Launch {
    /// No `#!` line: the program starts by itself.
    Direct,
    /// A `#!` line naming `sh` or `bash`, with the one argument the line passes, if any: on
    /// Windows the script runs through Git for Windows' `sh.exe`.
    Sh(Option<String>),
    /// A `#!` line naming another interpreter, as written.
    Other(String),
}

/// The [`Launch`] of a program whose first bytes are `head`. A `#!` line is the interpreter
/// path and at most one argument, as Unix reads it; `/usr/bin/env sh` names `sh`.
#[must_use]
pub fn launch_of(head: &[u8]) -> Launch {
    let Some(line) = head.strip_prefix(b"#!") else {
        return Launch::Direct;
    };
    let line = line.split(|byte| *byte == b'\n').next().unwrap_or_default();
    let line = String::from_utf8_lossy(line);
    let line = line.trim();
    let (interpreter, arg) = line
        .split_once([' ', '\t'])
        .map_or((line, None), |(interpreter, arg)| {
            (interpreter, Some(arg.trim()).filter(|arg| !arg.is_empty()))
        });
    let name = |path: &str| {
        path.rsplit(['/', '\\'])
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    let shell = |name: &str| matches!(name, "sh" | "bash");
    match (name(interpreter).as_str(), arg) {
        ("env", Some(program)) if shell(program) => Launch::Sh(None),
        (program, arg) if shell(program) => Launch::Sh(arg.map(str::to_owned)),
        _ => Launch::Other(line.to_owned()),
    }
}

/// Where Git for Windows keeps `sh.exe`, given Git's exec path (`git --exec-path`, as
/// `C:/Program Files/Git/mingw64/libexec/git-core`): `bin/sh.exe`, then `usr/bin/sh.exe`, in
/// each directory above the exec path, nearest first.
#[must_use]
pub fn sh_candidates(exec_path: &Path) -> Vec<PathBuf> {
    exec_path
        .ancestors()
        .skip(1)
        .take(3)
        .flat_map(|dir| {
            [
                dir.join("bin").join("sh.exe"),
                dir.join("usr").join("bin").join("sh.exe"),
            ]
        })
        .collect()
}

/// Why a shebang adapter does not start on Windows when no `sh.exe` is found.
pub const NO_SH: &str = "the plugin is a #! script, and Windows runs one only through \
     Git for Windows' sh.exe, which was not found above git --exec-path";

/// The command that starts the adapter program at `program`. On Unix the program itself. On
/// Windows a `#!` script naming `sh` or `bash` runs through Git for Windows' `sh.exe` (plan W6);
/// a script naming another interpreter, or one with no `sh.exe` to run it, is refused by name.
/// A program that cannot be read starts as it is, so starting it fails as it would.
///
/// # Errors
///
/// On Windows only: a `#!` script that has no interpreter bay can start.
pub fn script(program: &Path) -> Result<Command, String> {
    if !cfg!(windows) {
        return Ok(Command::new(program));
    }
    let mut head = [0u8; 256];
    let read = std::fs::File::open(program)
        .and_then(|mut file| std::io::Read::read(&mut file, &mut head))
        .unwrap_or(0);
    match launch_of(&head[..read]) {
        Launch::Direct => Ok(Command::new(program)),
        Launch::Sh(arg) => {
            let sh = git_sh().ok_or_else(|| format!("{}: {NO_SH}", program.display()))?;
            let mut cmd = Command::new(sh);
            cmd.args(arg).arg(program);
            Ok(cmd)
        }
        Launch::Other(line) => Err(format!(
            "{}: the plugin's #! line names {line}, and on Windows bay runs only a sh or bash \
             script (through Git for Windows' sh.exe) or a program that starts by itself",
            program.display()
        )),
    }
}

/// Git for Windows' `sh.exe`, found once from `git --exec-path`.
fn git_sh() -> Option<PathBuf> {
    static SH: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    SH.get_or_init(|| {
        let mut cmd = Command::new("git");
        cmd.arg("--exec-path");
        let output = crate::git::run_bounded(cmd, "git --exec-path").ok()?;
        let exec_path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !output.status.success() || exec_path.is_empty() {
            return None;
        }
        sh_candidates(Path::new(&exec_path))
            .into_iter()
            .find(|candidate| candidate.is_file())
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::{Launch, launch_of, reap, resolve, sh_candidates};
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

    #[test]
    fn a_shebang_names_how_the_adapter_starts() {
        assert_eq!(launch_of(b"\x7fELF\x02"), Launch::Direct);
        assert_eq!(launch_of(b"MZ\x90\x00"), Launch::Direct);
        assert_eq!(launch_of(b""), Launch::Direct);
        assert_eq!(launch_of(b"#!/bin/sh\nexit 0\n"), Launch::Sh(None));
        assert_eq!(launch_of(b"#!/bin/bash\r\n"), Launch::Sh(None));
        assert_eq!(
            launch_of(b"#! /bin/sh -eu\nset -x\n"),
            Launch::Sh(Some("-eu".to_owned()))
        );
        assert_eq!(launch_of(b"#!/usr/bin/env sh\n"), Launch::Sh(None));
        assert_eq!(launch_of(b"#!/usr/bin/env bash\n"), Launch::Sh(None));
        assert_eq!(
            launch_of(b"#!/usr/bin/env python3\n"),
            Launch::Other("/usr/bin/env python3".to_owned())
        );
        assert_eq!(
            launch_of(b"#!/usr/bin/zsh\n"),
            Launch::Other("/usr/bin/zsh".to_owned())
        );
        // A first line cut short by the read still names its interpreter.
        assert_eq!(launch_of(b"#!/bin/sh"), Launch::Sh(None));
    }

    #[test]
    fn sh_exe_is_looked_for_above_git_s_exec_path() {
        let at = |parts: &[&str]| parts.iter().collect::<std::path::PathBuf>();
        // Git for Windows: the exec path is `<root>/mingw64/libexec/git-core`, and its shell
        // is `<root>/bin/sh.exe`, with `<root>/usr/bin/sh.exe` beside it.
        let found = sh_candidates(&at(&["Git", "mingw64", "libexec", "git-core"]));
        assert_eq!(
            found,
            [
                at(&["Git", "mingw64", "libexec", "bin", "sh.exe"]),
                at(&["Git", "mingw64", "libexec", "usr", "bin", "sh.exe"]),
                at(&["Git", "mingw64", "bin", "sh.exe"]),
                at(&["Git", "mingw64", "usr", "bin", "sh.exe"]),
                at(&["Git", "bin", "sh.exe"]),
                at(&["Git", "usr", "bin", "sh.exe"]),
            ]
        );
    }

    #[test]
    fn a_search_path_takes_this_platform_s_separator() {
        let dirs = ["/usr/bin".to_owned(), "/bin".to_owned()];
        let joined = super::search_path(&dirs);
        let split: Vec<_> = std::env::split_paths(&joined).collect();
        assert_eq!(
            split,
            [
                std::path::Path::new("/usr/bin"),
                std::path::Path::new("/bin")
            ]
        );
    }

    /// On Windows a `#!/bin/sh` adapter starts through `sh.exe` from Git for Windows.
    #[cfg(windows)]
    #[test]
    fn a_sh_script_starts_through_git_s_sh_exe_on_windows() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("adapter");
        std::fs::write(&script, "#!/bin/sh\necho ok\n").unwrap();
        let mut cmd = super::script(&script).unwrap();
        assert!(
            std::path::Path::new(cmd.get_program())
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("sh.exe"))
        );
        let output = cmd.output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
        std::fs::write(&script, "#!/usr/bin/env python3\n").unwrap();
        assert!(super::script(&script).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_reaped_child_leaves_no_zombie() {
        // G575: an exited child bay never waits for stays in the process table.
        let child = std::process::Command::new("true").spawn().unwrap();
        let entry = std::path::PathBuf::from(format!("/proc/{}", child.id()));
        reap(child);
        let started = std::time::Instant::now();
        while entry.exists() && started.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            !entry.exists(),
            "{} is still in the process table",
            entry.display()
        );
    }
}
