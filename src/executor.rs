//! `bay runner run`: the supervised local executor (`DESIGN` § 9a,
//! `INTERFACES` § 3a).
//!
//! It fetches the change chains from the remote and drives every queued operation to a stated
//! outcome: published, or blocked with a precise repair. The agent that submitted need not be
//! there. With `--once` it makes one pass and exits (for a CI schedule or a test); otherwise
//! it keeps polling.
//!
//! The contract's actions map onto bay: `submit` is the request in the change chain (and, with
//! `[runner] command`, a call to that adapter); `inspect` is `bay status`; `resume` is running
//! the operation again from the chain, which never repeats a persisted candidate; `cancel` is a
//! newer revision (kickstart item 19b); `complete` is the evidence notes. The ownership claim
//! with epochs is item 16; leases and takeover are item 19a.
//!
//! For as long as it runs, the executor names itself in a box-local file: `executor.lock.d/<pid>`
//! in bay's state directory, holding its pid and its version, one per executor, which only its
//! owner removes, so two executors on one box never free each other's. `bay update` never waits
//! for it (owner ruling 2026-10-20): it keeps every version a live pid's file names, and the
//! executor starts every child `bay` from its own version's kept file
//! (`crate::update::pin_own_binary`), so an operation that began on one version ends on it.
//! Between operations, an executor whose binary changed since it started moves to the new one:
//! on Unix its result names the move (`detail.reexec`) and the process execs the new binary
//! with the same arguments; elsewhere it says so and keeps running until it is restarted. A new
//! binary that does not take those arguments (a bay without this command) is never moved to
//! (G337): the executor says so once and keeps running.
//!
//! Beside that box-local file, each pass publishes the executor's presence to the remote
//! (`crate::presence`, G67), so a box that runs no executor, or one that stopped, can be seen
//! from every other box; `--once` names the write in `detail.presence`.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::change::{Chain, ChangeRecord, EarlyConflict, HeldChain, OpState};
use crate::publish;
use crate::queue::Queue;
use crate::receipts;
use crate::repo::Repo;
use crate::result::{BayResult, Decision, Reason};
use crate::signing::Keys;

#[must_use]
pub fn run(dir: &Path, once: bool, interval: Duration) -> BayResult {
    let git = crate::git::Git::new(dir).version().ok();
    if let Some(refusal) = git.as_deref().and_then(below_floor) {
        return refusal;
    }
    // G67: the presence this executor publishes each pass, at most once per heartbeat.
    let mut beat = crate::presence::Beat::new(
        git.map(|line| crate::version::Version::parse(&line).to_string()),
        interval,
    );
    // Named before it pins its binary, so an update between the two keeps this version's file.
    let _presence = Presence::hold();
    crate::update::pin_own_binary();
    // The supervisor's own sightings of queue heads, on its monotonic clock (§ 6).
    let mut heads: HashMap<u64, Instant> = HashMap::new();
    let mut binary = Binary::now();
    // A looping executor moves to a new binary between operations; `--once` finishes its pass.
    let moves = !once && MOVES;
    loop {
        // Each pass reads the config at the trunk tip it fetched (ruling: config location),
        // so a config that landed since the last pass governs this one.
        let repo = match Repo::open_fetched(dir) {
            Ok(repo) => repo,
            Err(error) => return error.into_result(),
        };
        // A release that could not be pushed never stops the supervisor, which every queued
        // operation depends on: it is reported and tried again on the next pass (§ 6, § 9a).
        let mut seen = Seen::default();
        let unreleased = deadlines(&repo, &mut heads, &mut seen).err();
        let mut notes = Vec::new();
        let mut advanced = Vec::new();
        let ran = match pass(
            &repo,
            moves.then_some(&binary),
            &mut notes,
            &mut advanced,
            seen,
            &mut beat,
        ) {
            Ok(ran) => ran,
            Err(message) => return BayResult::failed(message),
        };
        // Between passes, never inside an operation: the re-exec point.
        if !once && binary.changed() {
            match binary
                .version()
                .filter(|_| moves && binary.takes_arguments())
            {
                // A pin is one version (owner ruling 2026-10-24, BACKLOG 67): a runner moves only
                // to the binary its pin names, and never when the pin cannot be read.
                Some(next) => match pinned_elsewhere(&next) {
                    None => {
                        return BayResult::ok(json!({"ran": ran, "reexec": {
                            "from": crate::version::BAY,
                            "to": next,
                            "path": binary.path,
                        }}));
                    }
                    Some(pin) => eprintln!("bay runner: {}", binary.pinned(&next, &pin)),
                },
                None => eprintln!("bay runner: {}", binary.stays()),
            }
            binary.accept();
        }
        if once {
            let mut detail = json!({"ran": ran});
            if !notes.is_empty() {
                detail["early_conflicts"] = json!(notes);
            }
            if !advanced.is_empty() {
                detail["advanced"] = json!(advanced);
            }
            if let Some(message) = &unreleased {
                detail["unreleased"] = json!(message);
            }
            if let Some(presence) = beat.outcome.take() {
                detail["presence"] = presence;
            }
            return BayResult::ok(detail);
        }
        if let Some(message) = &unreleased {
            eprintln!("bay runner: {message}");
        }
        for row in advanced.iter().filter(|row| row.get("error").is_some()) {
            eprintln!("bay runner: {row}");
        }
        // A looping supervisor never reaches the end of `main`: a pass that fetched trunk
        // leaves the coordination cache now (`crate::coord`).
        crate::coord::after_command();
        std::thread::sleep(interval);
    }
}

