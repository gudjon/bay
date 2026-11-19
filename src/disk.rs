//! The box's disk floor (G265): Git config `bay.minFreeMiB`, global or for one clone, and
//! [`DEFAULT_FLOOR_MIB`] when neither sets it (G655). `bay start` makes no view and check materialization writes no tree while the free
//! space of the filesystem it would write to is below the floor: both refuse `DISK_LOW`,
//! naming the free space, the floor and the path. A box setting, never a repository one: a
//! repository never declares how full a box's disk may get. `0` turns it off.
//!
//! Free space is what `df -P` reports as available to this user, in 1024-byte blocks. On
//! Windows the floor is not judged.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::git::Git;

/// The box's floor of free space, in mebibytes.
pub const FLOOR_KEY: &str = "bay.minFreeMiB";

/// G655 (owner ruling, 2026-11-05): the floor of a box that set none: 5 GiB, little enough that a
/// small machine meets it only when nearly full.
pub const DEFAULT_FLOOR_MIB: u64 = 5120;

use crate::version::TEST_HOOKS;

/// A filesystem below the box's floor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Low {
    pub free_mib: u64,
    pub floor_mib: u64,
    /// The path whose filesystem was measured.
    pub path: PathBuf,
}

impl Low {
    /// `DISK_LOW`'s detail.
    #[must_use]
    pub fn detail(&self) -> Value {
        json!({
            "free_mib": self.free_mib,
            "floor_mib": self.floor_mib,
            "path": self.path.display().to_string(),
            "setting": FLOOR_KEY,
            "recovery": format!("free space on that filesystem, or lower git config {FLOOR_KEY}"),
        })
    }

    /// The refusal in one line, for a caller that carries a reason as text.
    #[must_use]
    pub fn message(&self) -> String {
        format!(
            "DISK_LOW: {} MiB free at {}, below the box's floor of {} MiB ({FLOOR_KEY})",
            self.free_mib,
            self.path.display(),
            self.floor_mib
        )
    }
}

/// The box's floor as `git` reads it (global, or the clone's own config). Unset is
/// [`DEFAULT_FLOOR_MIB`].
///
/// # Errors
///
/// A value that is not a whole number of mebibytes, or a `git config` that fails: a floor that
/// cannot be read is never no floor.
pub fn floor(git: &Git) -> Result<u64, String> {
    let value = git
        .config_get(FLOOR_KEY)
        .map_err(|error| format!("{FLOOR_KEY} could not be read: {}", error.message()))?;
    let Some(value) = value else {
        return Ok(DEFAULT_FLOOR_MIB);
    };
    let value = value.trim();
    value
        .parse()
        .map_err(|_| format!("{FLOOR_KEY} = \"{value}\" is not a whole number of MiB"))
}

/// Whether the filesystem that holds `path` (or, when `path` does not exist yet, its nearest
/// existing ancestor) is below the box's floor. A floor of 0, or Windows, is never low.
///
/// # Errors
///
/// The floor cannot be read, or the free space cannot be measured.
pub fn below_floor(git: &Git, path: &Path) -> Result<Option<Low>, String> {
    let floor_mib = floor(git)?;
    if cfg!(windows) {
        return Ok(None);
    }
    let measured = existing(path);
    let free_mib = free_mib(&measured)?;
    Ok((free_mib < floor_mib).then_some(Low {
        free_mib,
        floor_mib,
        path: measured,
    }))
}

