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
//!
//! #123: after its queue's work, a pass starts the runner's own post work (`crate::bisect`) as
//! pieces, one environment each, each on its own thread ([`post`]): the post work of different
//! environments runs side by side, and one environment runs one piece at a time. A pass never
//! waits for a piece; it collects the verdict of each piece that ended, and the next pass, or
//! the one after, plans the next. A runner whose binary changed starts no piece and WAITS for
//! its running pieces to end before it moves to the new binary: it keeps driving operations
//! meanwhile, and never hands a piece over or stops inside one. `--once` waits for every piece
//! it started.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::change::{Chain, ChangeRecord, EarlyConflict, HeldChain, OpState};
use crate::check;
use crate::publish;
use crate::queue::Queue;
use crate::receipts;
use crate::repo::Repo;
use crate::result::{BayResult, Decision, Reason};
use crate::signing::Keys;

mod post;

use post::Post;

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
    // A clone set up before 0.4.0 names its runner under the old keys, which bay never reads,
    // so the runner ran as `local`: it moves them in its own clone, as `bay install` does.
    match crate::install::carry_over(&crate::git::Git::new(dir), "--local") {
        Ok(moved) => {
            for row in moved {
                eprintln!(
                    "bay runner: moved {} to {} in this clone (renamed in 0.4.0)",
                    row["from"].as_str().unwrap_or_default(),
                    row["to"].as_str().unwrap_or_default()
                );
            }
        }
        Err(why) => eprintln!("bay runner: could not move the renamed keys in this clone: {why}"),
    }
    // Named before it pins its binary, so an update between the two keeps this version's file.
    let _presence = Presence::hold();
    crate::update::pin_own_binary();
    // The supervisor's own sightings of queue heads, on its monotonic clock (§ 6).
    let mut heads: HashMap<u64, Instant> = HashMap::new();
    let mut binary = Binary::now();
    // A looping executor moves to a new binary between operations; `--once` finishes its pass.
    let moves = !once && MOVES;
    // #123: the post pieces still running, across passes; `--once` waits for its own.
    let mut post = Post::new(once);
    // The version a runner waits to move to while its post pieces run, said once.
    let mut waits_for: Option<String> = None;
    loop {
        // Each pass reads the config at the trunk tip it fetched (ruling: config location),
        // so a config that landed since the last pass governs this one.
        let repo = match Repo::open_fetched(dir) {
            Ok(repo) => Arc::new(repo),
            Err(error) => return error.into_result(),
        };
        // A fleet finding (2026-10-26): a runner is the one bay that runs for days, so it asks
        // for a new release itself, at most hourly and in the background, like any command.
        if !once {
            crate::update::auto();
        }
        // A release that could not be pushed never stops the supervisor, which every queued
        // operation depends on: it is reported and tried again on the next pass (§ 6, § 9a).
        let mut seen = Seen::default();
        let unreleased = deadlines(&repo, &mut heads, &mut seen).err();
        // A fleet finding (2026-10-26): a release the remote refused was reported only by
        // `--once`, so a looping runner held a dead head for hours without a word.
        if let Some(why) = unreleased.as_ref().filter(|_| !once) {
            eprintln!("bay runner: a queue place could not be released: {why}");
        }
        let mut notes = Vec::new();
        let mut advanced = Vec::new();
        let mut ran = match pass(
            &repo,
            moves.then_some(&binary),
            (&mut notes, &mut advanced),
            seen,
            &mut beat,
            &mut post,
        ) {
            Ok(ran) => ran,
            Err(message) => return BayResult::failed(message),
        };
        // Between passes, never inside an operation: the re-exec point.
        if !once
            && (binary.changed() || binary.holding())
            && let Some(moved) =
                reexec_point(&mut binary, moves, (&mut post, &mut waits_for), &mut ran)
        {
            return moved;
        }
        if once {
            let mut detail = json!({"ran": ran});
            let (told, notes): (Vec<Value>, Vec<Value>) = notes
                .into_iter()
                .partition(|note| note.get("red_owner").is_some());
            if !notes.is_empty() {
                detail["early_conflicts"] = json!(notes);
            }
            if !told.is_empty() {
                detail["red_owners"] = json!(told);
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
/// `bay.updateVersion`) when it does not allow `next` ([`crate::update::pin_allows`]: the pin
/// itself, or the newest release above it), or why the pin cannot be read. `None` when no pin
/// is set, or the pin allows `next`.
fn pinned_elsewhere(next: &str) -> Option<String> {
    match crate::update::BoxSettings::load() {
        Ok(settings) => settings
            .version
            .filter(|pin| !crate::update::pin_allows(pin, next))
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
    /// The file a pin kept this runner off, asked about again each pass: a pin that changes
    /// later moves the runner then (a fleet finding, 2026-10-26: runners told once stayed on an
    /// old release after their pin moved past it).
    held: Option<(u64, Option<std::time::SystemTime>, u64)>,
}

impl Binary {
    fn now() -> Self {
        let path = std::env::current_exe().ok();
        let stamp = path.as_deref().and_then(stamp);
        Self {
            path,
            stamp,
            held: None,
        }
    }

    /// The file at the path is another one now (inode, or modification time and size), and not
    /// the one a pin keeps this runner off.
    fn changed(&self) -> bool {
        self.path.as_deref().is_some_and(|path| {
            let now = stamp(path);
            now != self.stamp && (self.held.is_none() || now != self.held)
        })
    }

    /// The file at the path is the one a pin kept this runner off.
    fn holding(&self) -> bool {
        self.held.is_some() && self.path.as_deref().and_then(stamp) == self.held
    }

    /// Records the file now at the path as the one its pin keeps this runner off; whether it
    /// is a new one, to be told.
    fn hold(&mut self) -> bool {
        let now = self.path.as_deref().and_then(stamp);
        let new = self.held != now;
        self.held = now;
        new
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
            "{path} is bay {next}, but {pin}; this runner keeps running bay {} until its pin allows bay {next}",
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
    while let Some(head) = queue.head().cloned()
        && let Some(reason) = finished_head(repo, &head)
    {
        let record = repo
            .remote_record(&head.change)
            .ok()
            .map(|remote| remote.record);
        pass_on_or_release(repo, record.as_ref(), &head, &reason)?;
        queue = Queue::read(repo)?;
        // A place passed on to a live operation is a live head now.
        if queue.head().is_some_and(|now| now.seq == head.seq) {
            break;
        }
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
    // BACKLOG 119: a head main's red holds waits in its place for main, not for its own work, so
    // its deadline starts again; red windows of 25 and 35 minutes were measured (2026-10-28).
    if repo
        .remote_record(&head.change)
        .is_ok_and(|remote| waits_on_main(&remote.record, &head.op_id))
    {
        heads.insert(head.seq, Instant::now());
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
    finished(&record, head)
}

/// [`finished_head`] on `record`, the head's change as the remote has it.
fn finished(record: &ChangeRecord, head: &crate::queue::Entry) -> Option<String> {
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

/// BACKLOG 119: whether operation `op_id` of `record` waits in its place on main's red: no
/// runner holds it, and its outcome is `MAIN_RED` (`publish::wait_in_line`).
fn waits_on_main(record: &ChangeRecord, op_id: &str) -> bool {
    record.operations.iter().any(|op| {
        op.op_id == op_id
            && op.owner.is_none()
            && matches!(op.state, OpState::Submitted | OpState::Admitted)
            && op.outcome.as_deref() == Some(Reason::TrunkRed.code())
    })
}

/// The newer live operation of `record` that `op_id` was superseded by, when there is one: its
/// place passes to it, never released (the trust-agents ruling, 2026-10-26: a revision made at
/// the head kept no place and joined behind 120 changes).
fn successor(record: &ChangeRecord, op_id: &str) -> Option<String> {
    let op = record.operations.iter().find(|op| op.op_id == op_id)?;
    if op.state != OpState::Superseded {
        return None;
    }
    record
        .operations
        .iter()
        .rev()
        .find(|op| !op.state.is_terminal() && op.state != OpState::Blocked)
        .map(|op| op.op_id.clone())
}

/// Gives the place of `entry`, whose operation ended, to the operation that superseded it, or
/// else releases it as `reason`.
fn pass_on_or_release(
    repo: &Repo,
    record: Option<&ChangeRecord>,
    entry: &crate::queue::Entry,
    reason: &str,
) -> Result<(), String> {
    let signer = Keys::load(&repo.git).executor;
    match record.and_then(|record| successor(record, &entry.op_id)) {
        // A place released meanwhile is not replaced; the newer operation joins at its turn.
        Some(next) => crate::queue::replace(repo, signer.as_ref(), entry, &next).map(|_| ()),
        None => crate::queue::release(repo, signer.as_ref(), entry.seq, &entry.op_id, reason),
    }
}

/// The journal words for one piece of post work: each post check run, as `post <check> on
/// <tip>: <verdict>`, or the warm or bisect work by its kind.
fn post_line(done: &Value) -> String {
    if let Some(rows) = done.get("post").and_then(Value::as_array) {
        let runs: Vec<String> = rows
            .iter()
            .map(|row| {
                let tip: String = row["tip"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(10)
                    .collect();
                // A fleet finding (2026-10-28): a post check that was not run read "?", and
                // its reason was only in the pass's result, which a looping runner never prints.
                let verdict = row["verdict"].as_str().map_or_else(
                    || {
                        row["verdict"]["not_run"].as_str().map_or_else(
                            || "?".to_owned(),
                            |why| {
                                let why: Vec<&str> = why.lines().map(str::trim).collect();
                                format!("not run: {}", why.join("; "))
                            },
                        )
                    },
                    str::to_owned,
                );
                format!(
                    "post {} on {tip}: {verdict}",
                    row["check"].as_str().unwrap_or("?")
                )
            })
            .collect();
        return runs.join("; ");
    }
    done.as_object()
        .and_then(|object| object.keys().next())
        .map_or_else(|| "post work".to_owned(), |kind| format!("{kind} work"))
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
/// S1: while an operation or the post work runs, the beat's thread places in line each change
/// submitted since the pass read the chains ([`Arrivals`]); the pass's own visits come in the
/// order above, and a change that waits on a red main keeps its place there (BACKLOG 119).
///
/// N-7: after the queue's work, the targets with `advance = true` are fast-forwarded to their
/// newest promotable commit (`crate::target::advance`); each move goes to `advanced`.
///
/// #123: after the queue's work, [`post_pass`] collects the post pieces that ended and starts
/// the next ones in `post`; once `binary` changed it starts none, and the pass stops between
/// operations only when no piece runs.
#[allow(clippy::too_many_lines)]
fn pass(
    shared: &Arc<Repo>,
    binary: Option<&Binary>,
    (notes, advanced): (&mut Vec<Value>, &mut Vec<Value>),
    seen: Seen,
    beat: &mut crate::presence::Beat,
    post: &mut Post,
) -> Result<Vec<Value>, String> {
    let repo: &Repo = shared;
    // G438: this pass's visits share what it reads from the remote.
    repo.share_reads(true);
    // BACKLOG 113 (a review, 2026-10-28): a head's red gate judges main from this box's copy of
    // the post records. Only the post work fetched them, at a pass's end, so a box that runs no
    // post check judged a red main green and landed on it. The pass reads them before any visit.
    repo.fetch_evidence(&Keys::load(&repo.git));
    let _ = receipts::sync(repo);
    sweep_check_scratch(repo);
    // G289, G294: never forced; a chain ahead of the remote keeps its work, one apart from it
    // is set aside.
    // G434 (a fleet finding, 2026-10-28): fourteen submitted changes no pass could read went
    // unnamed for hours, the fetch's error dropped. Each is named in the journal with Git's words.
    for (change, why) in repo.fetch_chains().map_err(|error| error.message())? {
        let why = why.lines().map(str::trim).collect::<Vec<_>>().join("; ");
        eprintln!("bay runner: {change} could not be read from the remote: {why}");
    }
    let chain = Chain::new(&repo.git);
    let mut chains = chain.read_all().map_err(|error| error.message())?;
    let queue = match seen.after {
        Some(queue) => queue,
        None => Queue::read(repo)?,
    };
    in_pass_order(&mut chains, &queue);
    let records: Vec<ChangeRecord> = chains.iter().map(|held| held.record.clone()).collect();
    let mut arrivals = Arrivals::new(&records);
    sweep_ended(repo, &queue, &records);
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
    notes.extend(red_owners(repo));
    let due = publish::retention_due(repo, &chains);
    let reach = within_reach(repo, &queue);
    let began = std::time::Instant::now();
    let bound = pass_bound();
    // The heads this pass took between its operations: the loop, which reads the chains as the
    // pass began, never runs one of them again.
    let mut taken: HashSet<String> = HashSet::new();
    for record in &records {
        let id = record.id.as_str();
        let queued: Vec<String> = record
            .operations
            .iter()
            // A change that names one it fixes, reverts or supersedes is always reached: it may
            // be the repair of a red main, which runs ahead of the line (`queue_gate`).
            // A fleet finding (2026-10-27): an abandoned change's older operation was still
            // queued and run hours later. No operation of an ended change runs.
            .filter(|_| !change_ended(record))
            .filter(|op| runs(op) && (names_a_repair(record) || reach(op, freed(&ran))))
            .filter(|op| !taken.contains(&op.op_id))
            .map(|op| op.op_id.clone())
            .collect();
        if queued.is_empty() {
            continue;
        }
        // A fleet finding (2026-10-27): passes ran one and a half to two hours, and a change
        // submitted after a pass began was seen by no runner until it ended, while post checks,
        // which run after the loop, waited as long. Past its bound a pass starts no operation:
        // the next pass reads every chain again, the unplaced first.
        if !ran.is_empty() && began.elapsed() >= bound {
            break;
        }
        for op_id in queued {
            if binary.is_some_and(Binary::changed) && post.idle() {
                ran.extend(recovered.take());
                ran.append(&mut arrivals.rows);
                return Ok(ran);
            }
            ran.push(run_op(repo, id, &op_id, depth, beat, notes, &mut arrivals));
            forget_if_moved(repo, &ran);
            // F13 (2026-10-25, again 2026-10-26: a runner hung holding the head 19 minutes):
            // between operations, a head whose holder stopped writing is taken now, not at the
            // next pass's start an hour later.
            if let Some((head_id, head_op)) = abandoned_head(repo, &op_id) {
                ran.push(run_op(
                    repo,
                    &head_id,
                    &head_op,
                    depth,
                    beat,
                    notes,
                    &mut arrivals,
                ));
                forget_if_moved(repo, &ran);
                taken.insert(head_op);
            }
        }
    }
    ran.extend(
        seen.before
            .and_then(|before| queue_retention(repo, &before, &chains)),
    );
    ran.extend(compaction(repo, &chain));
    ran.extend(recovered.take());
    let plans = !binary.is_some_and(Binary::changed);
    ran.extend(post_pass(shared, post, plans, (beat, depth, &mut arrivals)));
    // A fleet finding (2026-10-27): a retention cut costs a fetch of its chain, and one that
    // does not happen leaves the chain due again; read inside the loop, hundreds of closed chains
    // kept a runner silent for minutes ahead of every change sorted after them, nothing admitted.
    // Cuts come after the queue's work and its post checks, while the pass is within its bound.
    for record in records
        .iter()
        .filter(|record| due.contains(record.id.as_str()))
    {
        if began.elapsed() >= bound {
            break;
        }
        if let Ok(Some(head)) = publish::retain(repo, &record.id) {
            ran.push(json!({"change": record.id, "retention_cut": head}));
        }
    }
    advanced.extend(advance(repo));
    ran.append(&mut arrivals.rows);
    Ok(ran)
}

/// After the queue's work, never ahead of it: the runner's own post checks on main's tip, then
/// the warm runs, then the culprit of a red post check (G244d), as pieces of one environment
/// each (#123, `crate::bisect::plan`). The rows of each piece that ended since the last pass
/// come first. With `plans`, unless this runner holds the head, it plans the next pieces, in
/// the environments no running piece holds, and starts each on its own thread; `--once` then
/// waits for them.
///
/// A fleet finding (2026-10-27): once every post check ran, a pass ran all five one after
/// another, minutes each, and the next pass, which publishes the head, waited behind them;
/// nothing landed for 40 minutes. A pass that holds the head does none (`holds_the_head`), and
/// one environment runs one piece at a time. #123 (a fleet finding, 2026-10-29): one piece a
/// pass, on the pass's own thread, made each environment's post checks wait for every other's,
/// and every pass wait for the post check: now no pass waits for a piece.
///
/// G423 (a fleet finding, 2026-10-28): a post check runs for minutes, and its runner wrote no
/// presence meanwhile, so the others read it as gone and took work only it serves, which then
/// waited out a lease. It beats once a heartbeat while it plans and while `--once` waits; a
/// looping runner's passes beat as they go on.
fn post_pass(
    repo: &Arc<Repo>,
    post: &mut Post,
    plans: bool,
    (beat, depth, arrivals): (&mut crate::presence::Beat, usize, &mut Arrivals),
) -> Vec<Value> {
    let mut rows = post.collect();
    if plans && !holds_the_head(repo) {
        let busy = post.busy();
        let began = Instant::now();
        let mut placing = || arrivals.step(repo, "post");
        let plan = beat.meanwhile(repo, "post", depth, &mut placing, || {
            crate::bisect::plan(repo, &busy)
        });
        for (kind, row) in plan.rows {
            // E21: post work writes its own journal line, as an operation does.
            eprintln!(
                "bay runner: {} ({}s)",
                post_line(&post::kinded(kind, row.clone())),
                began.elapsed().as_secs()
            );
            rows.push((kind, row));
        }
        for piece in plan.pieces {
            rows.extend(post.start(repo, piece));
        }
    }
    if post.waits && post.holds() {
        let mut placing = || arrivals.step(repo, "post");
        rows.extend(beat.meanwhile(repo, "post", depth, &mut placing, || post.finish()));
    }
    post::rows(rows)
}

/// The re-exec point of a runner whose binary changed, between passes and never inside an
/// operation: the result that names the move, once the new binary is a release this runner may
/// move to and no post piece runs. A pin is a floor: a runner moves to its pin or a newer
/// release, never to a build that was never released, and never when the pin cannot be read.
///
/// #123: a running post piece is never handed over. The runner waits for it, driving
/// operations meanwhile and starting no new piece, and says so once for each version.
fn reexec_point(
    binary: &mut Binary,
    moves: bool,
    (post, waits_for): (&mut Post, &mut Option<String>),
    ran: &mut Vec<Value>,
) -> Option<BayResult> {
    let Some(next) = binary
        .version()
        .filter(|_| moves && binary.takes_arguments())
    else {
        eprintln!("bay runner: {}", binary.stays());
        binary.accept();
        return None;
    };
    match pinned_elsewhere(&next) {
        None if !post.idle() => {
            if waits_for.as_deref() != Some(next.as_str()) {
                eprintln!(
                    "bay runner: waits for {} post piece(s) to end before it moves to bay {next}",
                    post.pending()
                );
                *waits_for = Some(next);
            }
            None
        }
        None => {
            ran.extend(post::rows(post.collect()));
            Some(BayResult::ok(
                json!({"ran": std::mem::take(ran), "reexec": {
                    "from": crate::version::BAY,
                    "to": next,
                    "path": binary.path,
                }}),
            ))
        }
        Some(pin) => {
            if binary.hold() {
                eprintln!("bay runner: {}", binary.pinned(&next, &pin));
            }
            None
        }
    }
}

/// Whether this runner holds the queue head's operation, still running or checked and not yet
/// published. Such a runner does no post-merge work: its next pass publishes the head (a fleet
/// finding, 2026-10-27: a runner that held a checked head ran five post checks first, two of
/// them waiting 300 s each for a worker, and the head waited 40 minutes; every other runner
/// rightly waited on its lease).
fn holds_the_head(repo: &Repo) -> bool {
    let Some(head) = Queue::read_shared(repo)
        .ok()
        .and_then(|queue| queue.head().cloned())
    else {
        return false;
    };
    let mine = Keys::load(&repo.git).executor_name;
    Chain::new(&repo.git)
        .read(&head.change)
        .ok()
        .and_then(|(_, record)| {
            record
                .operations
                .into_iter()
                .find(|op| op.op_id == head.op_id)
        })
        .is_some_and(|op| held_by(&op, &mine))
}

/// Whether `op` is `me`'s, still running or checked, so `me` publishes it next.
fn held_by(op: &crate::change::Operation, me: &str) -> bool {
    op.owner.as_deref() == Some(me) && matches!(op.state, OpState::Running | OpState::Checked)
}

/// How long a check's scratch directory may stand untouched before a pass removes it.
const STALE_SCRATCH: Duration = Duration::from_hours(6);

/// Removes each check scratch directory of this repository that nothing has touched for
/// [`STALE_SCRATCH`]: a check that returns removes its own, so these are what a killed or
/// re-executed process left (a fleet finding, 2026-10-27: 17 GB on one box's runners). A
/// directory a running check writes to is newer than that, and is kept. Both roots are swept:
/// the scratch root ([`check::scratch_root`]) and `<common-dir>/bay/check/`, where a bay before
/// 0.5.42 made its checks' trees.
fn sweep_check_scratch(repo: &Repo) {
    let mut roots = vec![repo.common_dir.join("bay").join("check")];
    roots.extend(check::scratch_root(&repo.common_dir));
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|at| at.elapsed().ok())
                .is_some_and(|age| age >= STALE_SCRATCH);
            if stale && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                let _ = check::remove_tree(&entry.path());
            }
        }
    }
}

/// How long a pass starts operations before it ends and the next reads every chain again.
const PASS_BOUND: Duration = Duration::from_secs(300);

/// [`PASS_BOUND`]; a test build honours `BAY_TEST_PASS_BOUND_S`, a release build never does.
fn pass_bound() -> Duration {
    if option_env!("BAY_RELEASE_VERSION").is_none()
        && let Some(secs) = std::env::var("BAY_TEST_PASS_BOUND_S")
            .ok()
            .and_then(|secs| secs.parse().ok())
    {
        return Duration::from_secs(secs);
    }
    PASS_BOUND
}

/// The one journal line of a pass's outcome for `op_id`: the decision, the reason code when
/// there is one, and what it waits for.
fn op_line(op_id: &str, result: &BayResult) -> String {
    let decision = serde_json::to_value(result.decision)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    let mut line = format!("{op_id} {decision}");
    if let Some(reason) = result.reason {
        line.push(' ');
        line.push_str(reason.code());
    }
    if let Some(waiting) = &result.waiting_for {
        line.push_str(" waiting for ");
        line.push_str(waiting);
    }
    // A fleet finding (2026-10-26): a head that failed said only "failed", and nobody could
    // tell why. A failure names its cause, on the same one line.
    if result.decision == Decision::Failed
        && let Some(why) = result.detail.as_ref().and_then(|detail| {
            detail
                .as_str()
                .map(str::to_owned)
                .or_else(|| detail["why"].as_str().map(str::to_owned))
        })
    {
        let why: String = why.split_whitespace().collect::<Vec<_>>().join(" ");
        line.push_str(": ");
        line.extend(why.chars().take(400));
    }
    line
}

/// Runs operation `op_id` of change `id` under this runner's beat, and returns its row of the
/// pass. Its outcome is one journal line (stderr) and, when it merged, the early conflicts it
/// makes for the changes behind it (G177). While it runs, `arrivals` places what is submitted
/// meanwhile (S1).
fn run_op(
    repo: &Repo,
    id: &str,
    op_id: &str,
    depth: usize,
    beat: &mut crate::presence::Beat,
    notes: &mut Vec<Value>,
    arrivals: &mut Arrivals,
) -> Value {
    // A pass carries the whole queue, so it can run long: its presence beats before each
    // operation and once a heartbeat during it, naming it, never only at the pass's start, so a
    // busy runner never reads as gone.
    let step = publish::Step::new("read");
    let began = std::time::Instant::now();
    let mut placing = || arrivals.step(repo, op_id);
    let result = beat.during(repo, op_id, depth, &mut placing, || {
        publish::execute(repo, id, op_id, &step).unwrap_or_else(BayResult::failed)
    });
    // E21: what follows the operation (its receipt, the conflicts it tells the line of) is its
    // own step, never the last one the operation set. The receipt names that last one.
    let last = step.get();
    step.set("record");
    let result = receipted(repo, id, last, result).with_op(op_id.to_owned());
    // Trunk moved: what waits behind the new head hears of a conflict now (G177).
    if result.decision == Decision::Landed
        && let Ok(queue) = Queue::read(repo)
    {
        notes.extend(early_conflicts(repo, &queue));
    }
    eprintln!(
        "bay runner: {}",
        journal_line(repo, id, op_id, &result, &step, began)
    );
    row(id, op_id, &result)
}

/// The pass's row of `result`, the outcome of operation `op_id` of change `id`.
fn row(id: &str, op_id: &str, result: &BayResult) -> Value {
    json!({
        "change": id,
        "op_id": op_id,
        "decision": result.decision,
        "reason": result.reason,
        "detail": result.detail,
    })
}

/// The journal line of `result`, the outcome of operation `op_id` of change `id`, whose visit
/// began at `began` and went through `step`'s steps.
fn journal_line(
    repo: &Repo,
    id: &str,
    op_id: &str,
    result: &BayResult,
    step: &publish::Step,
    began: Instant,
) -> String {
    // One line per operation on stderr (a service's journal), so a stalled queue can be read
    // without the refs: what each pass did with each change, and why.
    // A fleet finding (2026-10-27): batches landed every ten minutes on a check of under two,
    // and no record said where the rest went. Each line names how long its operation took.
    // Owner ruling 2026-10-27: a line names the change by its intent too, after the operation
    // id a reader's tools find first.
    let line = op_line(op_id, result);
    let line = match crate::change::Chain::new(&repo.git).read(id) {
        Ok((_, record)) if !record.intent.reference.trim().is_empty() => {
            let intent: String = record.intent.reference.chars().take(60).collect();
            line.replacen(op_id, &format!("{op_id} [{intent}]"), 1)
        }
        _ => line,
    };
    // A fleet finding (2026-10-27): a merge took five to eight minutes on a loaded box, and the
    // line did not say where; it names each step of a second or more.
    let steps: Vec<String> = step
        .seconds()
        .into_iter()
        .map(|(name, secs)| format!("{name} {secs}s"))
        .collect();
    if steps.len() > 1 {
        format!(
            "{line} ({}s: {})",
            began.elapsed().as_secs(),
            steps.join(", ")
        )
    } else {
        format!("{line} ({}s)", began.elapsed().as_secs())
    }
}

/// S1 (bay 0.6.0; a fleet measurement, 2026-10-28): a runner placed a submitted change only
/// when its pass reached it, and a pass that ran a head or a pre-check placed nothing for
/// minutes. With four runners busy a new change waited 2.7 min p50 for its place (dehurfyw:
/// request 21:32:21Z, queued 21:36:55Z), and batches held 2.6 changes of 24.
///
/// So while the pass's work runs, its heartbeat thread lists the remote's land requests once a
/// heartbeat (`refs/bay/requests/*`, one listing) and visits each the pass has not seen, one at
/// a time, by [`publish::place`]: the change gets its place in line, or its refusal or wait,
/// written as a pass writes it. It never claims, checks or publishes, and it never visits the
/// change whose operation the pass runs. A visit that finds the operation may run now (the
/// head, or no live queue) leaves it to the next pass.
struct Arrivals {
    /// Each change the pass read, with its latest land request; `None` for a change that ended,
    /// which no request runs again. A request visited since is recorded here too.
    seen: HashMap<String, Option<String>>,
    /// The new requests listed and not yet visited, oldest first: (change, request).
    pending: std::collections::VecDeque<(String, String)>,
    /// When the remote's requests were last listed.
    listed: Instant,
    /// One row per visit, for the pass's result.
    rows: Vec<Value>,
}

impl Arrivals {
    /// What the pass read: `records`, its chains. Nothing is listed for a heartbeat.
    fn new(records: &[ChangeRecord]) -> Self {
        let seen = records
            .iter()
            .map(|record| {
                let latest = record
                    .operations
                    .iter()
                    .rev()
                    .find_map(|op| op.request_id.clone());
                (record.id.clone(), latest.filter(|_| !change_ended(record)))
            })
            .collect();
        Self {
            seen,
            pending: std::collections::VecDeque::new(),
            listed: Instant::now(),
            rows: Vec::new(),
        }
    }

    /// One step beside the work on `holding`: once a heartbeat it lists the new requests, and
    /// it visits at most one, so the work never waits on more than one visit when it ends.
    fn step(&mut self, repo: &Repo, holding: &str) {
        if self.pending.is_empty() {
            if self.listed.elapsed() < repo.config.heartbeat() {
                return;
            }
            self.listed = Instant::now();
            self.pending = self.listing(repo);
        }
        let Some((change, request)) = self.pending.pop_front() else {
            return;
        };
        self.seen.insert(change.clone(), Some(request.clone()));
        if let Some(row) = place_arrival(repo, &change, &request, holding) {
            self.rows.push(row);
        }
    }

    /// The remote's land requests this pass has not seen, oldest first, their chains fetched
    /// in one batch. A change that ended in the pass's reading is never one.
    fn listing(&self, repo: &Repo) -> std::collections::VecDeque<(String, String)> {
        let prefix = crate::names::REQUESTS;
        let Ok(listed) = repo
            .git
            .run(&["ls-remote", repo.remote(), &format!("{prefix}*")])
        else {
            return std::collections::VecDeque::new();
        };
        let new: Vec<(String, String)> = listed
            .lines()
            .filter_map(|line| {
                let (request, name) = line.split_once('\t')?;
                let change = name.strip_prefix(prefix)?;
                match self.seen.get(change) {
                    Some(None) => None,
                    Some(Some(known)) if known == request => None,
                    _ => Some((change.to_owned(), request.to_owned())),
                }
            })
            .collect();
        if new.is_empty() {
            return std::collections::VecDeque::new();
        }
        let changes: Vec<String> = new.iter().map(|(change, _)| change.clone()).collect();
        for (change, why) in repo.remote_chains(&changes) {
            eprintln!("bay runner: {change} could not be read from the remote: {why}");
        }
        let mut new: Vec<(u64, String, String)> = new
            .into_iter()
            .map(|(change, request)| {
                let at = repo
                    .git
                    .run(&["log", "-1", "--format=%ct", &request])
                    .ok()
                    .and_then(|at| at.parse().ok())
                    .unwrap_or(u64::MAX);
                (at, change, request)
            })
            .collect();
        new.sort();
        new.into_iter()
            .map(|(_, change, request)| (change, request))
            .collect()
    }
}

/// S1: places the operation of `change` that `request` asked for, unless it ended, already
/// holds a place, is held by a runner, waits for a worker, is a lane's, or belongs to the
/// change whose operation `holding` the pass runs. Its row, and one journal line.
fn place_arrival(repo: &Repo, change: &str, request: &str, holding: &str) -> Option<Value> {
    let (_, record) = Chain::new(&repo.git).read(change).ok()?;
    if change_ended(&record)
        || record.lane.is_some()
        || record.operations.iter().any(|op| op.op_id == holding)
    {
        return None;
    }
    let op = record
        .operations
        .iter()
        .find(|op| op.request_id.as_deref() == Some(request))?;
    let waits = matches!(op.state, OpState::Submitted | OpState::Admitted)
        && op.owner.is_none()
        && op.unavailable.is_none();
    if !waits || Queue::local(&repo.git).ok()?.of(&op.op_id).is_some() {
        return None;
    }
    let op_id = op.op_id.clone();
    let step = publish::Step::new("place");
    let began = Instant::now();
    let result = match publish::place(repo, change, &op_id, &step) {
        Ok(Some(result)) => result,
        Ok(None) => return None,
        Err(why) => BayResult::failed(why),
    };
    let last = step.get();
    let result = receipted(repo, change, last, result).with_op(op_id.clone());
    let line = journal_line(repo, change, &op_id, &result, &step, began);
    eprintln!("bay runner: {line}, placed while {holding} ran");
    Some(row(change, &op_id, &result))
}

/// The queue head's (change, operation) when nobody holds it, or its holder stopped writing its
/// chain for two leases, so this runner takes it between two other operations; `None` when there is no head,
/// it is `just` the operation this runner ran, or its chain moved within that time (a live holder
/// heartbeats it while a check runs).
///
/// A head whose operation already ended is released here and now, never run (a fleet finding,
/// 2026-10-26: a superseded operation of an abandoned change held the head for five hours, and
/// each runner "took" it after every operation, ran nothing, and moved on).
fn abandoned_head(repo: &Repo, just: &str) -> Option<(String, String)> {
    // G438: the pass's shared reads, and the head's chain read once for all three questions.
    let head = Queue::read_shared(repo).ok()?.head()?.clone();
    if head.op_id == just {
        return None;
    }
    let tip = repo.read_chain(&head.change).ok()?;
    let record = Chain::new(&repo.git).record_at(&tip).ok()?;
    if let Some(reason) = finished(&record, &head) {
        if let Err(why) = pass_on_or_release(repo, Some(&record), &head, &reason) {
            eprintln!(
                "bay runner: queue place {} could not be released: {why}",
                head.seq
            );
        }
        return None;
    }
    // BACKLOG 119: a head main's red holds waits in its place; the pass visits it in its turn.
    if waits_on_main(&record, &head.op_id) {
        return None;
    }
    // A head nobody holds (one that yielded because main moved, or gave its claim back) is
    // taken now, never left to a later pass (a fleet finding, 2026-10-26: a yielded head waited
    // twenty minutes while all three runners pre-checked changes behind it).
    if record
        .operations
        .iter()
        .find(|op| op.op_id == head.op_id)
        .is_some_and(|op| {
            runs(op)
                && op.owner.is_none()
                && matches!(op.state, OpState::Submitted | OpState::Admitted)
        })
    {
        return Some((head.change, head.op_id));
    }
    let written = repo
        .git
        .run(&["log", "-1", "--format=%ct", &tip])
        .ok()?
        .parse::<u64>()
        .ok()?;
    let idle = crate::live::unix_now().saturating_sub(written);
    (idle > repo.config.lease().as_secs() * 2).then_some((head.change, head.op_id))
}

/// G438: forgets the pass's shared reads after an operation that moved anything (it landed,
/// was refused or blocked, or gave up its place), so the next visit reads what it changed. A
/// place it took is written to this store's own queue refs, which a shared read sees.
fn forget_if_moved(repo: &Repo, ran: &[Value]) {
    if ran
        .last()
        .is_some_and(|row| row["decision"].as_str() != Some("queued"))
    {
        repo.forget_reads();
    }
}

/// Releases every queue place, anywhere in the line, whose operation has ended (a fleet
/// finding, 2026-10-26: five places outlived their operations, superseded, abandoned, or
/// reserved again after publishing, and each held the whole line once it reached the head).
/// It reads the chains the pass already holds, so it costs no read of its own; a release is
/// the one any runner writes. A place whose change is not among them is left to the head's
/// own release.
fn sweep_ended(repo: &Repo, queue: &Queue, records: &[ChangeRecord]) {
    for entry in queue
        .entries
        .values()
        .filter(|entry| !queue.released.contains(&entry.seq))
    {
        let Some(record) = records.iter().find(|record| record.id == entry.change) else {
            continue;
        };
        let Some(op) = record.operations.iter().find(|op| op.op_id == entry.op_id) else {
            continue;
        };
        let op_ended = op.state.is_terminal() || op.state == OpState::Blocked;
        if !(op_ended || change_ended(record)) {
            continue;
        }
        let reason = if op_ended {
            op.outcome.as_ref().map_or_else(
                || word(op.state).unwrap_or_default(),
                |outcome| outcome.to_lowercase(),
            )
        } else {
            word_of_change(record)
        };
        if let Err(why) = pass_on_or_release(repo, Some(record), entry, &reason) {
            eprintln!(
                "bay runner: queue place {} could not be released: {why}",
                entry.seq
            );
        }
    }
}

/// Whether `record`'s change has ended (abandoned, superseded, merged, or its work already on
/// trunk): none of its operations runs again, whatever state an older one was left in.
fn change_ended(record: &ChangeRecord) -> bool {
    matches!(
        record.state,
        crate::change::ChangeState::Abandoned
            | crate::change::ChangeState::Superseded
            | crate::change::ChangeState::Landed
            | crate::change::ChangeState::NoEffect
    )
    // A fleet finding (2026-10-27): a change an older bay reopened after it merged read `ready`
    // with a later revision `submitted`; every runner took that revision each pass and refused it
    // as already merged, a receipt each time. A change with a published operation is on main.
    || record
        .operations
        .iter()
        .any(|op| op.state == OpState::Published)
}

/// The release reason for a place of an ended change: its state's word.
fn word_of_change(record: &ChangeRecord) -> String {
    if record
        .operations
        .iter()
        .any(|op| op.state == OpState::Published)
    {
        return "published".to_owned();
    }
    serde_json::to_value(record.state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "ended".to_owned())
}

/// Orders a pass: queued changes first, in queue order, so one pass carries the queue as far as
/// it can; the rest oldest first by their chain's last write (a fleet finding, 2026-10-25: in
/// change-id order a change whose id sorted last waited two hours). An operation released from
/// the queue (while it waited for a worker, main's hold or a dependency, or after a refused
/// publishing push), or never yet placed, goes first of all (fleet findings, 2026-10-26: such a
/// change came last, after every pre-check, and rejoined an hour or more later); joining the
/// line takes no check.
fn in_pass_order(chains: &mut [crate::change::HeldChain], queue: &Queue) {
    let place = |id: &str| queue.of_change(id).map_or(u64::MAX, |entry| entry.seq);
    let rejoins = |held: &crate::change::HeldChain| {
        queue.of_change(&held.id).is_none() && held.record.operations.iter().any(runs)
    };
    chains.sort_by_key(|held| {
        let at = if rejoins(held) { 0 } else { place(&held.id) };
        (at, held.committed)
    });
}

/// Whether a pass runs `op`: it is neither terminal nor blocked, or, G284, it waits for a
/// person's approval ([`publish::awaits_approval`]), which the pass asks again, so the approval
/// lands it and nobody lands it a second time.
/// Whether a pass has anything to do for operation `op` of change `id` (a fleet finding,
/// 2026-10-26: a pass visited every one of 130 waiting changes, at several network round trips
/// each, only to find each still waiting; a pass took twenty minutes, and a change with real
/// work waited for it). An operation that holds a queue place beyond the head, the entries
/// checked ahead of their turn and one batch, and only waits there, is left for a later pass:
/// nothing can move it before the line reaches it. With footprint lanes on, every entry is.
///
/// `freed` is how many places this pass has ended so far: each moves the line up by one.
fn within_reach(repo: &Repo, queue: &Queue) -> impl Fn(&crate::change::Operation, usize) -> bool {
    // Footprint lanes (NEW-10) may check any entry whose footprint is apart from those ahead
    // of it, wherever it waits, so with them on every entry is within reach.
    let reach = if repo.config.footprint_lanes() {
        usize::MAX / 2
    } else {
        1 + repo.config.ahead() + repo.config.batch().map_or(0, |batch| batch.max)
    };
    let places: HashMap<String, usize> = queue
        .entries
        .values()
        .filter(|entry| !queue.released.contains(&entry.seq))
        .map(|entry| (entry.op_id.clone(), queue.position(entry.seq)))
        .collect();
    move |op, freed| {
        let waits = matches!(op.state, OpState::Submitted | OpState::Admitted)
            && op.unavailable.is_none()
            && op.owner.is_none();
        !waits
            || places
                .get(&op.op_id)
                .is_none_or(|position| *position <= reach + freed)
    }
}

/// Whether `record` names a change it fixes, reverts or supersedes.
fn names_a_repair(record: &ChangeRecord) -> bool {
    !record.repair_names().is_empty()
}

/// How many operations of `ran` reached an outcome that gives up a queue place, or wait in a
/// place the line passes because main's red holds them (BACKLOG 119).
fn freed(ran: &[Value]) -> usize {
    ran.iter()
        .filter(|row| {
            row["reason"] == Reason::TrunkRed.code()
                || row["decision"]
                    .as_str()
                    .is_some_and(|decision| !matches!(decision, "queued" | "ok"))
        })
        .count()
}

fn runs(op: &crate::change::Operation) -> bool {
    !op.state.is_terminal() && (op.state != OpState::Blocked || publish::asked_again(op))
}

/// `result`, the outcome a pass reached at `step` for change `id`, with its receipt
/// ([`publish::with_receipt`]). G284: a wait for approval that nothing changed was told when it
/// began, so a pass that finds it unchanged writes no receipt of its own; G574: so is a check
/// that gave no clear answer.
fn receipted(repo: &Repo, id: &str, step: &str, result: BayResult) -> BayResult {
    let unchanged = matches!(
        result.reason,
        Some(Reason::ApprovalPending | Reason::CheckUnknown)
    ) && result
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

/// 0.6.3: tells the change that owns each red post check of trunk, once (`crate::red_owner`).
/// A box that serves none of the policy's post checks judges nothing for post work (G444), so it
/// tells nothing.
fn red_owners(repo: &Repo) -> Vec<Value> {
    let tracking = format!("{}^{{commit}}", repo.trunk_tracking_ref());
    let Ok(trunk) = repo.git.run(&["rev-parse", "--verify", "-q", &tracking]) else {
        return Vec::new();
    };
    let trunk = trunk.trim();
    if !crate::bisect::serves_post_work(&repo.git, trunk) {
        return Vec::new();
    }
    crate::red_owner::tell(repo, trunk)
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
    // As the candidate merge reads them (`crate::merge`): attributes from main only.
    let (code, _, stderr) = repo
        .git
        .run_status_with(&quiet, &[(crate::merge::ATTR_SOURCE, trunk)])
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

#[cfg(test)]
mod tests {
    use super::{in_pass_order, op_line};
    use crate::change::{ChangeRecord, HeldChain};
    use crate::queue::{Entry, Queue};
    use crate::result::BayResult;

    fn held(id: &str, state: &str, committed: u64) -> HeldChain {
        let record: ChangeRecord = serde_json::from_value(serde_json::json!({
            "schema": "bay.change/1", "id": id,
            "intent": {"reference": id, "revision": "1"},
            "actor": {"name": "agent", "session": "s"},
            "base_first": "b", "base": "b", "observed_remote": "b",
            "declared_inputs": [], "state": "ready",
            "operations": [{
                "op_id": format!("{id}.1"), "revision": "r", "request_digest": "d",
                "state": state, "epoch": 0,
            }],
        }))
        .unwrap();
        HeldChain {
            id: id.to_owned(),
            head: String::new(),
            committed,
            record,
        }
    }

    /// E21: post work writes a journal line naming each check, its tip and its verdict.
    #[test]
    fn post_work_names_each_check_its_tip_and_verdict() {
        let done = serde_json::json!({"post": [
            {"check": "suite", "tip": "24e619a45e67bf46", "verdict": "product_failure"},
        ]});
        assert_eq!(
            super::post_line(&done),
            "post suite on 24e619a45e: product_failure"
        );
        assert_eq!(
            super::post_line(&serde_json::json!({"bisect": {}})),
            "bisect work"
        );
        let not_run = serde_json::json!({"post": [
            {"check": "suite", "tip": "24e619a45e67bf46",
             "verdict": {"not_run": "the evidence push was refused\nremote: busy"}},
        ]});
        assert_eq!(
            super::post_line(&not_run),
            "post suite on 24e619a45e: not run: the evidence push was refused; remote: busy"
        );
    }

    /// A fleet finding (2026-10-27): a change reopened after it merged kept a revision
    /// `submitted` that every runner retook each pass. A change with a published operation has
    /// ended, whatever its state says, so none of its operations runs and its place goes.
    #[test]
    fn a_change_with_a_published_operation_has_ended() {
        let mut reopened = held("hxzd0001", "submitted", 1).record;
        assert!(!super::change_ended(&reopened));
        let mut first = reopened.operations[0].clone();
        first.op_id = "hxzd0001.0".to_owned();
        first.state = crate::change::OpState::Published;
        reopened.operations.insert(0, first);
        assert!(
            super::change_ended(&reopened),
            "a published operation ends the change"
        );
    }

    /// A fleet finding (2026-10-27): a runner that held a checked head ran five post checks
    /// first and the head waited 40 minutes. A runner that holds the head's operation, running or
    /// checked, does no post-merge work; another runner's hold, or an ended one, does not stop it.
    #[test]
    fn a_runner_holding_the_head_does_no_post_work() {
        let mut op = held("head0001", "checked", 1).record.operations.remove(0);
        op.owner = Some("runner-a".to_owned());
        assert!(super::held_by(&op, "runner-a"));
        assert!(!super::held_by(&op, "runner-b"), "another runner's hold");
        op.state = crate::change::OpState::Running;
        assert!(super::held_by(&op, "runner-a"));
        op.state = crate::change::OpState::Published;
        assert!(!super::held_by(&op, "runner-a"), "a published head is done");
        op.owner = None;
        op.state = crate::change::OpState::Checked;
        assert!(!super::held_by(&op, "runner-a"), "a head nobody holds");
    }

    /// A fleet finding (2026-10-27): an abandoned change's older operation was queued after the
    /// abandon and run hours later. An ended change's operations never run, and its places go.
    #[test]
    fn an_ended_change_never_runs_whatever_its_operations_say() {
        let mut open = held("open0001", "admitted", 1).record;
        assert!(!super::change_ended(&open));
        for state in [
            crate::change::ChangeState::Abandoned,
            crate::change::ChangeState::Superseded,
            crate::change::ChangeState::Landed,
            crate::change::ChangeState::NoEffect,
        ] {
            open.state = state;
            assert!(super::change_ended(&open), "{state:?}");
        }
        assert_eq!(
            super::word_of_change(&open),
            "no_effect",
            "a place is released with the change's state"
        );
    }

    /// A fleet finding (2026-10-26): changes whose places were given back while main was held
    /// waited an hour after the hold ended, sorted behind every queued change's check. A change
    /// that waits with no place goes first, since joining the line takes no check.
    #[test]
    fn a_change_waiting_without_a_place_goes_first() {
        let mut queue = Queue::default();
        queue.entries.insert(
            7,
            Entry {
                seq: 7,
                commit: String::new(),
                op_id: "queued01.1".to_owned(),
                change: "queued01".to_owned(),
            },
        );
        let mut chains = vec![
            held("queued01", "admitted", 1),
            held("ended001", "published", 2),
            held("unplaced", "admitted", 3),
            held("checked1", "checked", 4),
        ];
        in_pass_order(&mut chains, &queue);
        let order: Vec<&str> = chains.iter().map(|held| held.id.as_str()).collect();
        assert_eq!(order[..3], ["unplaced", "checked1", "queued01"]);
    }

    /// A failure's journal line names its cause, on one line.
    #[test]
    fn a_failed_operation_names_its_cause_on_its_line() {
        let failed = BayResult::failed("git push: the remote refused\nthe ref");
        assert_eq!(
            op_line("abcd1234.1", &failed),
            "abcd1234.1 failed: git push: the remote refused the ref"
        );
    }
}
