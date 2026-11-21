//! G835 (a fleet finding, 2026-11-21: a store read of the line took about 1.1 s on a store of
//! 10,331 changes, where one of the release before G830 took 70 ms: to pass over the merge
//! requests already answered, it read the record of every change). Whether a merge request
//! still waits for a runner is read from the record at its change's chain head, and that record
//! never changes, as the head's id names its tree. So two object ids, the head and the request,
//! answer it for good: a read of the line keeps the pairs it found answered, and a later read
//! passes over a kept pair without reading its record.
//!
//! The pairs are one file in bay's folder of the store, written whole through
//! [`crate::durable::rename`]. It holds only the pairs of the requests listed when it was written,
//! so it grows only as the requests do. A cache, never a record: a lost or damaged file costs one
//! read of the records, and a line that is not two object ids of one length is no pair.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::compact::is_object_id;

/// The file of the answered pairs, in bay's folder of the store.
const FILE: &str = "answered-requests";

/// A chain head and a merge request that the record at that head answered.
pub type Pair = (String, String);

fn file(common: &Path) -> PathBuf {
    common.join("bay").join(FILE)
}

/// The pairs kept in the store at `common`; none when it keeps none or cannot be read.
#[must_use]
pub fn kept(common: &Path) -> BTreeSet<Pair> {
    std::fs::read_to_string(file(common))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (head, request) = line.split_once(' ')?;
            (is_object_id(head) && is_object_id(request) && head.len() == request.len())
                .then(|| (head.to_owned(), request.to_owned()))
        })
        .collect()
}

/// Keeps `pairs` in the store at `common` in place of the pairs kept before. A write that fails
/// leaves the file as it was.
pub fn keep(common: &Path, pairs: &BTreeSet<Pair>) {
    let to = file(common);
    let from = to.with_file_name(format!("{FILE}.{}", std::process::id()));
    let mut text = String::new();
    for (head, request) in pairs {
        text.push_str(head);
        text.push(' ');
        text.push_str(request);
        text.push('\n');
    }
    let written = std::fs::create_dir_all(common.join("bay")).is_ok()
        && std::fs::write(&from, text).is_ok()
        && crate::durable::rename(&from, &to).is_ok();
    if !written {
        let _ = std::fs::remove_file(&from);
    }
}
