//! Bay ref names (`DESIGN` § 10a).
//!
//! Every id bay puts in a ref name is generated from a fixed alphabet; the names an operator
//! chooses (producer, executor, machine) use lower-case letters, digits and `-`, at most 32
//! characters, never `/`. So no bay ref is a directory of another (Git refuses a ref that is
//! also a directory of another, on both backends), and `<producer>/<id>` parses at its one `/`.

// Every bay ref namespace is spelled here and nowhere else (G103): other modules build their
// names from these constants.

/// Every bay ref but the evidence notes.
pub const BAY: &str = "refs/bay/";
/// bay's notes refs.
pub const NOTES: &str = "refs/notes/bay/";
/// `refs/notes/bay/evidence/<producer>`: evidence records, one notes ref per producer.
pub const EVIDENCE: &str = "refs/notes/bay/evidence/";
/// `refs/bay/changes/<Change-Id>`: a change chain.
pub const CHANGES: &str = "refs/bay/changes/";
/// `refs/bay/requests/<Change-Id>`: the agent-signed requests of a change.
pub const REQUESTS: &str = "refs/bay/requests/";
/// `refs/bay/landed/<Change-Id>`: a change's landing marker.
pub const LANDED: &str = "refs/bay/landed/";
/// `refs/bay/intent/<hash>/<generation>`: an intent claim.
pub const INTENT: &str = "refs/bay/intent/";
/// `refs/bay/landed-intent/<hash>/<generation>`: a landed intent claim.
pub const LANDED_INTENT: &str = "refs/bay/landed-intent/";
/// `refs/bay/landed-index/<shard>`: compacted landing markers (ruling R11).
pub const LANDED_INDEX: &str = "refs/bay/landed-index/";
/// `refs/bay/queue/<n>`: the publication queue.
pub const QUEUE: &str = "refs/bay/queue/";
/// `refs/bay/queue-done/<shard>`: finished queue places.
pub const QUEUE_DONE: &str = "refs/bay/queue-done/";
/// `refs/bay/live/<machine>`: a machine's live summary.
pub const LIVE: &str = "refs/bay/live/";
/// `refs/bay/log/<name>`: an event log.
pub const LOG: &str = "refs/bay/log/";
/// `refs/bay/executor/<executor>`: an executor's presence (G67, `crate::presence`).
pub const EXECUTOR: &str = "refs/bay/executor/";
/// `refs/bay/talk/<agent>/<id>`: talks.
pub const TALK: &str = "refs/bay/talk/";
/// `refs/bay/locks/<hash>`: the lock of one lockable path (`crate::locks`).
pub const LOCKS: &str = "refs/bay/locks/";
/// `refs/bay/fleet/<machine>`: `bay status`'s copy of the remote's live refs.
pub const FLEET: &str = "refs/bay/fleet/";
/// `refs/bay/fleet-executor/<executor>`: `bay status`'s copy of the remote's executor
/// presences (G67), local only.
pub const FLEET_EXECUTOR: &str = "refs/bay/fleet-executor/";
/// `refs/bay/target/<name>`: `bay status`'s copy of the remote's promotion target refs
/// (NEW-5, `crate::target`), local only.
pub const TARGET: &str = "refs/bay/target/";
/// `refs/bay/fetch/<Change-Id>`: a fetched remote chain, local only.
pub const FETCH: &str = "refs/bay/fetch/";
/// `refs/bay/kept/<Change-Id>/<sha>`: a local chain head the remote's chain moved apart from,
/// set aside before the chain ref moves to the remote's head (G294), local only.
pub const KEPT: &str = "refs/bay/kept/";
/// `refs/bay/fetch-evidence/<writer>`: the remote's copy of this machine's own evidence notes
/// ref, local only, while `bay evidence record` compares the two.
pub const FETCH_EVIDENCE: &str = "refs/bay/fetch-evidence/";
/// `refs/bay/fetch-waiting/<Change-Id>`: `bay waiting`'s and `bay approve`'s copy of the
/// remote's change chains (G280), local only, pruned to the remote's on each read.
pub const FETCH_WAITING: &str = "refs/bay/fetch-waiting/";
/// `refs/bay/census/heads/<branch>`: `bay status --branches`'s copy of the remote's branches,
/// local only, removed when the census ends.
pub const CENSUS: &str = "refs/bay/census/";
/// `refs/bay/archive/heads/<branch>`: the tip of a branch `bay prune` deleted,
/// pushed before the delete so the remote keeps its objects. Create-only on a bay host.
pub const ARCHIVE_HEADS: &str = "refs/bay/archive/heads/";
/// The enrolment registry.
pub const REGISTRY: &str = "refs/bay/registry";
/// The refs of the temporary history repository a check with `inputs = ["history"]` reads
/// (G267, `crate::check`): the base the candidate was merged onto, the candidate, and the
/// change's revision. They exist only in that repository, in the check's scratch, and are never
/// in the store or pushed.
pub const CHECK_BASE: &str = "refs/bay/check/base";
pub const CHECK_CANDIDATE: &str = "refs/bay/check/candidate";
pub const CHECK_REVISION: &str = "refs/bay/check/revision";

