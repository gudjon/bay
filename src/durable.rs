//! Replacing a file so that a crash leaves either the old file or the whole new one (G231).
//!
//! Every file bay writes in place of another goes through [`rename`]: the new file's bytes
//! reach the disk before the rename, and the rename reaches the disk after it. A rename of a
//! file whose bytes are still in the page cache can leave an empty file after a crash.

use std::io;
use std::path::Path;

/// Moves the written file `from` to `to`, durably: `from` is synced, renamed onto `to`, and on
/// Unix the directory holding `to` is synced too. `from` and `to` are on one file system.
///
/// # Errors
///
/// `from` cannot be opened or synced, or the rename fails. A directory that cannot be synced
/// is no error: the rename itself is done, and some file systems refuse a directory sync.
pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let (from, to) = (from.as_ref(), to.as_ref());
    std::fs::File::open(from)?.sync_all()?;
    std::fs::rename(from, to)?;
    #[cfg(unix)]
    if let Some(dir) = to.parent().filter(|dir| !dir.as_os_str().is_empty())
        && let Ok(dir) = std::fs::File::open(dir)
    {
        let _ = dir.sync_all();
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
}
