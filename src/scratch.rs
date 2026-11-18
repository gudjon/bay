//! G781 (a measurement, 2026-11-18: a live `bay queue` took 95 s, 87 s of it in four fetches
//! that each spent about 20 s walking the store's 119,575 local refs before asking the remote;
//! 6,942 of them were `bay waiting`'s copies of every change chain, kept between runs). bay's
//! local scratch refs, the copies of a remote ref a command reads and then deletes, are named
//! under the process that wrote them, `<namespace><pid>/<name>`, so a later command can tell
//! one in use from one a crashed or older process left behind. [`prune`] deletes the second
//! kind: a scratch ref of a process that no longer runs, or one in the layout before G781.
//! It costs one listing, one liveness probe per process named, and one transaction.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::git::Git;
use crate::names::{FETCH, FETCH_EVIDENCE, FETCH_WAITING};

/// The namespaces whose refs are scratch: read and deleted by the command that wrote them.
pub const NAMESPACES: [&str; 3] = [FETCH, FETCH_EVIDENCE, FETCH_WAITING];

/// This process's own prefix under `namespace`.
#[must_use]
pub fn own(namespace: &str) -> String {
    format!("{namespace}{}/", std::process::id())
}

/// The process a scratch ref under `namespace` names, when it is in the per-process layout.
fn writer(namespace: &str, name: &str) -> Option<u32> {
    let rest = name.strip_prefix(namespace)?;
    let (pid, tail) = rest.split_once('/')?;
    if tail.is_empty() {
        return None;
    }
    pid.parse().ok()
}

/// Deletes the scratch refs no running process holds: those of a process that has ended, and
/// those in the layout before G781. Best effort: a store that cannot be listed or written is
/// left as it is.
pub fn prune(git: &Git) {
    let mut args = vec!["for-each-ref", "--format=%(objectname) %(refname)"];
    args.extend(NAMESPACES);
    let Ok(listed) = git.run(&args) else {
        return;
    };
    let here = std::process::id();
    let mut alive: BTreeMap<u32, bool> = BTreeMap::new();
    let mut deletes = String::new();
    for line in listed.lines() {
        let Some((target, name)) = line.split_once(' ') else {
            continue;
        };
        let Some(namespace) = NAMESPACES.iter().find(|ns| name.starts_with(**ns)) else {
            continue;
        };
        let stale = match writer(namespace, name) {
            Some(pid) if pid == here => false,
            Some(pid) => !*alive
                .entry(pid)
                .or_insert_with(|| crate::executor::alive(pid)),
            None => true,
        };
        if stale {
            let _ = writeln!(deletes, "delete {name} {target}");
        }
    }
    if !deletes.is_empty() {
        let _ = git.run_with(&["update-ref", "--stdin"], &[], Some(deletes.as_bytes()));
    }
}

/// The store's local refs by namespace (the first three path parts, e.g. `refs/bay/changes`),
/// and their total, from one listing.
#[must_use]
pub fn census(git: &Git) -> Option<(usize, BTreeMap<String, usize>)> {
    let listed = git.run(&["for-each-ref", "--format=%(refname)"]).ok()?;
    let mut by: BTreeMap<String, usize> = BTreeMap::new();
    let mut total = 0;
    for name in listed.lines() {
        total += 1;
        let parts: Vec<&str> = name.splitn(4, '/').collect();
        let keep = if parts.len() > 3 { 3 } else { 2 };
        *by.entry(parts[..keep.min(parts.len())].join("/"))
            .or_default() += 1;
    }
    Some((total, by))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scratch_ref_names_its_writer_only_in_the_per_process_layout() {
        assert_eq!(
            writer(FETCH_WAITING, "refs/bay/fetch-waiting/123/abc"),
            Some(123)
        );
        assert_eq!(writer(FETCH_WAITING, "refs/bay/fetch-waiting/abc"), None);
        assert_eq!(writer(FETCH, "refs/bay/fetch/from-123"), None);
        assert_eq!(writer(FETCH, "refs/bay/fetch/123/"), None);
    }
}
