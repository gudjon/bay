//! G721 (a measurement, 2026-11-14: each `bay status` on a store of 6,300 changes ran about 40
//! ancestry checks and a dozen history queries over the same commits as the read before it, each
//! a Git process). An answer that reads only objects named for good can never change, so it is
//! kept in the store and read back by every later process instead of Git.
//!
//! What is kept: the answer of a query [`crate::git`] judges fixed (a `log`, `rev-list`,
//! `merge-base`, `diff-tree` or `ls-tree` over object ids only, with no option that reads refs,
//! the clock, signature keys, notes or mailmaps), and whether one commit is an ancestor of
//! another. Each is keyed by the Git binary, the config Git reads here and the query itself, so
//! a config change or another Git asks Git again. A store with replace refs or shallow history
//! keeps nothing: there, what a commit's history is can change.
//!
//! G722 (a measurement, 2026-11-15: in one hour the single file of answers grew to 15 MB, and
//! every process read it whole at its first ask): each answer is a file of its own, named by its
//! key, so an ask opens one file however many are kept. It goes in whole through
//! [`crate::durable::rename`] and carries a checksum, so a reader never takes a part of one. A
//! cache, never a record: a lost or damaged answer costs one Git call. An answer not read for
//! [`KEPT_DAYS`] days is removed by a later write, a few directories at a time.
//!
//! G780 (a measurement, 2026-11-18: one store's answers reached 2.5 GB in 65,442 files, 25,771
//! written in a day, and one box held 5.7 GB of them across its clones: an answer over a busy
//! trunk's history is written again at each new tip, and age alone never bounded them). The
//! answers hold at most [`ANSWERS_MAX_BYTES`]: each write keeps its own directory, one of 256,
//! under its share, removing the answers read least lately first. A read marks its answer read
//! (its file's modification time, at most once an hour), since not every system keeps a file's
//! access time.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The directory of answers, under the store's common directory.
const ANSWERS_DIR: &str = "bay/answers";

/// The file of answers an earlier bay kept (0.6.72), removed by the next write.
const OLD_FILE: &str = "bay/facts";

/// The largest answer kept.
pub const FACT_MAX_BYTES: usize = 4 << 20;

/// An answer not read for this many days is removed.
const KEPT_DAYS: u64 = 7;

/// G780: the most bytes of answers a store keeps. A runner re-reads the same answers every pass;
/// at a busy store's pace of new trunk tips, this holds days of them.
pub const ANSWERS_MAX_BYTES: u64 = 256 << 20;

/// The directories answers spread over: the first byte of a key.
const DIRS: u64 = 256;

/// A read marks its answer read again only when the mark is older than this.
const READ_MARK_S: u64 = 3600;

/// A key's bytes.
const KEY_BYTES: usize = 32;
/// An answer's checksum bytes.
const SUM_BYTES: usize = 4;

/// In a test build, `BAY_TEST_FACTS=off` keeps and reads no answer.
fn off() -> bool {
    crate::version::TEST_HOOKS && std::env::var("BAY_TEST_FACTS").is_ok_and(|value| value == "off")
}

/// The key of `query` (its kind, arguments and input) under `context` (the Git binary and the
/// digest of its config).
#[must_use]
pub fn key(context: &str, kind: &str, args: &[&str], input: Option<&[u8]>) -> [u8; KEY_BYTES] {
    let mut hasher = Sha256::new();
    hasher.update(b"bay-facts-1\0");
    hasher.update(context.as_bytes());
    hasher.update(b"\0");
    hasher.update(kind.as_bytes());
    for arg in args {
        hasher.update(b"\0");
        hasher.update(arg.as_bytes());
    }
    hasher.update(b"\0\0");
    hasher.update(input.unwrap_or_default());
    hasher.finalize().into()
}