/// Validates a producer, executor or machine name.
///
/// # Errors
///
/// The name is empty, longer than 32 characters, or has a character outside `a-z 0-9 -`.
pub fn operator_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 32 {
        return Err(format!("the name {name:?} must have 1 to 32 characters"));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(format!(
            "the name {name:?} may use only lower-case letters, digits and -"
        ));
    }
    Ok(())
}

fn hex32(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn decimal(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) && !value.starts_with('0')
}

/// Validates a full bay ref name: its namespace and every id or name in it. Refs outside
/// `refs/bay/` and `refs/notes/bay/` are not bay's and pass.
///
/// # Errors
///
/// The name is in a bay namespace but is not a bay ref name.
pub fn bay_ref(name: &str) -> Result<(), String> {
    let refuse = |why: &str| Err(format!("{name} is not a bay ref name: {why}"));
    if let Some(producer) = name.strip_prefix("refs/notes/bay/evidence/") {
        return operator_name(producer).map_err(|why| format!("{name}: {why}"));
    }
    if name.starts_with("refs/notes/bay/") {
        return refuse("bay's notes are refs/notes/bay/evidence/<producer>");
    }
    let Some(rest) = name.strip_prefix("refs/bay/") else {
        return Ok(());
    };
    if rest == "registry" {
        return Ok(());
    }
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        ["changes" | "requests" | "landed", id] => {
            if crate::ids::is_change_id(id) {
                Ok(())
            } else {
                refuse("a change id is 8 characters of lower-case base32")
            }
        }
        ["intent" | "landed-intent", hash, generation] => {
            if hex32(hash) && decimal(generation) {
                Ok(())
            } else {
                refuse("an intent claim is <32 hex>/<generation>")
            }
        }
        ["locks", hash] => {
            if hex32(hash) {
                Ok(())
            } else {
                refuse("a lock is named by 32 hex characters")
            }
        }
        ["landed-index", shard] => {
            // Ruling R11: a shard is one byte, in decimal, 0 to 255.
            if *shard == "0" || (decimal(shard) && shard.parse::<u16>().is_ok_and(|n| n < 256)) {
                Ok(())
            } else {
                refuse("a shard is a decimal number from 0 to 255")
            }
        }
        ["queue" | "queue-done", number] => {
            if decimal(number) {
                Ok(())
            } else {
                refuse("a queue place or shard is a decimal number")
            }
        }
        ["talk", who, id] => {
            operator_name(who).map_err(|why| format!("{name}: {why}"))?;
            if crate::talk::is_talk_id(id) {
                Ok(())
            } else {
                refuse("a talk id is 16 characters of lower-case base32")
            }
        }
        // G269: a pruned branch's archive, named by the branch, slashes too.
        ["archive", "heads", branch @ ..] if !branch.is_empty() && !branch.contains(&"") => Ok(()),
        ["live" | "log" | "executor", operator] => {
            operator_name(operator).map_err(|why| format!("{name}: {why}"))
        }
        _ => refuse("unknown namespace"),
    }
}

/// Where an executor holds an operation's in-flight objects (G173). Local to the executor's
/// store and never pushed: [`bay_ref`] does not know the namespace, so the host hook refuses
/// it as an unknown one, and no ref of it is a directory of a pushable bay ref.
pub const INFLIGHT_PREFIX: &str = "refs/bay/inflight/";