/// The executor's Git floor (`GIT-FEATURES` § 1, G68): below it the executor refuses with
/// `TOOL_BELOW_FLOOR` before it fetches or claims anything. `line` is what `git --version`
/// printed; a Git that cannot report its version is left to the first Git call, which fails
/// with its own message.
fn below_floor(line: &str) -> Option<BayResult> {
    use crate::version::{GIT_FLOOR_EXECUTOR, Version};
    let version = Version::parse(line);
    (version < Version::parse(GIT_FLOOR_EXECUTOR)).then(|| {
        BayResult::blocked(crate::result::Reason::ToolBelowFloor).with_detail(json!({
            "tool": "git",
            "role": "runner",
            "version": version.to_string(),
            "floor": GIT_FLOOR_EXECUTOR,
            "recovery": "upgrade git",
        }))
    })
}

/// The pin that keeps this runner off bay `next`: the box's pin (`BAY_UPDATE_VERSION`, else
/// `bay.updateVersion`) when it names another version, or why the pin cannot be read. `None`
/// when no pin is set, or the pin is `next`.
fn pinned_elsewhere(next: &str) -> Option<String> {
    match crate::update::BoxSettings::load() {
        Ok(settings) => settings
            .version
            .filter(|pin| pin != next)
            .map(|pin| format!("this box is pinned to bay {pin}")),
        Err(why) => Some(format!("the pin cannot be read ({why})")),
    }
}

/// Whether this platform can replace the running executor in place (`exec`).
const MOVES: bool = cfg!(unix);

