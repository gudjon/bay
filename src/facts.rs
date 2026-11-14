//! G721 (a measurement, 2026-11-14: each `bay status` on a store of 6,300 changes ran about 40
//! ancestry checks and a dozen history queries over the same commits as the read before it, each
//! a Git process). An answer that reads only objects named for good can never change, so it is
//! kept in one file of the store and read back by every later process instead of Git.
//!
//! What is kept: the answer of a query [`crate::git`] judges fixed (a `log`, `rev-list`,
//! `merge-base`, `diff-tree` or `ls-tree` over object ids only, with no option that reads refs,
//! the clock, signature keys, notes or mailmaps), and whether one commit is an ancestor of
//! another. Each is keyed by the Git version, the config Git reads here and the query itself, so
//! a config change or a new Git asks Git again. A store with replace refs or shallow history keeps
//! nothing: there, what a commit's history is can change.
//!
//! The file is a cache, never a record: a lost or damaged entry costs one Git call. Each entry is
//! one frame, written whole by one `write` under the file's lock: its length, its key, its
//! answer, and a checksum of both. A reader skips a frame whose checksum fails, and stops at a
//! frame cut short; the next writer cuts a short tail off before it appends. Past
//! [`FACTS_MAX_BYTES`] the file starts again empty.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use sha2::{Digest, Sha256};

/// The file, under the store's common directory.
const FACTS_FILE: &str = "bay/facts";

/// Past this size the file starts again empty.
const FACTS_MAX_BYTES: u64 = 64 << 20;

/// The largest answer kept.
pub const FACT_MAX_BYTES: usize = 4 << 20;

/// A frame's bytes before its key: its length.
const LEN_BYTES: u64 = 4;
/// A key's bytes.
const KEY_BYTES: usize = 32;
/// A frame's checksum bytes.
const SUM_BYTES: usize = 4;

/// What this process has read of one store's file: each key's answer, where the reading
/// stopped, and the file it read (its inode and length), so a file started again is read anew.
#[derive(Default)]
struct Read_ {
    answers: HashMap<[u8; KEY_BYTES], Vec<u8>>,
    read_to: u64,
    file: Option<(u64, u64)>,
}

static READ: std::sync::LazyLock<Mutex<HashMap<PathBuf, Read_>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// In a test build, `BAY_TEST_FACTS=off` keeps and reads no answer.
fn off() -> bool {
    crate::version::TEST_HOOKS && std::env::var("BAY_TEST_FACTS").is_ok_and(|value| value == "off")
}

/// The key of `query` (its kind, arguments and input) under `context` (the Git version and the
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

fn path(common: &Path) -> PathBuf {
    common.join(FACTS_FILE)
}

/// The file's inode and length, or `None` when it is absent.
fn stamp(file: &std::fs::File) -> Option<(u64, u64)> {
    let meta = file.metadata().ok()?;
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let inode = 0;
    Some((inode, meta.len()))
}

/// Reads the frames from `from` on, into `answers`: the offset past the last whole frame.
fn read_frames(
    file: &mut std::fs::File,
    from: u64,
    len: u64,
    answers: &mut HashMap<[u8; KEY_BYTES], Vec<u8>>,
) -> u64 {
    let mut at = from;
    if file.seek(SeekFrom::Start(at)).is_err() {
        return at;
    }
    let mut rest = Vec::new();
    if file.read_to_end(&mut rest).is_err() {
        return at;
    }
    let mut bytes = rest.as_slice();
    loop {
        let Some((head, tail)) = bytes.split_first_chunk::<4>() else {
            return at;
        };
        let body = u64::from(u32::from_le_bytes(*head));
        // A length the rest of the file cannot hold is a frame cut short.
        let Ok(body_len) = usize::try_from(body) else {
            return at;
        };
        if body_len < KEY_BYTES + SUM_BYTES || body_len > tail.len() || at + LEN_BYTES + body > len
        {
            return at;
        }
        let (frame, after) = tail.split_at(body_len);
        let (key, rest) = frame.split_at(KEY_BYTES);
        let (answer, check) = rest.split_at(rest.len() - SUM_BYTES);
        if sum(key, answer) == check
            && let Ok(key) = <[u8; KEY_BYTES]>::try_from(key)
        {
            answers.insert(key, answer.to_vec());
        }
        at += LEN_BYTES + body;
        bytes = after;
    }
}

