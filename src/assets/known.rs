//! BACKLOG 295: what bay last knew of each pinned file in one folder (a view or a main checkout),
//! so a freeze hashes only the files whose stat moved since, as Git's index spares it the
//! unchanged files of a status.
//!
//! The record is the folder's own: `bay-assets-known` in its Git directory, so it goes when the
//! folder goes. Each line is `<identity> <size> <modified ns> <changed ns> <inode> <path>`. A
//! record is only ever a shortcut: one that is missing, unreadable or cannot be written costs a
//! hash, never a wrong answer. A stat as new as the record's own write is not trusted (an edit in
//! the same tick of the clock would keep the stat), as Git treats a racily clean entry.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The record's name in a folder's Git directory.
const NAME: &str = "bay-assets-known";

/// What bay compares to see that a file did not move: its size, its modification and change
/// times and its inode. The change time moves on every write and cannot be set back, so a copy
/// that keeps the old modification time still reads as moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    size: u64,
    modified: u128,
    changed: u128,
    inode: u64,
}

impl Stat {
    #[must_use]
    pub fn of(meta: &std::fs::Metadata) -> Self {
        let modified = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |time| time.as_nanos());
        #[cfg(unix)]
        let (changed, inode) = {
            use std::os::unix::fs::MetadataExt as _;
            let seconds = u128::try_from(meta.ctime()).unwrap_or(0);
            let nanos = u128::try_from(meta.ctime_nsec()).unwrap_or(0);
            (seconds * 1_000_000_000 + nanos, meta.ino())
        };
        #[cfg(not(unix))]
        let (changed, inode) = (0, 0);
        Self {
            size: meta.len(),
            modified,
            changed,
            inode,
        }
    }
}

/// What the record says of one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seen<'a> {
    /// Its stat is the one recorded with these bytes: it did not move since.
    Holds(&'a str),
    /// It was written since bay recorded it.
    Moved,
    /// There is no record of it, or the record cannot tell.
    Unknown,
}

/// The record of one folder.
#[derive(Debug, Default)]
pub struct Known {
    files: BTreeMap<String, (String, Stat)>,
    /// When the record was last written, in nanoseconds: a stat this new or newer is not trusted.
    written: u128,
    learned: bool,
}

impl Known {
    /// The record of the folder at `root`, empty when there is none or it cannot be read.
    #[must_use]
    pub fn of(root: &Path) -> Self {
        let Some(file) = file_of(root) else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&file) else {
            return Self::default();
        };
        let written = std::fs::metadata(&file)
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |time| time.as_nanos());
        let mut files = BTreeMap::new();
        for line in text.lines() {
            let mut fields = line.splitn(6, ' ');
            let (Some(id), Some(size), Some(modified), Some(changed), Some(inode), Some(path)) = (
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
            ) else {
                continue;
            };
            let (Ok(size), Ok(modified), Ok(changed), Ok(inode)) = (
                size.parse(),
                modified.parse(),
                changed.parse(),
                inode.parse(),
            ) else {
                continue;
            };
            let stat = Stat {
                size,
                modified,
                changed,
                inode,
            };
            files.insert(path.to_owned(), (id.to_owned(), stat));
        }
        Self {
            files,
            written,
            learned: false,
        }
    }

    /// What the record says of `path`, whose stat is `stat` now.
    #[must_use]
    pub fn seen(&self, path: &str, stat: &Stat) -> Seen<'_> {
        match self.files.get(path) {
            None => Seen::Unknown,
            Some((id, known)) if known == stat => {
                if stat.modified < self.written && stat.changed < self.written {
                    Seen::Holds(id)
                } else {
                    // Racily the same: an edit in the tick of the record's write keeps the stat.
                    Seen::Unknown
                }
            }
            Some(_) => Seen::Moved,
        }
    }

    /// Records that `path`, with `stat`, holds the bytes of `id`.
    pub fn learn(&mut self, path: &str, id: &str, stat: Stat) {
        self.files.insert(path.to_owned(), (id.to_owned(), stat));
        self.learned = true;
    }

    /// Writes the record into the folder at `root` when something was learned. Best effort: a
    /// record that cannot be written only costs a hash at the next freeze.
    pub fn keep(&self, root: &Path) {
        if !self.learned {
            return;
        }
        let Some(file) = file_of(root) else {
            return;
        };
        let mut text = String::new();
        for (path, (id, stat)) in &self.files {
            let _ = writeln!(
                text,
                "{id} {} {} {} {} {path}",
                stat.size, stat.modified, stat.changed, stat.inode
            );
        }
        let tmp = file.with_file_name(format!("{NAME}.{}.tmp", std::process::id()));
        if std::fs::write(&tmp, text).is_err() || crate::durable::rename(&tmp, &file).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// The record's file for the folder at `root`: in the Git directory its `.git` names, or none
/// when `root` has no `.git` of its own (a check's scratch tree).
fn file_of(root: &Path) -> Option<PathBuf> {
    let dot = root.join(".git");
    if dot.is_dir() {
        return Some(dot.join(NAME));
    }
    let link = std::fs::read_to_string(&dot).ok()?;
    let own = root.join(link.trim().strip_prefix("gitdir:")?.trim());
    own.is_dir().then(|| own.join(NAME))
}
