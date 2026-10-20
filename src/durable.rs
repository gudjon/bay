//! Replacing a file so that a crash leaves either the old file or the whole new one (G231).
//!
//! Every file bay writes in place of another goes through [`rename`]: the new file's bytes
//! reach the disk before the rename, and the rename reaches the disk after it. A rename of a
//! file whose bytes are still in the page cache can leave an empty file after a crash.

use std::io;
use std::path::{Path, PathBuf};

/// Moves the written file `from` to `to`, durably: `from` is synced, renamed onto `to`, and on
/// Unix the directory holding `to` is synced too. `from` and `to` are on one file system.
///
/// # Errors
///
/// `from` cannot be opened or synced, or the rename fails. A directory that cannot be synced
/// is no error: the rename itself is done, and some file systems refuse a directory sync.
pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let (from, to) = (from.as_ref(), to.as_ref());
    sync(from)?;
    std::fs::rename(from, to)?;
    #[cfg(unix)]
    if let Some(dir) = to.parent().filter(|dir| !dir.as_os_str().is_empty())
        && let Ok(dir) = std::fs::File::open(dir)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Brings the bytes of the file at `path` to the disk. Windows flushes only through a handle
/// that may write, so there the file is opened for writing; a file that refuses that (one with
/// the read-only attribute) is only opened, as it must exist.
fn sync(path: &Path) -> io::Result<()> {
    if cfg!(windows) {
        match std::fs::OpenOptions::new().write(true).open(path) {
            Ok(file) => file.sync_all(),
            Err(_) => std::fs::File::open(path).map(drop),
        }
    } else {
        std::fs::File::open(path)?.sync_all()
    }
}

/// Places the written program `from` at `to`, where a program may be running (plan W3). On
/// Unix this is [`rename`]: a running program's file is replaced as any other is. Windows
/// neither replaces nor removes a running `.exe`, but it renames one, so there `to` first moves
/// aside ([`aside`]: `bay.exe` to `bay.old.exe`), then `from` takes its name; when that fails,
/// `to` moves back. The next replacement removes what was moved aside once it no longer runs.
///
/// # Errors
///
/// `from` cannot be synced, `to` cannot move aside, or `from` cannot take its name.
pub fn replace_program(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let (from, to) = (from.as_ref(), to.as_ref());
    if cfg!(windows) && to.exists() {
        replace_aside(from, to, std::process::id())
    } else {
        rename(from, to)
    }
}

/// The names a program at `to` moves aside to, in turn: `<stem>.old<.ext>`, and, when an older
/// one there cannot be removed because it still runs, `<stem>.old.<pid><.ext>`.
#[must_use]
pub fn aside(to: &Path, pid: u32) -> [PathBuf; 2] {
    let stem = to
        .file_stem()
        .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
    let ext = to
        .extension()
        .map_or_else(String::new, |ext| format!(".{}", ext.to_string_lossy()));
    [
        to.with_file_name(format!("{stem}.old{ext}")),
        to.with_file_name(format!("{stem}.old.{pid}{ext}")),
    ]
}

/// [`replace_program`]'s Windows plan, on any platform: `to` moves aside, `from` takes its name.
fn replace_aside(from: &Path, to: &Path, pid: u32) -> io::Result<()> {
    sync(from)?;
    let [old, unique] = aside(to, pid);
    let away = match std::fs::remove_file(&old) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            let _ = std::fs::remove_file(&unique);
            unique
        }
        _ => old,
    };
    std::fs::rename(to, &away)?;
    if let Err(error) = std::fs::rename(from, to) {
        let _ = std::fs::rename(&away, to);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// G231: no source file renames a file except through [`super::rename`].
    #[test]
    fn every_file_bay_writes_survives_a_crash_whole() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut direct = Vec::new();
        let mut stack = vec![src];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs")
                    && !path.ends_with("durable.rs")
                {
                    let text = std::fs::read_to_string(&path).unwrap();
                    for (number, line) in text.lines().enumerate() {
                        if line.contains("fs::rename(") {
                            direct.push(format!("{}:{}", path.display(), number + 1));
                        }
                    }
                }
            }
        }
        assert!(
            direct.is_empty(),
            "a rename outside crate::durable::rename can leave an empty file after a crash: {direct:?}"
        );
    }

    #[test]
    fn a_rename_moves_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join(".new"), dir.path().join("file"));
        std::fs::write(&to, "old").unwrap();
        std::fs::write(&from, "new").unwrap();
        super::rename(&from, &to).unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "new");
        assert!(!from.exists());
    }

    #[test]
    fn a_program_moves_aside_by_its_name() {
        let dir = std::path::Path::new("bin");
        assert_eq!(
            super::aside(&dir.join("bay.exe"), 7),
            [dir.join("bay.old.exe"), dir.join("bay.old.7.exe")]
        );
        assert_eq!(
            super::aside(&dir.join("bay"), 7),
            [dir.join("bay.old"), dir.join("bay.old.7")]
        );
    }

    /// Plan W3: the Windows swap of a running program, run here as plain renames. The program
    /// moves aside, the new one takes its name, and the next swap reuses the aside name.
    #[test]
    fn a_running_program_is_replaced_by_moving_it_aside() {
        let dir = tempfile::tempdir().unwrap();
        let (to, from) = (dir.path().join("bay.exe"), dir.path().join(".bay.new"));
        std::fs::write(&to, "one").unwrap();
        std::fs::write(&from, "two").unwrap();
        super::replace_aside(&from, &to, 7).unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "two");
        let old = dir.path().join("bay.old.exe");
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "one");
        assert!(!from.exists());

        std::fs::write(&from, "three").unwrap();
        super::replace_aside(&from, &to, 7).unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "three");
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "two");

        // A new program that is not there leaves the old one in its place.
        let missing = dir.path().join(".bay.missing");
        assert!(super::replace_aside(&missing, &to, 7).is_err());
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "three");
    }
}
