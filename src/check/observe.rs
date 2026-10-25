//! Observed reads (items 1 and 2, owner ruling 2026-10-25): what a check with
//! `footprint = "observed"` read and listed in its tree, so its record is reused only on a tree
//! that holds the same bytes there, with no read set declared by hand.
//!
//! bay writes the check's tree fresh. [`arm`] then sets every file's and directory's times
//! [`BACK`] in the past, so any read lands strictly after them, whatever the kernel's
//! timestamp granularity. Under `relatime`, the Linux default, the first read of a file whose
//! access time is not after its write time moves its access time, and a directory listing does
//! the same for the directory. After the run, [`collect`] names every entry whose access or
//! write time moved past the armed time: a write counts as a read too, since a check that reads
//! a file and then rewrites it leaves its access time behind its write time.
//!
//! [`arm`] first proves the mount moves access times, on a probe file of its own outside the
//! tree. A mount with `noatime`, or a filesystem that ignores access times, observes nothing:
//! the record gets no footprint and is never reused (unknown is never a pass).

use std::fs::{File, FileTimes};
use std::path::Path;
use std::time::{Duration, SystemTime};

/// How far [`arm`] sets times back: more than any filesystem's timestamp granularity.
pub const BACK: Duration = Duration::from_secs(2);

/// What [`arm`] left: the armed time, after which a moved time means the check touched it.
#[derive(Clone, Copy, Debug)]
pub struct Armed {
    pub at: SystemTime,
}

/// Sets the times of every entry of `tree` back by [`BACK`] and proves, on a probe in
/// `scratch` (outside the tree), that a read moves an access time there. `None` when it does
/// not, or when a time cannot be set: nothing is observed then.
#[must_use]
pub fn arm(tree: &Path, scratch: &Path) -> Option<Armed> {
    let at = SystemTime::now().checked_sub(BACK)?;
    if !probe(scratch, at) {
        return None;
    }
    let times = FileTimes::new().set_accessed(at).set_modified(at);
    // Files first, then their directories, deepest first: setting a file's times never moves
    // its directory's, and a directory is set after its entries.
    let mut dirs = Vec::new();
    walk(tree, &mut |path, is_dir| {
        if is_dir {
            dirs.push(path.to_path_buf());
            true
        } else {
            set(path, times)
        }
    })?;
    dirs.push(tree.to_path_buf());
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
    for dir in dirs {
        if !set(&dir, times) {
            return None;
        }
    }
    Some(Armed { at })
}

/// Whether a read of a fresh file under `scratch`, armed at `at`, moves its access time.
fn probe(scratch: &Path, at: SystemTime) -> bool {
    let path = scratch.join(".bay-observe-probe");
    let armed = std::fs::write(&path, b"probe").is_ok()
        && set(&path, FileTimes::new().set_accessed(at).set_modified(at));
    let moved = armed
        && std::fs::read(&path).is_ok()
        && std::fs::metadata(&path)
            .and_then(|meta| meta.accessed())
            .is_ok_and(|accessed| accessed > at);
    let _ = std::fs::remove_file(&path);
    moved
}

fn set(path: &Path, times: FileTimes) -> bool {
    File::open(path)
        .and_then(|file| file.set_times(times))
        .is_ok()
}

/// What the check touched under `tree` since `armed`: the files it read (or wrote) and the
/// directories it listed (or changed), as `/`-separated paths relative to `tree` (`""` for
/// the tree itself), sorted. `None` when the tree cannot be read whole: then nothing is known.
#[must_use]
pub fn collect(tree: &Path, armed: Armed) -> Option<(Vec<String>, Vec<String>)> {
    let touched = |path: &Path| {
        std::fs::symlink_metadata(path).is_ok_and(|meta| {
            meta.accessed().is_ok_and(|time| time > armed.at)
                || meta.modified().is_ok_and(|time| time > armed.at)
        })
    };
    let relative = |path: &Path| -> String {
        path.strip_prefix(tree)
            .map(|rel| {
                rel.components()
                    .map(|part| part.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .unwrap_or_default()
    };
    let (mut read, mut listed) = (Vec::new(), Vec::new());
    if touched(tree) {
        listed.push(String::new());
    }
    walk(tree, &mut |path, is_dir| {
        if touched(path) {
            if is_dir {
                listed.push(relative(path));
            } else {
                read.push(relative(path));
            }
        }
        true
    })?;
    read.sort();
    listed.sort();
    Some((read, listed))
}

/// Calls `visit(path, is_dir)` for every entry under `dir`, depth first, never following a
/// symbolic link. `None` when a directory cannot be read or `visit` says no.
fn walk(dir: &Path, visit: &mut dyn FnMut(&Path, bool) -> bool) -> Option<()> {
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        let kind = entry.file_type().ok()?;
        let path = entry.path();
        if kind.is_symlink() {
            continue;
        }
        if !visit(&path, kind.is_dir()) {
            return None;
        }
        if kind.is_dir() {
            walk(&path, visit)?;
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::{arm, collect};

    #[test]
    fn what_a_reader_touched_is_named_and_nothing_else() {
        let scratch = tempfile::tempdir().unwrap();
        let tree = scratch.path().join("tree");
        std::fs::create_dir_all(tree.join("dir")).unwrap();
        std::fs::create_dir_all(tree.join("quiet")).unwrap();
        std::fs::write(tree.join("a.txt"), "a").unwrap();
        std::fs::write(tree.join("b.txt"), "b").unwrap();
        std::fs::write(tree.join("dir/c.txt"), "c").unwrap();
        std::fs::write(tree.join("quiet/d.txt"), "d").unwrap();
        let Some(armed) = arm(&tree, scratch.path()) else {
            // A box whose mount does not move access times observes nothing: no claim here.
            return;
        };
        let _ = std::fs::read(tree.join("a.txt")).unwrap();
        let _ = std::fs::read_dir(tree.join("dir")).unwrap().count();
        let (read, listed) = collect(&tree, armed).unwrap();
        assert_eq!(read, ["a.txt"]);
        assert_eq!(listed, ["dir"]);
    }
}
