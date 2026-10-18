//! `bay executor run`: the supervised local executor (`DESIGN` § 9a,
//! `INTERFACES` § 3a).
//!
//! It fetches the change chains from the remote and drives every queued operation to a stated
//! outcome: published, or blocked with a precise repair. The agent that submitted need not be
//! there. With `--once` it makes one pass and exits (for a CI schedule or a test); otherwise
//! it keeps polling.
//!
//! The contract's actions map onto bay: `submit` is the request in the change chain (and, with
//! `[executor] command`, a call to that adapter); `inspect` is `bay status`; `resume` is running
//! the operation again from the chain, which never repeats a persisted candidate; `cancel` is a
//! newer revision (kickstart item 19b); `complete` is the evidence notes. The ownership claim
//! with epochs is item 16; leases and takeover are item 19a.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::change::{CHANGES_PREFIX, Chain, OpState};
use crate::publish;
use crate::queue::Queue;
use crate::receipts;
use crate::repo::Repo;
use crate::result::BayResult;
use crate::signing::Keys;

#[must_use]
pub fn run(dir: &Path, once: bool, interval: Duration) -> BayResult {
    let repo = match Repo::open(dir) {
        Ok(repo) => repo,
        Err(message) => return BayResult::failed(message),
    };
    // The supervisor's own sightings of queue heads, on its monotonic clock (§ 6).
    let mut heads: HashMap<u64, Instant> = HashMap::new();
    loop {
        if let Err(message) = deadlines(&repo, &mut heads) {
            return BayResult::failed(message);
        }
        let ran = match pass(&repo) {
            Ok(ran) => ran,
            Err(message) => return BayResult::failed(message),
        };
        if once {
            return BayResult::ok(json!({"ran": ran}));
        }
        std::thread::sleep(interval);
    }
}

/// Releases a queue head that made no progress within its deadline, measured from this
/// supervisor's first sighting of it as head; a late observer or a restart only delays it.
fn deadlines(repo: &Repo, heads: &mut HashMap<u64, Instant>) -> Result<(), String> {
    let queue = Queue::read(repo)?;
    heads.retain(|seq, _| queue.head().is_some_and(|head| head.seq == *seq));
    let Some(head) = queue.head() else {
        return Ok(());
    };
    let first = *heads.entry(head.seq).or_insert_with(Instant::now);
    if first.elapsed() >= repo.config.progress_deadline() {
        let signer = Keys::load(&repo.git).executor;
        crate::queue::release(repo, signer.as_ref(), head.seq, &head.op_id, "deadline")?;
        heads.remove(&head.seq);
    }
    Ok(())
}

/// One pass: every operation that is neither terminal nor blocked runs to an outcome.
fn pass(repo: &Repo) -> Result<Vec<Value>, String> {
    let _ = receipts::sync(repo);
    let refspec = format!("+{CHANGES_PREFIX}*:{CHANGES_PREFIX}*");
    repo.git
        .run(&["fetch", "-q", "--no-tags", repo.remote(), &refspec])
        .map_err(|error| error.message())?;
    let chain = Chain::new(&repo.git);
    let names = repo
        .git
        .run(&["for-each-ref", "--format=%(refname)", CHANGES_PREFIX])
        .map_err(|error| error.message())?;
    let mut ran = Vec::new();
    for name in names.lines() {
        let id = name.trim_start_matches(CHANGES_PREFIX);
        let Ok((_, record)) = chain.read(id) else {
            continue;
        };
        let queued: Vec<String> = record
            .operations
            .iter()
            .filter(|op| !op.state.is_terminal() && op.state != OpState::Blocked)
            .map(|op| op.op_id.clone())
            .collect();
        if queued.is_empty() {
            if let Ok(Some(head)) = publish::retain(repo, id) {
                ran.push(json!({"change": id, "retention_cut": head}));
            }
            continue;
        }
        for op_id in queued {
            let step = Cell::new("read");
            let result =
                publish::execute(repo, id, &op_id, &step).unwrap_or_else(BayResult::failed);
            let result = publish::with_receipt(repo, id, step.get(), result).with_op(op_id.clone());
            ran.push(json!({
                "change": id,
                "op_id": op_id,
                "decision": result.decision,
                "reason": result.reason,
                "detail": result.detail,
            }));
        }
    }
    Ok(ran)
}