/// Execs the binary `result` moves to (`detail.reexec.path`, from [`run`]) with this process's
/// arguments, once the result is written: the executor's next pass runs on the new version,
/// under the same pid and supervisor. Returns only when it cannot; this process then exits
/// cleanly, and its supervisor starts it again.
pub fn reexec(result: &BayResult) {
    let Some(path) = result
        .detail
        .as_ref()
        .and_then(|detail| detail["reexec"]["path"].as_str())
    else {
        return;
    };
    let _ = std::io::Write::flush(&mut std::io::stdout());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let error = Command::new(path).args(std::env::args_os().skip(1)).exec();
        eprintln!("bay runner: cannot start {path}: {error}");
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// The running binary as it was at start: its path and what identifies the file there.
struct Binary {
    path: Option<PathBuf>,
    stamp: Option<(u64, Option<std::time::SystemTime>, u64)>,
}

impl Binary {
    fn now() -> Self {
        let path = std::env::current_exe().ok();
        let stamp = path.as_deref().and_then(stamp);
        Self { path, stamp }
    }

    /// The file at the path is another one now (inode, or modification time and size).
    fn changed(&self) -> bool {
        self.path
            .as_deref()
            .is_some_and(|path| stamp(path) != self.stamp)
    }

    /// The version the file at the path answers now (`bay --version`); `None` when it does not
    /// answer as bay, so it is never started in this executor's place.
    fn version(&self) -> Option<String> {
        let mut probe = Command::new(self.path.as_deref()?);
        probe.arg("--version");
        let output = crate::git::run_bounded(probe, "bay --version").ok()?;
        let printed = String::from_utf8_lossy(&output.stdout);
        let mut words = printed.split_whitespace();
        if !output.status.success() || words.next() != Some("bay") {
            return None;
        }
        words.next().map(str::to_owned)
    }

    /// Whether the file at the path takes this executor's own arguments (G337): they and
    /// `--help` exit 0. A bay that lacks this command would only exit, and its supervisor start
    /// it again and again, so it is never started in this executor's place.
    fn takes_arguments(&self) -> bool {
        let Some(path) = self.path.as_deref() else {
            return false;
        };
        let mut probe = Command::new(path);
        probe
            .args(std::env::args_os().skip(1))
            .arg("--help")
            .stdin(std::process::Stdio::null());
        crate::git::run_bounded(probe, "bay --help").is_ok_and(|output| output.status.success())
    }

    /// Takes the file now at the path as this executor's own, so one change is told once.
    fn accept(&mut self) {
        self.stamp = self.path.as_deref().and_then(stamp);
    }

    /// Why this runner keeps running its own version though its binary is now bay `next`:
    /// `pin` says which pin keeps it.
    fn pinned(&self, next: &str, pin: &str) -> String {
        let path = self
            .path
            .as_deref()
            .map_or_else(String::new, |path| path.display().to_string());
        format!(
            "{path} is bay {next}, but {pin}; this runner keeps running bay {} until it is restarted",
            crate::version::BAY
        )
    }

    /// Why this executor keeps running its own version though its binary changed.
    fn stays(&self) -> String {
        let path = self
            .path
            .as_deref()
            .map_or_else(String::new, |path| path.display().to_string());
        let why = if MOVES {
            "does not answer as bay, or does not take this runner's command"
        } else {
            "changed, and this platform cannot start it in this process's place"
        };
        format!(
            "{path} {why}; this runner keeps running bay {} until it is restarted",
            crate::version::BAY
        )
    }
}

fn stamp(path: &Path) -> Option<(u64, Option<std::time::SystemTime>, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let inode = 0;
    Some((inode, meta.modified().ok(), meta.len()))
}

/// The box-local file a running executor names itself in: `executor.lock.d/<pid>`, holding its
/// pid and its version, written by a temporary file and a rename, removed on drop by its owner
/// only. An executor that execs a new version keeps its pid, and the new version writes it again.
struct Presence(PathBuf);

impl Presence {
    fn hold() -> Option<Self> {
        let dir = crate::update::state_dir()?.join(LOCKS);
        std::fs::create_dir_all(&dir).ok()?;
        let pid = std::process::id();
        let tmp = dir.join(format!(".{pid}.tmp"));
        std::fs::write(&tmp, format!("{pid}\n{}\n", crate::version::BAY)).ok()?;
        let path = dir.join(pid.to_string());
        crate::durable::rename(&tmp, &path).ok()?;
        Some(Self(path))
    }
}

impl Drop for Presence {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

const LOCKS: &str = "executor.lock.d";

/// The versions the live executors on this box run, from their files: an update never prunes
/// one. A file whose pid is gone is stale and ignored; a pid this process may not signal exists,
/// so it counts as live. A file that names no version (an older bay's) keeps nothing.
#[must_use]
pub fn live_versions() -> Vec<String> {
    let Some(dir) = crate::update::state_dir().map(|dir| dir.join(LOCKS)) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            let text = std::fs::read_to_string(entry.path()).ok()?;
            let version = text.lines().nth(1)?.trim().to_owned();
            (!version.is_empty() && alive(pid)).then_some(version)
        })
        .collect()
}