/// The answer kept for `key` in the store at `common`, reading what other processes appended
/// since this one last read.
#[must_use]
pub fn get(common: &Path, key: &[u8; KEY_BYTES]) -> Option<Vec<u8>> {
    if off() {
        return None;
    }
    let mut read = READ.lock().unwrap_or_else(PoisonError::into_inner);
    let held = read.entry(common.to_path_buf()).or_default();
    if let Some(answer) = held.answers.get(key) {
        return Some(answer.clone());
    }
    let mut file = std::fs::File::open(path(common)).ok()?;
    let (inode, len) = stamp(&file)?;
    if held.file.is_none_or(|(known, _)| known != inode) || len < held.read_to {
        *held = Read_::default();
    }
    if len > held.read_to {
        held.read_to = read_frames(&mut file, held.read_to, len, &mut held.answers);
    }
    held.file = Some((inode, len));
    held.answers.get(key).cloned()
}

/// Keeps `answer` for `key` in the store at `common`. Best effort: a store that cannot be
/// written keeps nothing.
pub fn put(common: &Path, key: &[u8; KEY_BYTES], answer: &[u8]) {
    if off() || answer.len() > FACT_MAX_BYTES {
        return;
    }
    let file_path = path(common);
    if let Some(dir) = file_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(&file_path)
    else {
        return;
    };
    if file.lock().is_err() {
        return;
    }
    let mut read = READ.lock().unwrap_or_else(PoisonError::into_inner);
    let held = read.entry(common.to_path_buf()).or_default();
    let mut written = || -> std::io::Result<()> {
        let (inode, len) = stamp(&file).ok_or_else(|| std::io::Error::other("no metadata"))?;
        if held.file.is_none_or(|(known, _)| known != inode) || len < held.read_to {
            *held = Read_::default();
        }
        if len > FACTS_MAX_BYTES {
            file.set_len(0)?;
            *held = Read_::default();
        } else {
            // What others appended since this process read is read now; a frame cut short by a
            // writer that died is cut off before this one goes after it.
            let good = read_frames(&mut file, held.read_to, len, &mut held.answers);
            if good < len {
                file.set_len(good)?;
            }
            held.read_to = good;
        }
        let body = u32::try_from(KEY_BYTES + answer.len() + SUM_BYTES)
            .map_err(|_| std::io::Error::other("answer too large"))?;
        let mut frame = Vec::with_capacity(4 + KEY_BYTES + answer.len() + SUM_BYTES);
        frame.extend_from_slice(&body.to_le_bytes());
        frame.extend_from_slice(key);
        frame.extend_from_slice(answer);
        frame.extend_from_slice(&sum(key, answer));
        file.write_all(&frame)?;
        held.read_to += frame.len() as u64;
        held.file = stamp(&file);
        Ok(())
    };
    if written().is_ok() {
        held.answers.insert(*key, answer.to_vec());
    }
    let _ = file.unlock();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_kept_is_read_back_and_a_damaged_tail_is_cut_off() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path();
        let one = key("git 2.56", "log", &["a"], None);
        let two = key("git 2.56", "log", &["b"], None);
        put(common, &one, b"first");
        // A writer that died left half a frame.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path(common))
            .unwrap();
        file.write_all(&[200, 0, 0, 0, 1, 2, 3]).unwrap();
        drop(file);
        put(common, &two, b"second");
        READ.lock().unwrap().clear();
        assert_eq!(get(common, &one).as_deref(), Some(&b"first"[..]));
        assert_eq!(get(common, &two).as_deref(), Some(&b"second"[..]));
    }

    #[test]
    fn a_frame_whose_checksum_fails_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path();
        let one = key("git 2.56", "log", &["a"], None);
        put(common, &one, b"first");
        let mut bytes = std::fs::read(path(common)).unwrap();
        let last = bytes.len() - SUM_BYTES - 1;
        bytes[last] ^= 0xff;
        std::fs::write(path(common), bytes).unwrap();
        READ.lock().unwrap().clear();
        assert_eq!(get(common, &one), None);
    }
}