/// `path`, or its nearest ancestor that exists; `.` when none does.
fn existing(path: &Path) -> PathBuf {
    path.ancestors()
        .find(|ancestor| !ancestor.as_os_str().is_empty() && ancestor.exists())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// The space available to this user on the filesystem holding `path`, in whole mebibytes.
fn free_mib(path: &Path) -> Result<u64, String> {
    // A test build reads a disk of any size: no test can fill the machine's.
    if TEST_HOOKS
        && let Some(free) = std::env::var("BAY_TEST_FREE_MIB")
            .ok()
            .and_then(|free| free.parse().ok())
    {
        return Ok(free);
    }
    let mut cmd = std::process::Command::new("df");
    cmd.arg("-Pk").arg(path);
    let output = crate::git::run_bounded(cmd, "df -Pk").map_err(|error| {
        format!(
            "the free space at {} could not be read: {}",
            path.display(),
            error.message()
        )
    })?;
    if !output.status.success() {
        return Err(format!(
            "the free space at {} could not be read: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    available_kib(&text).map(|kib| kib / 1024).ok_or_else(|| {
        format!(
            "the free space at {} could not be read from df: {}",
            path.display(),
            text.trim()
        )
    })
}

/// The `Available` column of `df -P`'s one data line, in kibibytes. The columns are
/// `Filesystem`, `1024-blocks`, `Used`, `Available`, `Capacity` and `Mounted on`; a filesystem
/// or mount name may hold spaces, so the column is found as the one before the capacity
/// (`NN%`).
fn available_kib(text: &str) -> Option<u64> {
    let line = text.lines().skip(1).find(|line| !line.trim().is_empty())?;
    let words: Vec<&str> = line.split_whitespace().collect();
    // The capacity is never the first word: that is the filesystem's name.
    let (capacity, _) = words.iter().enumerate().skip(1).find(|(_, word)| {
        word.strip_suffix('%')
            .is_some_and(|number| number.parse::<u32>().is_ok())
    })?;
    words.get(capacity - 1)?.parse().ok()
}

/// G792 (owner, 2026-11-19: "lower disk space all over, and bay plays a role"): the bytes `paths`
/// take on disk, each file counted once however many links reach it (a hardlinked install is
/// one copy), never following a symbolic link. Unix counts allocated blocks; elsewhere, file
/// lengths. An unreadable entry counts as nothing.
pub struct Footprint {
    #[cfg(unix)]
    seen: std::collections::HashSet<(u64, u64)>,
}

impl Default for Footprint {
    fn default() -> Self {
        Self::new()
    }
}

impl Footprint {
    #[must_use]
    pub fn new() -> Self {
        Self {
            #[cfg(unix)]
            seen: std::collections::HashSet::new(),
        }
    }

    /// The bytes under `root` not already counted by this footprint.
    pub fn bytes(&mut self, root: &Path) -> u64 {
        let mut total = 0;
        let mut stack = vec![root.to_path_buf()];
        while let Some(path) = stack.pop() {
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                if let Ok(entries) = std::fs::read_dir(&path) {
                    stack.extend(entries.flatten().map(|entry| entry.path()));
                }
            } else if meta.is_file() {
                total += self.counted(&meta);
            }
        }
        total
    }

    #[cfg(unix)]
    fn counted(&mut self, meta: &std::fs::Metadata) -> u64 {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() > 1 && !self.seen.insert((meta.dev(), meta.ino())) {
            return 0;
        }
        meta.blocks() * 512
    }

    #[cfg(not(unix))]
    fn counted(&mut self, meta: &std::fs::Metadata) -> u64 {
        meta.len()
    }
}

/// G792: a view this large holds more than a change's edits, most often build output or
/// installed packages; doctor names it so a person or steward can free it.
pub const LARGE_VIEW_BYTES: u64 = 1 << 30;

/// G792: bay's footprint on this box for the repository at `common_dir`: its object store, bay's
/// own state beside it, and each of its views (`views`: change id and path), largest first.
/// Each root is measured on its own thread, so a file linked into two roots counts in both.
#[must_use]
pub fn footprint(common_dir: &Path, views: &[(String, PathBuf)]) -> Value {
    let mut roots: Vec<PathBuf> = vec![common_dir.join("objects"), common_dir.join("bay")];
    roots.extend(views.iter().map(|(_, path)| path.clone()));
    let sizes: Vec<u64> = std::thread::scope(|scope| {
        let handles: Vec<_> = roots
            .iter()
            .map(|root| scope.spawn(move || Footprint::new().bytes(root)))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap_or(0))
            .collect()
    });
    let (store, state) = (sizes[0], sizes[1]);
    let mut rows: Vec<(String, PathBuf, u64)> = views
        .iter()
        .zip(&sizes[2..])
        .map(|((change, path), bytes)| (change.clone(), path.clone(), *bytes))
        .collect();
    rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    let views_bytes: u64 = rows.iter().map(|row| row.2).sum();
    json!({
        "store_bytes": store,
        "state_bytes": state,
        "views_bytes": views_bytes,
        "total_bytes": store + state + views_bytes,
        "views": rows
            .iter()
            .map(|(change, path, bytes)| json!({
                "change": change,
                "path": path.display().to_string(),
                "bytes": bytes,
            }))
            .collect::<Vec<_>>(),
    })
}

/// `bytes` in plain words: `512 B`, `3.4 KiB`, `6.4 GiB`.
#[must_use]
pub fn plain_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_available_column_is_the_one_before_the_capacity() {
        let df = "Filesystem     1024-blocks      Used Available Capacity Mounted on\n\
                  /dev/sda1        102400000  51200000  40960000      56% /\n";
        assert_eq!(available_kib(df), Some(40_960_000));
        let spaced = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                      map auto_home 0 0 0 100% /System/Volumes/Data/home\n";
        assert_eq!(available_kib(spaced), Some(0));
        assert_eq!(available_kib("Filesystem\n"), None);
        assert_eq!(available_kib(""), None);
    }

    #[test]
    fn a_path_not_made_yet_is_measured_at_its_nearest_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a/b/c");
        assert_eq!(existing(&deep), dir.path());
    }
}