/// `kill -0`: success, or a refusal to signal (EPERM), means the process exists. On Windows,
/// `tasklist` lists the process by its id (its CSV row holds `"<pid>"`).
fn alive(pid: u32) -> bool {
    if cfg!(windows) {
        let mut probe = Command::new("tasklist");
        probe.args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]);
        return crate::git::run_bounded(probe, "tasklist").is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
        });
    }
    let mut probe = Command::new("kill");
    probe.args(["-0", &pid.to_string()]).env("LC_ALL", "C");
    crate::git::run_bounded(probe, "kill -0").is_ok_and(|output| {
        output.status.success()
            || String::from_utf8_lossy(&output.stderr)
                .to_ascii_lowercase()
                .contains("not permitted")
    })
}

/// The queue as one pass's [`deadlines`] read it, so the pass reads it once (G200b):
/// `before` as it found it, `after` as it left it (`None` once a release made it stale).
#[derive(Default)]
struct Seen {
    before: Option<Queue>,
    after: Option<Queue>,
}

/// Releases a queue head that made no progress within its deadline, measured from this
/// supervisor's first sighting of it as head; a late observer or a restart only delays it.
/// An error is a release the remote refused or did not take; the next pass tries it again.
/// What it read of the queue goes to `seen`.
fn deadlines(
    repo: &Repo,
    heads: &mut HashMap<u64, Instant>,
    seen: &mut Seen,
) -> Result<(), String> {
    let mut queue = Queue::read(repo)?;
    seen.before = Some(queue.clone());
    // Heads whose operation already reached an outcome release at once, one after another.
    while let Some(head) = queue.head()
        && let Some(reason) = finished_head(repo, head)
    {
        let signer = Keys::load(&repo.git).executor;
        crate::queue::release(repo, signer.as_ref(), head.seq, &head.op_id, &reason)?;
        queue = Queue::read(repo)?;
    }
    heads.retain(|seq, _| queue.head().is_some_and(|head| head.seq == *seq));
    let Some(head) = queue.head().cloned() else {
        seen.after = Some(queue);
        return Ok(());
    };
    let first = *heads.entry(head.seq).or_insert_with(Instant::now);
    if first.elapsed() < repo.config.progress_deadline() {
        seen.after = Some(queue);
        return Ok(());
    }
    let signer = Keys::load(&repo.git).executor;
    crate::queue::release(repo, signer.as_ref(), head.seq, &head.op_id, "deadline")?;
    heads.remove(&head.seq);
    Ok(())
}

/// The head always releases (§ 6): a head whose operation already reached an outcome
/// (published, blocked, failed or superseded) is never run again, so it would hold the queue
/// for good. Such a head is a second reservation of an operation that two executors queued at
/// once, or one whose release a crash lost. A change that the retention cut closed keeps no
/// operations, and an expired op id is never run again (§ 5, § 9a), so its place is finished
/// too and is released with the change's state. Returns the release reason, the operation's
/// outcome, or `None` while the operation can still run (or its chain cannot be read).
fn finished_head(repo: &Repo, head: &crate::queue::Entry) -> Option<String> {
    // The remote's record, never moving this store's chain back (G289).
    let record = repo.remote_record(&head.change).ok()?.record;
    let Some(op) = record.operations.iter().find(|op| op.op_id == head.op_id) else {
        return if record.retention_cut {
            word(record.state)
        } else {
            None
        };
    };
    if !op.state.is_terminal() && op.state != OpState::Blocked {
        return None;
    }
    Some(op.outcome.as_ref().map_or_else(
        || word(op.state).unwrap_or_default(),
        |outcome| outcome.to_lowercase(),
    ))
}

/// A state's snake-case word, as the change chain spells it.
fn word(state: impl serde::Serialize) -> Option<String> {
    serde_json::to_value(state)
        .ok()
        .and_then(|state| state.as_str().map(str::to_owned))
}