/// The in-flight ref of the operation `op_id` (`<change id>.<n>`).
///
/// # Errors
///
/// `op_id` is not a change id, a `.` and an operation number.
pub fn inflight_ref(op_id: &str) -> Result<String, String> {
    let plain = op_id
        .split_once('.')
        .is_some_and(|(change, n)| crate::ids::is_change_id(change) && decimal(n));
    if plain {
        Ok(format!("{INFLIGHT_PREFIX}{op_id}"))
    } else {
        Err(format!(
            "the operation id {op_id:?} is not <change id>.<operation number>"
        ))
    }
}

/// Whether some name in `names` is a directory prefix of another (`refs/x/a` and
/// `refs/x/a/b`): such a set cannot exist at once.
#[must_use]
pub fn prefix_clash<'a>(names: &[&'a str]) -> Option<(&'a str, &'a str)> {
    for a in names {
        for b in names {
            if b.len() > a.len() && b.starts_with(a) && b.as_bytes()[a.len()] == b'/' {
                return Some((a, b));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{bay_ref, operator_name, prefix_clash};

    #[test]
    fn operator_names_are_short_lower_case_and_have_no_slash() {
        assert!(operator_name("ci-linux-2").is_ok());
        assert!(operator_name("ci/linux").is_err());
        assert!(operator_name("CI").is_err());
        assert!(operator_name(&"a".repeat(33)).is_err());
        assert!(operator_name("").is_err());
    }

    #[test]
    fn ids_come_from_the_fixed_alphabet() {
        assert!(bay_ref("refs/bay/changes/abcd2345").is_ok());
        assert!(bay_ref("refs/bay/changes/ABCD2345").is_err());
        assert!(bay_ref("refs/bay/changes/abcd2345/request").is_err());
        assert!(bay_ref("refs/bay/queue/07").is_err());
        assert!(bay_ref("refs/bay/intent/0123456789abcdef0123456789abcdef/1").is_ok());
        assert!(bay_ref("refs/bay/landed-index/0").is_ok());
        assert!(bay_ref("refs/bay/landed-index/255").is_ok());
        assert!(bay_ref("refs/bay/landed-index/256").is_err());
        assert!(bay_ref("refs/bay/landed-index/007").is_err());
        assert!(bay_ref("refs/bay/whatever").is_err());
        assert!(bay_ref("refs/notes/bay/evidence/ci/linux").is_err());
        assert!(bay_ref("refs/heads/main").is_ok());
        assert!(bay_ref("refs/bay/talk/agent-b/abcdefghijklmnop").is_ok());
        assert!(bay_ref("refs/bay/talk/agent-b/abcd2345").is_err());
        assert!(bay_ref("refs/bay/talk/Agent/abcdefghijklmnop").is_err());
        // An in-flight ref is local only: as a pushed name it is an unknown namespace.
        assert!(bay_ref("refs/bay/inflight/abcd2345.1").is_err());
        // G67: an executor's presence is named by its executor; the fleet's copy is local only.
        assert!(bay_ref("refs/bay/executor/ci-linux-2").is_ok());
        assert!(bay_ref("refs/bay/executor/ci/linux").is_err());
        assert!(bay_ref("refs/bay/fleet-executor/ci-linux-2").is_err());
        // G269: a pruned branch's archive is named by the branch, slashes too.
        assert!(bay_ref("refs/bay/archive/heads/ff").is_ok());
        assert!(bay_ref("refs/bay/archive/heads/agent/task-1").is_ok());
        assert!(bay_ref("refs/bay/archive/heads/").is_err());
        assert!(bay_ref("refs/bay/archive/ff").is_err());
        // The census copy is local only.
        assert!(bay_ref("refs/bay/census/heads/ff").is_err());
    }

    #[test]
    fn an_inflight_ref_is_named_by_its_op_id() {
        use super::inflight_ref;
        assert_eq!(
            inflight_ref("abcd2345.1").unwrap(),
            "refs/bay/inflight/abcd2345.1"
        );
        assert!(inflight_ref("abcd2345.01").is_err());
        assert!(inflight_ref("abcd2345").is_err());
        assert!(inflight_ref("abcd2345.1/x").is_err());
        assert!(inflight_ref("ABCD2345.1").is_err());
    }

    #[test]
    fn a_name_that_is_a_prefix_of_another_is_found() {
        assert_eq!(
            prefix_clash(&["refs/bay/changes/abc", "refs/bay/changes/abc/request"]),
            Some(("refs/bay/changes/abc", "refs/bay/changes/abc/request"))
        );
        assert_eq!(
            prefix_clash(&["refs/bay/changes/abc", "refs/bay/changes/abcd"]),
            None
        );
    }
}
