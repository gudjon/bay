//! G589: `bay runner drain`, a runner that stops between operations when it is asked to.
//!
//! A runner's service keeps it running (`Restart=always`, `KeepAlive`), so restarting one is the
//! way a new environment, a new nice level or a new tool path reaches it. A restart by signal ends
//! the runner inside whatever it runs: an operation, a check, a post piece (a fleet finding,
//! 2026-10-30: changing a runner's environment waited for an empty queue, which a busy line never
//! has). `bay runner drain` writes one file in the clone's `.git/bay`: its runner stops at the
//! point it moves to a new binary at, between operations and once its post pieces end, takes the
//! file away and exits 0, and its service starts it again with the service's settings. A runner
//! that cannot take the file away does not stop, so a drain is never a loop of restarts.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::result::BayResult;

use super::post::{self, Post};

/// The file a drain is asked by, in the clone's common directory.
const FILE: &str = "runner-drain";

/// A drain of the runner of one clone.
pub struct Drain {
    path: Option<PathBuf>,
    /// Whether the runner said why it waits, so it says so once.
    told: bool,
}

impl Drain {
    /// The drain of the runner of the clone at `dir`; none when `dir` is no Git clone.
    pub fn of(dir: &Path) -> Self {
        Self {
            path: path(dir),
            told: false,
        }
    }

    /// Whether a drain is asked now.
    pub fn asked(&self) -> bool {
        self.path.as_deref().is_some_and(Path::exists)
    }

    /// The runner's result once it may stop: no post piece runs and the file is taken away.
    /// Until then it starts no piece, says once why it waits and keeps driving operations.
    pub fn point(&mut self, post: &mut Post, ran: &mut Vec<Value>) -> Option<BayResult> {
        let path = self.path.as_deref()?;
        if !post.idle() {
            if !self.told {
                eprintln!(
                    "bay runner: waits for {} post piece(s) to end before it stops, as bay runner drain asked",
                    post.pending()
                );
                self.told = true;
            }
            return None;
        }
        let asked = read(path);
        if let Err(why) = std::fs::remove_file(path).or_else(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(error)
            }
        }) {
            if !self.told {
                eprintln!(
                    "bay runner: does not stop: {} cannot be removed ({why}), and a runner started again would stop again",
                    path.display()
                );
                self.told = true;
            }
            return None;
        }
        ran.extend(post::rows(post.collect()));
        eprintln!(
            "bay runner: stops, as bay runner drain asked; its service starts it again with the service's settings"
        );
        Some(BayResult::ok(
            json!({"ran": std::mem::take(ran), "drained": asked}),
        ))
    }
}

/// `bay runner drain`: asks the runner of the clone at `dir` to stop between operations, or,
/// with `cancel`, takes the ask back.
#[must_use]
pub fn drain(dir: &Path, cancel: bool) -> BayResult {
    let Some(path) = path(dir) else {
        return BayResult::failed(format!("{} is not a Git clone", dir.display()));
    };
    if cancel {
        let was = path.exists();
        if let Err(why) = std::fs::remove_file(&path).or_else(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(error)
            }
        }) {
            return BayResult::failed(format!("cannot remove {}: {why}", path.display()));
        }
        return BayResult::ok(json!({"drain": null, "cancelled": was}));
    }
    let asked = json!({"asked_at": crate::clock::now_rfc3339(), "by_pid": std::process::id()});
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| {
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, format!("{asked}\n"))?;
            crate::durable::rename(&tmp, &path)
        });
    if let Err(why) = written {
        return BayResult::failed(format!("cannot write {}: {why}", path.display()));
    }
    BayResult::ok(json!({"drain": {"path": path, "asked": asked}}))
}

/// The drain file of the clone at `dir`.
fn path(dir: &Path) -> Option<PathBuf> {
    let common = crate::git::Git::new(dir).common_dir().ok()?;
    let common = PathBuf::from(common);
    let common = if common.is_absolute() {
        common
    } else {
        dir.join(common)
    };
    Some(common.join("bay").join(FILE))
}

/// What the drain file says: when it was asked, and by which process.
fn read(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(text.trim()).ok())
        .unwrap_or(Value::Null)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    /// A runner that cannot take the drain file away never stops: its service would start it
    /// again, and it would stop again at once.
    #[test]
    fn a_drain_whose_file_cannot_be_removed_never_stops_the_runner() {
        let dir = tempfile::tempdir().unwrap();
        let bay = dir.path().join("bay");
        std::fs::create_dir(&bay).unwrap();
        let file = bay.join(FILE);
        std::fs::write(&file, "{\"asked_at\":\"2026-10-30T15:00:00Z\"}\n").unwrap();
        std::fs::set_permissions(&bay, std::fs::Permissions::from_mode(0o555)).unwrap();
        // A user who may write a read-only directory (root) cannot see this guard.
        let probe = bay.join("probe");
        if std::fs::write(&probe, "").is_ok() {
            let _ = std::fs::remove_file(&probe);
            std::fs::set_permissions(&bay, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let mut drain = Drain {
            path: Some(file.clone()),
            told: false,
        };
        let mut post = Post::new(false);
        assert!(drain.asked());
        assert!(drain.point(&mut post, &mut Vec::new()).is_none());
        assert!(file.exists());
        std::fs::set_permissions(&bay, std::fs::Permissions::from_mode(0o755)).unwrap();
        let stopped = drain.point(&mut post, &mut Vec::new()).unwrap();
        assert_eq!(
            stopped.detail.unwrap()["drained"]["asked_at"],
            "2026-10-30T15:00:00Z"
        );
        assert!(!file.exists());
    }
}