/// One pass: every operation that is neither terminal nor blocked runs to an outcome, and
/// one blocked `APPROVAL_PENDING` is asked again (G284, [`publish::awaits_approval`]). With a
/// `binary` to watch, it stops between operations once that binary changed. Before the
/// first operation, and after each landing, the entries behind the queue's head are probed
/// for a conflict with trunk (G177); what each probe noted goes to `notes`.
///
/// G200b: a pass costs what its live work costs, not what its history holds. It reads every
/// chain in one batch and the queue as [`deadlines`] left it in `seen`; a closed chain costs a
/// fetch only once its retention cut may be due ([`publish::retention_due`]).
///
/// G67: before its first operation the pass publishes this executor's presence through
/// `beat`, naming the operation it drives first and the live queue depth, at a fixed cost.
///
/// N-7: after the queue's work, the targets with `advance = true` are fast-forwarded to their
/// newest promotable commit (`crate::target::advance`); each move goes to `advanced`.
fn pass(
    repo: &Repo,
    binary: Option<&Binary>,
    notes: &mut Vec<Value>,
    advanced: &mut Vec<Value>,
    seen: Seen,
    beat: &mut crate::presence::Beat,
) -> Result<Vec<Value>, String> {
    let _ = receipts::sync(repo);
    // G289, G294: never forced; a chain ahead of the remote keeps its work, one apart from it
    // is set aside.
    repo.fetch_chains().map_err(|error| error.message())?;
    let chain = Chain::new(&repo.git);
    let mut chains = chain.read_all().map_err(|error| error.message())?;
    // Queued changes go first, in queue order, so one pass carries the queue as far as it can:
    // when the head lands, the next entry is head by the time it is reached.
    let queue = match seen.after {
        Some(queue) => queue,
        None => Queue::read(repo)?,
    };
    let place = |id: &str| queue.of_change(id).map_or(u64::MAX, |entry| entry.seq);
    chains.sort_by_key(|held| place(&held.id));
    let records: Vec<ChangeRecord> = chains.iter().map(|held| held.record.clone()).collect();
    let holding = records
        .iter()
        .flat_map(|record| &record.operations)
        .find(|op| !op.state.is_terminal() && op.state != OpState::Blocked)
        .map(|op| op.op_id.as_str());
    let depth = queue
        .entries
        .keys()
        .filter(|seq| !queue.released.contains(seq))
        .count();
    beat.publish(repo, holding, depth);
    let mut ran = Vec::new();
    // An event a past run lost between the chain and the log is written now (G9), and a log
    // record whose push failed is pushed again.
    let mut recovered = match publish::recover_events(repo, &records) {
        Ok(0) => None,
        Ok(written) => Some(json!({"events_recovered": written})),
        Err(error) => Some(json!({"events_recovered": null, "error": error})),
    };
    crate::log::push(repo, &Keys::load(&repo.git).executor_name);
    notes.extend(early_conflicts(repo, &queue));
    let due = publish::retention_due(repo, &chains);
    for record in &records {
        let id = record.id.as_str();
        let queued: Vec<String> = record
            .operations
            .iter()
            .filter(|op| runs(op))
            .map(|op| op.op_id.clone())
            .collect();
        if queued.is_empty() {
            if due.contains(id)
                && let Ok(Some(head)) = publish::retain(repo, id)
            {
                ran.push(json!({"change": id, "retention_cut": head}));
            }
            continue;
        }
        for op_id in queued {
            if binary.is_some_and(Binary::changed) {
                ran.extend(recovered.take());
                return Ok(ran);
            }
            let step = Cell::new("read");
            let result =
                publish::execute(repo, id, &op_id, &step).unwrap_or_else(BayResult::failed);
            let result = receipted(repo, id, step.get(), result).with_op(op_id.clone());
            // Trunk moved: what waits behind the new head hears of a conflict now (G177).
            if result.decision == Decision::Landed
                && let Ok(queue) = Queue::read(repo)
            {
                notes.extend(early_conflicts(repo, &queue));
            }
            ran.push(json!({
                "change": id,
                "op_id": op_id,
                "decision": result.decision,
                "reason": result.reason,
                "detail": result.detail,
            }));
        }
    }
    ran.extend(
        seen.before
            .and_then(|before| queue_retention(repo, &before, &chains)),
    );
    ran.extend(compaction(repo, &chain));
    ran.extend(recovered.take());
    // G244d: after the queue's work, never ahead of it, the culprit of a red post check.
    if let Some(bisect) = crate::bisect::run(repo) {
        ran.push(json!({"bisect": bisect}));
    }
    advanced.extend(advance(repo));
    Ok(ran)
}

