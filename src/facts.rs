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
    let mut kept = std::fs::read(path(common, key)).ok()?;
    let at = kept.len().checked_sub(SUM_BYTES)?;
    let check = kept.split_off(at);
    (sum(key, &kept) == check.as_slice()).then_some(kept)
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
    prune(common, dir);
}

/// Removes the answers in `dir` not read for [`KEPT_DAYS`] days, and the file of answers an
/// earlier bay kept.
fn prune(common: &Path, dir: &Path) {
    let _ = std::fs::remove_file(common.join(OLD_FILE));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let Some(cutoff) = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(KEPT_DAYS * 86_400))
    else {
        return;
    };
    for entry in entries.flatten() {
        let used = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.accessed().or_else(|_| meta.modified()).ok());
        if used.is_some_and(|used| used < cutoff) {
            let _ = std::fs::remove_file(entry.path());
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
}