fn sum(key: &[u8], answer: &[u8]) -> [u8; SUM_BYTES] {
    let mut hasher = Sha256::new();
    hasher.update(key);
    hasher.update(answer);
    let digest = hasher.finalize();
    [digest[0], digest[1], digest[2], digest[3]]
}

/// The file of `key`'s answer: its first byte names its directory, the rest its file.
fn path(common: &Path, key: &[u8; KEY_BYTES]) -> PathBuf {
    let hex = crate::agent_settings::hex(key);
    common.join(ANSWERS_DIR).join(&hex[..2]).join(&hex[2..])
}

/// The answer kept for `key` in the store at `common`.
#[must_use]
pub fn get(common: &Path, key: &[u8; KEY_BYTES]) -> Option<Vec<u8>> {
    if off() {
        return None;
    }
    let file = path(common, key);
    let mut kept = std::fs::read(&file).ok()?;
    let at = kept.len().checked_sub(SUM_BYTES)?;
    let check = kept.split_off(at);
    if sum(key, &kept) != check.as_slice() {
        return None;
    }
    mark_read(&file);
    Some(kept)
}

/// G780: marks the answer at `file` read now, unless it was marked within [`READ_MARK_S`].
fn mark_read(file: &Path) {
    let now = std::time::SystemTime::now();
    let fresh = std::fs::metadata(file)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age.as_secs() < READ_MARK_S);
    if !fresh && let Ok(handle) = std::fs::File::options().write(true).open(file) {
        let _ = handle.set_modified(now);
    }
}

/// The most bytes one directory of answers keeps.
fn dir_max_bytes() -> u64 {
    let total = if crate::version::TEST_HOOKS {
        std::env::var("BAY_TEST_ANSWERS_MAX_BYTES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(ANSWERS_MAX_BYTES)
    } else {
        ANSWERS_MAX_BYTES
    };
    (total / DIRS).max(1)
}

/// Keeps `answer` for `key` in the store at `common`. Best effort: a store that cannot be
/// written keeps nothing.
pub fn put(common: &Path, key: &[u8; KEY_BYTES], answer: &[u8]) {
    if off() || answer.len() > FACT_MAX_BYTES {
        return;
    }
    let file = path(common, key);
    let Some(dir) = file.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let mut text = answer.to_vec();
    text.extend_from_slice(&sum(key, answer));
    let tmp = file.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&tmp, text).is_err() || crate::durable::rename(&tmp, &file).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    // The first byte of the key spreads the pruning over the writes: each write prunes the one
    // directory it wrote in, of what has not been read lately.
    prune(common, dir, &file);
}

/// Removes the answers in `dir` not read for [`KEPT_DAYS`] days, then, while `dir` holds more
/// than its share of [`ANSWERS_MAX_BYTES`], the answers read least lately (G780); and the file
/// of answers an earlier bay kept. `wrote` is never removed: the answer just kept stays.
fn prune(common: &Path, dir: &Path, wrote: &Path) {
    let _ = std::fs::remove_file(common.join(OLD_FILE));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let Some(cutoff) = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(KEPT_DAYS * 86_400))
    else {
        return;
    };
    let mut kept: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let used = meta.modified().ok();
        let path = entry.path();
        if used.is_some_and(|used| used < cutoff) {
            let _ = std::fs::remove_file(&path);
        } else if path != wrote {
            kept.push((used.unwrap_or(std::time::UNIX_EPOCH), meta.len(), path));
        }
    }
    evict(kept, wrote, dir_max_bytes());
}