/// Whether a pass runs `op`: it is neither terminal nor blocked, or, G284, it waits for a
/// person's approval ([`publish::awaits_approval`]), which the pass asks again, so the approval
/// lands it and nobody lands it a second time.
fn runs(op: &crate::change::Operation) -> bool {
    !op.state.is_terminal() && (op.state != OpState::Blocked || publish::awaits_approval(op))
}

/// `result`, the outcome a pass reached at `step` for change `id`, with its receipt
/// ([`publish::with_receipt`]). G284: a wait for approval that nothing changed was told when it
/// began, so a pass that finds it unchanged writes no receipt of its own.
fn receipted(repo: &Repo, id: &str, step: &str, result: BayResult) -> BayResult {
    let unchanged = result.reason == Some(Reason::ApprovalPending)
        && result
            .detail
            .as_ref()
            .is_some_and(|detail| detail["unchanged"] == true);
    if unchanged {
        result.with_change(id)
    } else {
        publish::with_receipt(repo, id, step, result)
    }
}

/// N-7: the pass's moves of the targets with `advance = true`, read against the trunk, the
/// registry and every producer's evidence as the remote holds them now. A fault is reported as
/// one row, never fatal to the pass: the queue does not wait on a promotion.
fn advance(repo: &Repo) -> Vec<Value> {
    if !repo.config.targets.iter().any(|target| target.advance) {
        return Vec::new();
    }
    repo.fetch_registry();
    repo.fetch_evidence(&Keys::load(&repo.git));
    let moved = crate::registry::Registry::current(&repo.git).and_then(|registry| {
        let trunk = repo.fetch_trunk().map_err(|error| error.message())?;
        crate::target::advance(repo, &trunk, registry.as_ref(), crate::live::unix_now())
    });
    moved.unwrap_or_else(|why| vec![json!({"error": why})])
}

