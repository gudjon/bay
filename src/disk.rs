//! The box's disk floor (G265): Git config `bay.minFreeMiB`, global or for one clone. When it
//! is set, `bay start` makes no view and check materialization writes no tree while the free
//! space of the filesystem it would write to is below the floor: both refuse `DISK_LOW`,
//! naming the free space, the floor and the path. A box setting, never a repository one: a
//! repository never declares how full a box's disk may get. Unset, nothing is refused.
//!
//! Free space is what `df -P` reports as available to this user, in 1024-byte blocks. On
//! Windows the floor is not judged.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::git::Git;

/// The box's floor of free space, in mebibytes.
pub const FLOOR_KEY: &str = "bay.minFreeMiB";

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

/// The box's floor as `git` reads it (global, or the clone's own config). Unset is `None`.
///
/// # Errors
///
/// A value that is not a whole number of mebibytes, or a `git config` that fails: a floor that
/// cannot be read is never no floor.
pub fn floor(git: &Git) -> Result<Option<u64>, String> {
    let value = git
        .config_get(FLOOR_KEY)
        .map_err(|error| format!("{FLOOR_KEY} could not be read: {}", error.message()))?;
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    value
        .parse()
        .map(Some)
        .map_err(|_| format!("{FLOOR_KEY} = \"{value}\" is not a whole number of MiB"))
}

/// Whether the filesystem that holds `path` (or, when `path` does not exist yet, its nearest
/// existing ancestor) is below the box's floor. No floor, or Windows, is never low.
///
/// # Errors
///
/// The floor cannot be read, or the free space cannot be measured.
pub fn below_floor(git: &Git, path: &Path) -> Result<Option<Low>, String> {
    let Some(floor_mib) = floor(git)? else {
        return Ok(None);
    };
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
