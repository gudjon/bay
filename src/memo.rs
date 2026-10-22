//! Loads that are pure functions of an object id, made once per process.
//!
//! The registry, the policy and the config at a commit never change: the commit's id names its
//! tree, and the tree names every blob. So within one bay process a load at a fixed revision is
//! kept and handed out again, keyed by the repository's git directory and that revision, never
//! by a ref name. A caller resolves a ref each time it reads one, so a moved ref is a new key and
//! a kept load can never be stale. A revision that is not [`fixed`] is loaded each time, and a
//! load its caller says may differ on a retry (a git fault) is never kept.

use std::any::Any;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, PoisonError};

use crate::git::Git;

/// The most loads one process keeps; past it the memo starts over, so a long-lived process (an
/// executor, `bay status --since`) that sees many trunk tips stays bounded.
const CAPACITY: usize = 512;

/// The load's kind, the repository's git directory, and the fixed revision.
type Key = (&'static str, PathBuf, String);
type Table = Mutex<HashMap<Key, Box<dyn Any + Send>>>;

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(Table::default)
}

/// Whether `rev` names one object forever: a full object id (40 or 64 lowercase hex digits),
/// alone or followed only by `^` and `~` parent steps. A ref name, `HEAD`, an abbreviated id or
/// a peel such as `^{tree}` is not fixed.
#[must_use]
pub fn fixed(rev: &str) -> bool {
    let digits = rev
        .bytes()
        .take_while(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        .count();
    (digits == 40 || digits == 64)
        && rev[digits..]
            .bytes()
            .all(|byte| byte == b'^' || byte == b'~' || byte.is_ascii_digit())
}

/// The value `load` makes for `rev` in `git`'s repository, as one `kind` of load. When `rev`
/// is [`fixed`], the first value for which `keep` holds is kept, and every later call with the
/// same kind, git directory and revision gets a copy of it without calling `load`.
#[must_use]
pub fn at<T: Clone + Send + 'static>(
    kind: &'static str,
    git: &Git,
    rev: &str,
    keep: impl FnOnce(&T) -> bool,
    load: impl FnOnce() -> T,
) -> T {
    if !fixed(rev) {
        return load();
    }
    let key = (kind, git.dir().to_path_buf(), rev.to_owned());
    let lock = || table().lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(kept) = lock().get(&key).and_then(|value| value.downcast_ref::<T>()) {
        return kept.clone();
    }
    // The lock is not held while `load` runs git: a slow load never stalls another thread.
    let value = load();
    if keep(&value) {
        let mut table = lock();
        if table.len() >= CAPACITY {
            table.clear();
        }
        table.insert(key, Box::new(value.clone()));
    }
    value
}

#[cfg(test)]
mod tests {
    use super::fixed;

    #[test]
    fn only_a_full_object_id_and_its_parent_steps_are_fixed() {
        let sha1 = "0123456789abcdef0123456789abcdef01234567";
        let sha256 = format!("{sha1}0123456789abcdef01234567");
        assert!(fixed(sha1));
        assert!(fixed(&sha256));
        assert!(fixed(&format!("{sha1}^1")));
        assert!(fixed(&format!("{sha1}~2^")));
        let unfixed = [
            "HEAD".to_owned(),
            "main".to_owned(),
            "refs/bay/registry".to_owned(),
            sha1[..39].to_owned(),
            format!("{sha1}0"),
            format!("{sha1}^{{tree}}"),
            format!("{sha1}:path"),
            sha1.to_uppercase(),
        ];
        for rev in &unfixed {
            assert!(!fixed(rev), "{rev}");
        }
    }
}