/// The pass's queue retention (§ 10a): the (reservation, release) pairs below the head of the
/// queue as this pass found it (`before`, read ahead of any release the pass made, so a place
/// released in this pass stays for one pass more) whose operation the pass's chains show
/// finished, released at least the retention window ago. `None` when nothing was removed; a
/// failure is reported and the next pass tries again.
fn queue_retention(repo: &Repo, before: &Queue, chains: &[HeldChain]) -> Option<Value> {
    let records: HashMap<&str, &ChangeRecord> = chains
        .iter()
        .map(|held| (held.id.as_str(), &held.record))
        .collect();
    let finished = |entry: &crate::queue::Entry| {
        records
            .get(entry.change.as_str())
            .is_some_and(|record| record.finished(&entry.op_id))
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let cutoff = now.saturating_sub(repo.config.retention_window().as_secs());
    let seqs = before.prunable(finished, cutoff);
    if seqs.is_empty() {
        return None;
    }
    match crate::queue::prune(repo, before, &seqs) {
        Ok(pruned) if pruned.is_empty() => None,
        Ok(pruned) => Some(json!({"queue_pruned": pruned})),
        Err(error) => Some(json!({"queue_pruned": null, "error": error})),
    }
}

/// The pass's marker compaction (§ 10a ref-growth budget, `crate::compact`), over the chains as
/// this pass left them, so a change it just cut is compacted in the same pass. `None` when
/// nothing moved; a failure is reported and the next pass tries again.
fn compaction(repo: &Repo, chain: &Chain) -> Option<Value> {
    let records: Vec<ChangeRecord> = match chain.read_all() {
        Ok(chains) => chains.into_iter().map(|held| held.record).collect(),
        Err(error) => return Some(json!({"compacted": null, "error": error.message()})),
    };
    let signer = Keys::load(&repo.git).executor;
    match crate::compact::run(repo, signer.as_ref(), &records) {
        Ok(moved) if moved.is_empty() => None,
        Ok(moved) => Some(json!({"compacted": moved})),
        Err(error) => Some(json!({"compacted": null, "error": error})),
    }
}

/// How many queue entries behind the head one probe tests (G177). The head merges for real
/// when it runs; a longer queue is reached as its head moves on, since every landing probes
/// again. A small constant bounds the probe's cost per pass, and the design names none.
const EARLY_PROBES: usize = 8;

/// G177 (`DESIGN` § 6 step 2): each live queue entry behind the head, up to [`EARLY_PROBES`],
/// is test-merged onto the trunk tip this executor fetched with `git merge-tree --write-tree
/// --quiet`, a clean or conflict answer that writes no objects (`GIT-FEATURES` § 2, Git 2.50,
/// the executor floor). Only an entry that conflicts is merged again by the publication merge,
/// for its paths, and its operation records them in its chain, where `bay status` shows
/// `MERGE_CONFLICT` early. The entry keeps its place: its own merge, once it is head, still
/// decides. A later probe that finds it clean clears the note. Every probe is best effort: a
/// git fault skips its entry, and a note the remote did not take is written on a later pass.
/// Returns what was noted, one object per chain written.
fn early_conflicts(repo: &Repo, queue: &Queue) -> Vec<Value> {
    let tracking = format!("{}^{{commit}}", repo.trunk_tracking_ref());
    let Ok(trunk) = repo.git.run(&["rev-parse", "--verify", "-q", &tracking]) else {
        return Vec::new();
    };
    let chain = Chain::new(&repo.git);
    let mut notes = Vec::new();
    let behind = queue
        .entries
        .values()
        .filter(|entry| !queue.released.contains(&entry.seq))
        .skip(1)
        .take(EARLY_PROBES);
    for entry in behind {
        let Ok((_, record)) = chain.read(&entry.change) else {
            continue;
        };
        let Some(op) = record
            .operations
            .iter()
            .find(|op| op.op_id == entry.op_id)
            .filter(|op| !op.state.is_terminal() && op.state != OpState::Blocked)
        else {
            continue;
        };
        let Ok(found) = probe(repo, &trunk, &op.revision) else {
            continue;
        };
        let standing = op.early_conflict.as_ref().map(|standing| &standing.paths);
        if standing == found.as_ref().map(|found| &found.paths) {
            continue;
        }
        let paths = found.as_ref().map(|found| found.paths.clone());
        match publish::note_early_conflict(repo, &entry.change, &entry.op_id, found) {
            Ok(true) => notes.push(json!({
                "change": entry.change,
                "op_id": entry.op_id,
                "trunk": trunk,
                "conflicts": paths,
            })),
            Ok(false) => {}
            Err(error) => notes.push(json!({
                "change": entry.change,
                "op_id": entry.op_id,
                "error": error,
            })),
        }
    }
    notes
}

/// The conflict of `revision` with `trunk`, `None` when it merges cleanly; an error is a git
/// fault. The quiet probe answers clean or conflict alone; only a conflict is merged again for
/// its paths, and that merge decides.
fn probe(repo: &Repo, trunk: &str, revision: &str) -> Result<Option<EarlyConflict>, String> {
    let quiet = ["merge-tree", "--write-tree", "--quiet", trunk, revision];
    let (code, _, stderr) = repo
        .git
        .run_status(&quiet)
        .map_err(|error| error.message())?;
    match code {
        0 => return Ok(None),
        1 => {}
        _ => return Err(stderr),
    }
    match crate::merge::merge_onto(&repo.git, trunk, revision).map_err(|error| error.message())? {
        crate::merge::Merge::Clean(_) => Ok(None),
        crate::merge::Merge::Conflict(paths, _) => Ok(Some(EarlyConflict {
            trunk: trunk.to_owned(),
            paths,
        })),
    }
}