/// Removes the least lately read of `kept` while they and `wrote` hold more than `max` bytes.
fn evict(mut kept: Vec<(std::time::SystemTime, u64, PathBuf)>, wrote: &Path, max: u64) {
    let wrote_bytes = std::fs::metadata(wrote).map_or(0, |meta| meta.len());
    let mut bytes = wrote_bytes + kept.iter().map(|(_, len, _)| len).sum::<u64>();
    if bytes <= max {
        return;
    }
    kept.sort_by_key(|(used, _, _)| *used);
    for (_, len, path) in kept {
        if bytes <= max {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            bytes = bytes.saturating_sub(len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_kept_is_read_back_and_a_damaged_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path();
        let one = key("git", "log", &["a"], None);
        let two = key("git", "log", &["b"], None);
        put(common, &one, b"first");
        put(common, &two, b"second");
        assert_eq!(get(common, &one).as_deref(), Some(&b"first"[..]));
        assert_eq!(get(common, &two).as_deref(), Some(&b"second"[..]));
        // A damaged answer is not read.
        let file = path(common, &one);
        let mut bytes = std::fs::read(&file).unwrap();
        bytes[0] ^= 0xff;
        std::fs::write(&file, bytes).unwrap();
        assert_eq!(get(common, &one), None);
        // An answer cut short is not read.
        std::fs::write(&file, b"fi").unwrap();
        assert_eq!(get(common, &one), None);
    }

    #[test]
    fn a_write_removes_the_file_of_answers_an_earlier_bay_kept() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path();
        std::fs::create_dir_all(common.join("bay")).unwrap();
        std::fs::write(common.join(OLD_FILE), b"old").unwrap();
        put(common, &key("git", "log", &["a"], None), b"first");
        assert!(!common.join(OLD_FILE).exists());
    }

    /// Keys of `n` answers that share one directory, as a busy store's answers do.
    fn keys_in_one_dir(n: usize) -> Vec<[u8; KEY_BYTES]> {
        let first = key("g780", "q", &["0"], None)[0];
        (0..)
            .map(|i: u32| key("g780", "q", &[&i.to_string()], None))
            .filter(|key| key[0] == first)
            .take(n)
            .collect()
    }

    fn set_used(file: &Path, secs_ago: u64) {
        let at = std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago);
        std::fs::File::options()
            .write(true)
            .open(file)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }

    #[test]
    fn g780_a_directory_over_its_share_loses_the_answers_read_least_lately() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path();
        let keys = keys_in_one_dir(4);
        let answer = vec![b'x'; 1000];
        for (n, key) in keys.iter().take(3).enumerate() {
            put(common, key, &answer);
            // The first was read 3 hours ago, the second 2, the third 1.
            set_used(&path(common, key), 3600 * (3 - n as u64));
        }
        // The oldest is read now, so it is the most lately read of the three.
        assert!(get(common, &keys[0]).is_some());
        let folder = path(common, &keys[0]).parent().unwrap().to_owned();
        let written = path(common, &keys[3]);
        std::fs::write(&written, vec![b'y'; 1004]).unwrap();
        let kept: Vec<_> = std::fs::read_dir(&folder)
            .unwrap()
            .flatten()
            .filter(|entry| entry.path() != written)
            .map(|entry| {
                let meta = entry.metadata().unwrap();
                (meta.modified().unwrap(), meta.len(), entry.path())
            })
            .collect();
        // Room for two answers of 1,004 bytes: two of the four go, the least lately read first.
        evict(kept, &written, 2 * 1004);
        assert!(path(common, &keys[0]).exists(), "the answer just read went");
        assert!(written.exists(), "the answer just written went");
        assert!(
            !path(common, &keys[1]).exists(),
            "the least lately read stayed"
        );
        assert!(!path(common, &keys[2]).exists());
    }

    #[test]
    fn g780_a_read_marks_its_answer_read() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path();
        let one = key("git", "log", &["a"], None);
        put(common, &one, b"first");
        set_used(&path(common, &one), 7200);
        assert!(get(common, &one).is_some());
        let age = std::fs::metadata(path(common, &one))
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap();
        assert!(
            age.as_secs() < 60,
            "a read left its answer marked {age:?} ago"
        );
        // Its content and checksum stand.
        assert_eq!(get(common, &one).as_deref(), Some(&b"first"[..]));
    }
}
