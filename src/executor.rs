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

mod drain;
mod post;

pub use drain::drain;

use drain::Drain;
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
    // Named before it pins its binary, so an update between the two keeps this version's file.
    let _presence = Presence::hold();
    crate::update::pin_own_binary();
    // The supervisor's own sightings of queue heads, on its monotonic clock (§ 6).
    let mut heads: HashMap<u64, HeadSighting> = HashMap::new();
    let mut binary = Binary::now();
    // A looping executor moves to a new binary between operations; `--once` finishes its pass.
    let moves = !once && MOVES;
    // #123: the post pieces still running, across passes; `--once` waits for its own.
    let mut post = Post::new(once);
    // The version a runner waits to move to while its post pieces run, said once.
    let mut waits_for: Option<String> = None;
    // G589: a stop `bay runner drain` asks of this clone's runner, between operations.
    let mut drain = Drain::of(dir);
    // BACKLOG 157: the waits on a change nobody submitted, visited again once it moves.
    let mut parked = Parked::default();
    // G683: whether a failed pass is tried again, once a looping runner has made one.
    let mut retries = false;
    // G696: a looping runner watches another's lease across its passes, never inside one.
    if !once {
        publish::watch_leases_across_passes();
    }
    // G788: the line's head a wait between passes last ended for.
    let mut woke = None;
    loop {
        // Each pass reads the config at the trunk tip it fetched (ruling: config location),
        // so a config that landed since the last pass governs this one.
        let repo = match Repo::open_fetched(dir) {
            Ok(repo) => Arc::new(repo),
            Err(error) => match ends(error.into_result(), retries, dir, interval) {
                Some(result) => return result,
                None => continue,
            },
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
            (moves.then_some(&binary), (!once).then_some(&drain)),
            (&mut notes, &mut advanced),
            seen,
            &mut beat,
            (&mut post, &mut parked),
        ) {
            Ok(ran) => ran,
            Err(message) => match ends(BayResult::failed(message), retries, dir, interval) {
                Some(result) => return result,
                None => continue,
            },
        };
        retries = !once;
        // Between passes, never inside an operation: the re-exec point. G589: a drain asked of
        // this runner comes first: it stops there once its post pieces end, and its service
        // starts the installed binary, a new one too.
        if !once && drain.asked() {
            if let Some(drained) = drain.point(&mut post, &mut ran) {
                return drained;
            }
        } else if !once
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
        // G380, G789: a head that yielded its turn, which no operation after it took again,
        // runs in the next pass, at once: a pass that took the head before it read the chains
        // visits no change that landed meanwhile, which used to take it again.
        if !ends_yielded(&ran) {
            wait_for_line(&repo, interval, &mut woke);
        }
    }
}

/// G788: how often a runner asks the remote whether the line's head moved
/// ([`crate::queue::moved_head`]): between passes, and between a pass's operations.
const LINE_PROBE: Duration = Duration::from_secs(2);

/// G788 (a fleet measurement, 2026-11-18: the next change in line was claimed p50 58 to 79 s
/// after a publish freed the head, and four claims in five were another runner's, which learned
/// of it only on its next pass): the wait between passes is `interval`, or less once the line's
/// head on the remote is one this store has not read: the next pass starts at once and visits
/// it. `woke` is the head a wait already ended for, so a head the next pass could not read ends
/// no second wait.
fn wait_for_line(repo: &Repo, interval: Duration, woke: &mut Option<(u64, String)>) {
    let began = Instant::now();
    loop {
        if let Some(head) = crate::queue::moved_head(repo)
            && woke.as_ref() != Some(&head)
        {
            *woke = Some(head);
            return;
        }
        let left = interval.saturating_sub(began.elapsed());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(LINE_PROBE));
    }
}

/// G683 (a fleet finding, 2026-11-09: a runner stopped on one reset connection): the result a
/// failed pass ends the runner with, or `None` when it `retries`: a looping runner that has made
/// a pass says why this one failed and tries again after `interval`. A runner whose first pass
/// fails stops, as its setup is what is wrong; so does one whose repository at `dir` is gone,
/// which no later pass can serve: a runner a killed test left behind once retried in
/// its removed directory for good.
fn ends(result: BayResult, retries: bool, dir: &Path, interval: Duration) -> Option<BayResult> {
    if !retries || repository_gone(dir) {
        return Some(result);
    }
    eprintln!(
        "bay runner: this pass failed, the next tries again: {}",
        result.say()
    );
    std::thread::sleep(interval);
    None
}

/// Whether the repository at `dir` is gone: no `HEAD` is left in its git directory, a linked
/// work tree's `.git` file, or a bare one. The directories a runner writes its state in may
/// come back as it writes, so their presence says nothing.
fn repository_gone(dir: &Path) -> bool {
    !(dir.join(".git").is_file()
        || dir.join(".git").join("HEAD").is_file()
        || dir.join("HEAD").is_file())
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
/// under the same pid and supervisor. Its open readers end first (G803), so it leaves no child
/// the new binary would never wait for. Returns only when it cannot; this process then exits
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
        // G803: the exec keeps this process's children, and the new binary waits for none of them.
        crate::git::close_open_readers();
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
pub(crate) fn alive(pid: u32) -> bool {
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
    /// G694: when the fetch that read `after` began.
    read_at: Option<Instant>,
    /// G779: the queue head's change and the chain head its release check read from the remote.
    head_chain: Option<(String, String)>,
}

/// Releases a queue head that made no progress within its deadline, measured from this
/// supervisor's first sighting of it as head; a late observer or a restart only delays it.
/// An error is a release the remote refused or did not take; the next pass tries it again.
/// What it read of the queue goes to `seen`.
/// A queue head as this supervisor watches it for its progress deadline: when it first saw it
/// as head, the head of its change's chain and its holder's lease then
/// ([`crate::lease::watched`]), and when either last moved.
struct HeadSighting {
    first: Instant,
    chain: Option<String>,
    moved: Instant,
}

/// G740 (a fleet finding, 2026-11-15: a passing head was released mid-publish twice, 31
/// minutes after a runner first saw it as head, while its owner beat its lease every few
/// seconds through an 812 s batch check at load 70; the line stood still for 80 minutes):
/// whether a head past its progress deadline is released. Past the deadline its chain is watched:
/// a head whose owner moved it within the last lease is alive and keeps its place, up to twice
/// the deadline; one whose chain stood still for a lease is released, one lease after the
/// deadline at the earliest.
fn released_by_deadline(sighting: &HeadSighting, deadline: Duration, lease: Duration) -> bool {
    let waited = sighting.first.elapsed();
    waited >= deadline && (sighting.moved.elapsed() >= lease || waited >= deadline * 2)
}

fn deadlines(
    repo: &Repo,
    heads: &mut HashMap<u64, HeadSighting>,
    seen: &mut Seen,
) -> Result<(), String> {
    seen.read_at = Some(Instant::now());
    let mut queue = Queue::read(repo)?;
    seen.before = Some(queue.clone());
    // Heads whose operation already reached an outcome release at once, one after another.
    while let Some(head) = queue.head().cloned()
        && let Some(reason) = finished_head(repo, &head, seen)
    {
        let record = repo
            .remote_record(&head.change)
            .ok()
            .map(|remote| remote.record);
        pass_on_or_release(repo, record.as_ref(), &head, &reason)?;
        seen.read_at = Some(Instant::now());
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
    let deadline = repo.config.progress_deadline();
    let sighting = heads.entry(head.seq).or_insert_with(|| HeadSighting {
        first: Instant::now(),
        chain: None,
        moved: Instant::now(),
    });
    if sighting.first.elapsed() < deadline {
        seen.after = Some(queue);
        return Ok(());
    }
    // G740: past the deadline, the head's chain is read to see whether its owner still moves it,
    // or its lease (G787), which comes in the same fetch. The first read starts the watch: the
    // head then has one lease to show it moves.
    let chain = repo
        .remote_chain(&head.change)
        .ok()
        .map(|chain| crate::lease::watched(&repo.git, &head.change, &chain));
    if chain.is_some() && chain != sighting.chain {
        sighting.moved = Instant::now();
        sighting.chain = chain;
    }
    if !released_by_deadline(sighting, deadline, repo.config.lease()) {
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
fn finished_head(repo: &Repo, head: &crate::queue::Entry, seen: &mut Seen) -> Option<String> {
    // The remote's record, never moving this store's chain back (G289).
    let remote = repo.remote_record(&head.change).ok()?;
    // G779: the pass's first visit reuses this read of the head's chain.
    seen.head_chain = Some((head.change.clone(), remote.head));
    finished(&remote.record, head)
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
        .find(|op| record.waits(op))
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
/// runner to watch, it stops between operations once the runner is leaving ([`leaving`]). Before the
/// pass's own visits, and after each landing once the head it freed is taken, the entries behind
/// the queue's head are probed for a conflict with trunk (G177); what each probe noted goes to
/// `notes`.
///
/// G789 (a fleet measurement, 2026-11-18, at load 70 to 100: a pass woken by a publish spent
/// about 25 s on its setup, its fetches, sweeps, presence and reads of every chain, before it
/// visited the new head; a runner that landed spent p50 24 s on what followed its landing before
/// it took the next): the queue's head, when it is free to take ([`abandoned_head`]), is visited
/// first, and so is each head its landing frees, on what a head's visit reads itself: main and
/// the config, the queue and the head's chain as [`deadlines`] read them, and the evidence notes
/// its red gate judges main by. The rest of the setup follows them.
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
/// the next ones in `post`; once the runner is leaving it starts none, and the pass stops
/// between operations only when no piece runs.
#[allow(clippy::too_many_lines)]
fn pass(
    shared: &Arc<Repo>,
    watch: (Option<&Binary>, Option<&Drain>),
    (notes, advanced): (&mut Vec<Value>, &mut Vec<Value>),
    seen: Seen,
    beat: &mut crate::presence::Beat,
    (post, parked): (&mut Post, &mut Parked),
) -> Result<Vec<Value>, String> {
    let repo: &Repo = shared;
    // G438: this pass's visits share what it reads from the remote.
    repo.share_reads(true);
    // G779: and the head's chain the deadlines read, not fetched again by the first visit.
    if let Some((change, head)) = &seen.head_chain {
        repo.note_chain(change, head);
    }
    // G694: the queue the deadlines left is the pass's shared read of it, not fetched again
    // for the first visit.
    if seen.after.is_some()
        && let Some(at) = seen.read_at
    {
        repo.read_at(crate::repo::Shared::Queue, at);
    }
    // BACKLOG 113 (a review, 2026-10-28): a head's red gate judges main from this box's copy of
    // the post records. Only the post work fetched them, at a pass's end, so a box that runs no
    // post check judged a red main green and landed on it. The pass reads them before any visit.
    repo.fetch_evidence(&Keys::load(&repo.git));
    let began = std::time::Instant::now();
    let bound = pass_bound();
    let chain = Chain::new(&repo.git);
    let mut ran = Vec::new();
    // The heads this pass took first or between its operations: the loop, which reads the
    // chains as the pass began, never runs one of them again.
    let mut taken: HashSet<String> = HashSet::new();
    // G789: the head first, and each head a landing frees, before the rest of the setup; while
    // one runs, the beat's thread does beside it what the pass did before its visits
    // ([`Arrivals::first`]). The first take's visit begins with the deadline check's reads of the
    // queue and the head's chain, which it reads by. G585: a repair this store holds keeps its
    // turn before every other change.
    let mut arrivals: Option<Arrivals> = None;
    let mut just = String::new();
    let mut took = seen.read_at.unwrap_or_else(Instant::now);
    while !(leaving(watch) && post.idle())
        && (ran.is_empty() || began.elapsed() < bound)
        && let Some((head_id, head_op)) = abandoned_head(repo, &just)
        && !taken.contains(&head_op)
        && !parked.left_as_head(&head_op)
    {
        if arrivals.is_none() {
            let local = local_records(&chain);
            if local.iter().any(repairs) {
                break;
            }
            arrivals = Some(Arrivals::first(repo, &local, parked));
        }
        let Some(arrivals) = arrivals.as_mut() else {
            break;
        };
        taken.insert(head_op.clone());
        let depth = Queue::read_shared(repo).map_or(0, |queue| live_depth(&queue));
        let visit = (head_id.as_str(), head_op.as_str());
        let rows = run_op(repo, visit, Some(took), depth, beat, arrivals);
        record_rows(repo, rows, (&mut ran, &mut taken, parked));
        just = head_op;
        took = Instant::now();
    }
    // The places those heads freed are counted in the line as read from here on, and the probe
    // below tells the line of their landings.
    let first = ran.len();
    if let Some(arrivals) = arrivals.as_mut() {
        arrivals.owed = false;
    }
    let _ = receipts::sync(repo);
    sweep_check_scratch(repo);
    sweep_tmp_packs(repo);
    // G762: a store bay fetches into keeps a bounded number of packs.
    crate::packs::bound(&repo.common_dir);
    // G289, G294: never forced; a chain ahead of the remote keeps its work, one apart from it
    // is set aside.
    // G434 (a fleet finding, 2026-10-28): fourteen submitted changes no pass could read went
    // unnamed for hours, the fetch's error dropped. Each is named in the journal with Git's words.
    let mut unread = HashSet::new();
    for (change, why) in repo.fetch_chains().map_err(|error| error.message())? {
        let why = why.lines().map(str::trim).collect::<Vec<_>>().join("; ");
        eprintln!("bay runner: {change} could not be read from the remote: {why}");
        unread.insert(change);
    }
    let mut chains = chain.read_all().map_err(|error| error.message())?;
    parked.keep(&chains, &unread);
    // G789: as the heads taken first left it, read again only when one of them moved it.
    let queue = match seen.after {
        Some(queue) if first == 0 => queue,
        _ => Queue::read_shared(repo)?,
    };
    // G750: a `QUEUED` wait is visited again once the line's head or main moved.
    let trunk = repo
        .git
        .ref_target(&repo.trunk_tracking_ref())
        .ok()
        .flatten();
    parked.keep_stands(
        repo,
        queue.head().map(|entry| entry.op_id.as_str()),
        trunk.as_deref(),
    );
    in_pass_order(&mut chains, &queue);
    let records: Vec<ChangeRecord> = chains.iter().map(|held| held.record.clone()).collect();
    let mut arrivals = match arrivals {
        Some(mut arrivals) => {
            arrivals.know(&records);
            arrivals
        }
        None => Arrivals::new(&records),
    };
    sweep_ended(repo, &queue, &records);
    let holding = records
        .iter()
        .flat_map(|record| record.operations.iter().filter(|op| record.waits(op)))
        .next()
        .map(|op| op.op_id.as_str());
    let depth = live_depth(&queue);
    beat.publish(repo, holding, depth);
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
    // G788: when this pass last asked the remote whether the line's head moved.
    let mut probed = Instant::now();
    for record in &records {
        let id = record.id.as_str();
        let queued: Vec<String> = record
            .operations
            .iter()
            // A change that names one it fixes, reverts or supersedes is always reached: it may
            // be the repair of a red main, which runs ahead of the line (`queue_gate`).
            // A fleet finding (2026-10-27): an abandoned change's older operation was still
            // queued and run hours later. No operation of an ended change runs.
            .filter(|_| !record.ended())
            // G750: a repair is never parked: a red main, which no stand names, frees it.
            .filter(|op| !parked.holds(&op.op_id) || names_a_repair(record))
            .filter(|op| runs(op) && (names_a_repair(record) || reach(op, freed(&ran[first..]))))
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
            if leaving(watch) && post.idle() {
                ran.extend(recovered.take());
                ran.append(&mut arrivals.rows);
                notes.append(&mut arrivals.notes);
                return Ok(ran);
            }
            let rows = run_op(repo, (id, &op_id), None, depth, beat, &mut arrivals);
            record_rows(repo, rows, (&mut ran, &mut taken, parked));
            // G788: a head that moved on the remote since this pass read the line (another
            // runner's publish freed it) is read afresh, so it is taken below, in this pass. An
            // operation that just answered it waits behind another head is taken too, once it
            // is the head itself; a head that just yielded its turn (G380) is not. A landing read
            // the line afresh itself (G680).
            if probed.elapsed() >= LINE_PROBE && ran[ran.len() - 1]["decision"] != "merged" {
                probed = Instant::now();
                if crate::queue::moved_head(repo).is_some() {
                    repo.forget_reads();
                }
            }
            let waited = ran.last().is_some_and(|row| {
                row["reason"].as_str() == Some(Reason::Queued.code())
                    && row["detail"]["head"].as_str() != Some(op_id.as_str())
            });
            // F13 (2026-10-25, again 2026-10-26: a runner hung holding the head 19 minutes):
            // between operations, a head whose holder stopped writing is taken now, not at the
            // next pass's start an hour later. G789: its visit begins with the take's reads.
            let took = Instant::now();
            // G801: nor one this runner just left to another runner, while that one is live.
            if let Some((head_id, head_op)) = abandoned_head(repo, if waited { "" } else { &op_id })
                && !parked.left_as_head(&head_op)
            {
                let visit = (head_id.as_str(), head_op.as_str());
                let rows = run_op(repo, visit, Some(took), depth, beat, &mut arrivals);
                record_rows(repo, rows, (&mut ran, &mut taken, parked));
                taken.insert(head_op);
            }
            // G177, G789: a landing whose probe no operation beside it made probes now.
            if std::mem::take(&mut arrivals.owed)
                && let Ok(queue) = Queue::read_shared(repo)
            {
                notes.extend(early_conflicts(repo, &queue));
            }
        }
    }
    ran.extend(
        seen.before
            .and_then(|before| queue_retention(repo, &before, &chains)),
    );
    ran.extend(compaction(repo, &chain));
    ran.extend(recovered.take());
    let plans = !leaving(watch);
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
        // G599: a cut the host refused is named, never dropped; it is tried again each pass.
        match publish::retain(repo, &record.id) {
            Ok(Some(head)) => ran.push(json!({"change": record.id, "retention_cut": head})),
            Ok(None) => {}
            Err(error) => {
                ran.push(json!({"change": record.id, "retention_cut": null, "error": error}));
            }
        }
    }
    advanced.extend(advance(repo));
    ran.append(&mut arrivals.rows);
    notes.append(&mut arrivals.notes);
    Ok(ran)
}

/// Puts `rows`, a visit's ([`run_op`]), in the pass's `ran`, each noted in `parked`; the members
/// of a batch it landed are `taken`, never visited again in the pass (G789).
fn record_rows(
    repo: &Repo,
    rows: Vec<Value>,
    (ran, taken, parked): (&mut Vec<Value>, &mut HashSet<String>, &mut Parked),
) {
    for (index, row) in rows.into_iter().enumerate() {
        if index > 0
            && let Some(op_id) = row["op_id"].as_str()
        {
            taken.insert(op_id.to_owned());
        }
        parked.note(repo, &row);
        ran.push(row);
    }
    forget_if_moved(repo, ran);
}

/// Whether a head's last visit in `ran`, a pass's rows, yielded its turn (F11, G380): the pass
/// went on to the other changes, and the next pass runs it again.
fn ends_yielded(ran: &[Value]) -> bool {
    ran.iter()
        .filter(|row| row["detail"]["yielded"] == true)
        .any(|row| {
            ran.iter()
                .rev()
                .find(|last| last["op_id"] == row["op_id"])
                .is_some_and(|last| last["detail"]["yielded"] == true)
        })
}

/// The live places of `queue`: the depth a runner's presence names.
fn live_depth(queue: &Queue) -> usize {
    queue
        .entries
        .keys()
        .filter(|seq| !queue.released.contains(seq))
        .count()
}

/// G789: this store's own chains, as the last fetch left them, read without asking the remote:
/// what a head taken before the pass reads every chain places arrivals by.
fn local_records(chain: &Chain) -> Vec<HeldChain> {
    chain.read_all().unwrap_or_default()
}

/// Whether `held` is a change that names one it fixes, reverts or supersedes, with an operation
/// to run: G585, it runs before every other change, so no head is taken ahead of it.
fn repairs(held: &HeldChain) -> bool {
    !held.record.ended() && names_a_repair(&held.record) && held.record.operations.iter().any(runs)
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

/// Whether a looping runner is leaving: its binary changed, or a drain is asked (G589). A
/// leaving runner starts no post piece, and stops between operations once none runs.
fn leaving((binary, drain): (Option<&Binary>, Option<&Drain>)) -> bool {
    binary.is_some_and(Binary::changed) || drain.is_some_and(Drain::asked)
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
///
/// G763 (owner, 2026-11-16: "it's a real issue how much disk space Bay actually needs"; one box
/// held 41 GB of check scratch, one tree of 14 GB, every one older than two hours): a directory
/// named for an operation (`<change>.<n>.<attempt>`, or `…-own` for a check run in a view) whose
/// operation or change has ended, as this store's chain reads, is removed at once, whatever its
/// age; an operation still under way keeps its tree. The status sweep runs this too, so a store
/// no runner passes over (one agents' views share) is swept as well.
pub(crate) fn sweep_check_scratch(repo: &Repo) {
    let mut roots = vec![repo.common_dir.join("bay").join("check")];
    roots.extend(check::scratch_root(&repo.common_dir));
    let chain = Chain::new(&repo.git);
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let stale = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|at| at.elapsed().ok())
                .is_some_and(|age| age >= STALE_SCRATCH);
            if stale || scratch_of_ended(&chain, &entry.file_name().to_string_lossy()) {
                let _ = check::remove_tree(&entry.path());
            }
        }
    }
}

/// G763: whether check scratch directory `name` (`<change>.<n>.<attempt>`, maybe `-own`) is of
/// an operation that has ended, or of a change that has, by this store's chain. A name that
/// names no operation, or a change this store cannot read, is not judged ended.
fn scratch_of_ended(chain: &Chain, name: &str) -> bool {
    // A check run in a view (`-own`) may run for an operation that ended (a fix checked before it
    // is sent again), so only its change having ended frees its tree.
    let own = name.ends_with("-own");
    let name = name.strip_suffix("-own").unwrap_or(name);
    let mut parts = name.split('.');
    let (Some(change), Some(n)) = (parts.next(), parts.next()) else {
        return false;
    };
    if change.is_empty() || n.parse::<u64>().is_err() {
        return false;
    }
    let op_id = format!("{change}.{n}");
    let Ok((_, record)) = chain.read(change) else {
        return false;
    };
    record.ended()
        || (!own
            && record
                .operations
                .iter()
                .find(|op| op.op_id == op_id)
                .is_some_and(|op| op.state.is_terminal()))
}

/// How long a half-written pack may stand untouched before a pass removes it.
const STALE_TMP_PACK: Duration = Duration::from_hours(24);

/// BACKLOG 150 (a fleet finding, 2026-10-30: 244 such files, 591 MB, in one runner's clone):
/// removes each `objects/pack/tmp_*` file of this repository that nothing has touched for
/// [`STALE_TMP_PACK`]. Git's index-pack writes a fetched or pushed pack there first and renames
/// it when it ends; on a failed fetch, or one stopped at its time bound by any signal, it leaves
/// the file (measured: SIGTERM or SIGKILL, to the child or its group, each left one), and only
/// `git prune` removes it, after two weeks. A pack a fetch writes now is newer, and is kept.
pub(crate) fn sweep_tmp_packs(repo: &Repo) {
    let Ok(entries) = std::fs::read_dir(repo.common_dir.join("objects").join("pack")) else {
        return;
    };
    for entry in entries.flatten() {
        if is_tmp_pack(&entry) && tmp_pack_is_stale(&entry) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Whether this entry of `objects/pack` is a half-written pack.
pub(crate) fn is_tmp_pack(entry: &std::fs::DirEntry) -> bool {
    entry.file_name().to_string_lossy().starts_with("tmp_")
        && entry.file_type().is_ok_and(|kind| kind.is_file())
}

/// Whether nothing has touched this half-written pack for [`STALE_TMP_PACK`].
pub(crate) fn tmp_pack_is_stale(entry: &std::fs::DirEntry) -> bool {
    entry
        .metadata()
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|at| at.elapsed().ok())
        .is_some_and(|age| age >= STALE_TMP_PACK)
}

/// BACKLOG 157 (a fleet finding, 2026-10-30: 39% of one runner's journal lines): an operation
/// that waits on a change nobody submitted was visited every pass, each visit a read of the
/// remote, though only that change moving can free it. The waits a runner saw, across its
/// passes, by operation id: its change's chain head and that of each change it waits on, at the
/// visit. A pass visits it again only once one of them moved. Held in the runner's memory only:
/// after a restart each is visited once.
///
/// G750 (a fleet finding, 2026-11-16: two runners on one box visited each of 41 changes waiting
/// in line every pass, each visit 16 to 40 s of fetches, a push and a hundred local reads,
/// while the head checked for 86 minutes, and each visit answered `QUEUED` as the last had):
/// an operation answered `QUEUED` is parked too, with the queue's head operation and main's
/// commit it was answered under. It is visited again once its own chain moved, the queue's
/// head changed, main moved, or [`STAND_KEPT`] passed.
///
/// G801 (a fleet measurement, 2026-11-19: in 3 h one box's two runners made 122 visits to 25
/// heads whose checks only another live runner's box serves, a median 5 s each, mostly a fetch
/// of every presence; a head was visited again a median 61 s after the last, and between the
/// operations of a pass too): an operation left to another live runner is parked the same way,
/// under the queue's head and main's rules (its policy and red post checks, which name the
/// environments that serve it), while a presence that serves it, as each pass's fetch of the
/// chains brings it, stays live. A move of main that changes neither leaves it parked.
///
/// G805 (a fleet measurement, 2026-11-20: one box's runner answered 101 times an hour, 2.9 s
/// each, that another runner held one of two heads, a visit every 37 s while the holder's chain
/// stood still): an operation another runner holds is parked under its own chain, while its
/// holder's lease, as each pass's fetch of the chains brings it, moves within a lease (G696). It
/// is visited again once its chain moved, or once the lease stood a lease, when that visit
/// takes it over as before.
#[derive(Default)]
struct Parked(
    HashMap<String, Vec<(String, String)>>,
    HashMap<String, Stand>,
);

/// What an answer was given under, and until when it holds.
struct Stand {
    under: Under,
    until: Instant,
}

/// What a parked answer read that ends its stand once it moves, beside the operation's chain.
enum Under {
    /// G750: a `QUEUED` answer: the queue's head operation and main's commit.
    Queued { head: Option<String>, main: Main },
    /// G801: a leave to another runner: the queue's head operation and main's rules, and the
    /// environments it was left for, by which each pass renews the stand from the presences.
    Left {
        head: Option<String>,
        main: Main,
        environments: Vec<String>,
    },
    /// G805: an operation another runner holds, of `change`: each pass judges the stand right
    /// after its fetch brought the holder's lease, as a visit would judge it then ([`held_until`]),
    /// and it holds for the rest of the pass. It reads neither the line nor main.
    Held { change: String, owner: String },
}

impl Under {
    /// The queue's head operation and what of main the answer read, as this store reads them
    /// now; `None` when main's rules cannot be read.
    fn line(repo: &Repo, leave: bool) -> Option<(Option<String>, Main)> {
        let head = Queue::read_shared(repo)
            .ok()
            .and_then(|queue| queue.head().map(|entry| entry.op_id.clone()));
        let trunk = repo
            .git
            .ref_target(&repo.trunk_tracking_ref())
            .ok()
            .flatten();
        Some((head, Main::at(repo, trunk.as_deref(), leave)?))
    }

    /// The queue's head operation this answer was given under, when it read the line.
    fn head(&self) -> Option<&str> {
        match self {
            Self::Queued { head, .. } | Self::Left { head, .. } => head.as_deref(),
            Self::Held { .. } => None,
        }
    }
}

/// What of main a stand's answer read.
#[derive(PartialEq, Eq)]
enum Main {
    /// G750: main's commit: a `QUEUED` answer reads all of it.
    Commit(Option<String>),
    /// G801: main's policy (its revision and manifest digest) and its red post checks, each as
    /// `<check>@<environment>`: all a leave to another runner reads of main, the checks a change
    /// needs there and so the environments that serve it (`publish::served_elsewhere`).
    Rules(Option<(u64, String)>, Vec<String>),
}

impl Main {
    /// What a stand reads of main at `trunk`: its rules when the stand is a leave, else its
    /// commit. `None` when the rules cannot be read: no stand holds on them then.
    fn at(repo: &Repo, trunk: Option<&str>, leave: bool) -> Option<Self> {
        if !leave {
            return Some(Self::Commit(trunk.map(str::to_owned)));
        }
        let trunk = trunk?;
        let policy = crate::policy::load_at(&repo.git, trunk)
            .ok()?
            .map(|policy| (policy.revision, policy.manifest_digest));
        let registry = crate::registry::Registry::current(&repo.git).ok()?;
        let mut red: Vec<String> = crate::red::holding(&repo.git, trunk, registry.as_ref())
            .ok()?
            .into_iter()
            .map(|red| format!("{}@{}", red.check, red.environment))
            .collect();
        red.sort_unstable();
        red.dedup();
        Some(Self::Rules(policy, red))
    }
}

/// G750: the longest a `QUEUED` operation goes unvisited while nothing it was answered under
/// moved: what no key names (a red main, a policy, a deadline) is read again within this.
const STAND_KEPT: Duration = Duration::from_secs(600);

impl Parked {
    /// Keeps only the waits whose every chain holds, in `chains`, the head it held at the visit,
    /// none of them one in `unread`, which this pass could not read from the remote.
    fn keep(&mut self, chains: &[HeldChain], unread: &HashSet<String>) {
        let heads: HashMap<&str, &str> = chains
            .iter()
            .map(|held| (held.id.as_str(), held.head.as_str()))
            .collect();
        self.0.retain(|_, read| {
            read.iter().all(|(change, head)| {
                !unread.contains(change) && heads.get(change.as_str()) == Some(&head.as_str())
            })
        });
        let parked = &self.0;
        self.1.retain(|op_id, _| parked.contains_key(op_id));
    }

    /// G750: keeps only the stands answered under the queue head `head` and main at `trunk`,
    /// while they hold; the waits of any other stand end. G801: a stand of an operation left to
    /// another runner holds under main's rules at `trunk`, as long as a presence this store
    /// holds serves it, as this pass's fetch of the chains brought them. G805: a stand of an
    /// operation another runner holds, as long as that runner's lease moves.
    fn keep_stands(&mut self, repo: &Repo, head: Option<&str>, trunk: Option<&str>) {
        for stand in self.1.values_mut() {
            match &stand.under {
                Under::Left { environments, .. } => {
                    stand.until = serving_until(repo, environments).unwrap_or_else(Instant::now);
                }
                Under::Held { change, owner } => {
                    stand.until = held_until(repo, change, owner).unwrap_or_else(Instant::now);
                }
                Under::Queued { .. } => {}
            }
        }
        let now = Instant::now();
        let commit = Main::at(repo, trunk, false);
        // Main's rules are read only for a leave that still holds.
        let rules = self
            .1
            .values()
            .any(|stand| matches!(stand.under, Under::Left { .. }) && now < stand.until)
            .then(|| Main::at(repo, trunk, true))
            .flatten();
        let gone: Vec<String> = self
            .1
            .iter()
            .filter(|(_, stand)| {
                let moved = match &stand.under {
                    Under::Queued { main, .. } => {
                        stand.under.head() != head || commit.as_ref() != Some(main)
                    }
                    Under::Left { main, .. } => {
                        stand.under.head() != head || rules.as_ref() != Some(main)
                    }
                    Under::Held { .. } => false,
                };
                moved || stand.until <= now
            })
            .map(|(op_id, _)| op_id.clone())
            .collect();
        for op_id in gone {
            self.1.remove(&op_id);
            self.0.remove(&op_id);
        }
    }

    /// Whether `op_id` waits, and nothing it waits on moved, within its stand's time: a head
    /// left to another runner is visited again in the pass in which that runner may read stale.
    fn holds(&self, op_id: &str) -> bool {
        self.0.contains_key(op_id)
            && self
                .1
                .get(op_id)
                .is_none_or(|stand| Instant::now() < stand.until)
    }

    /// G801: whether `op_id`, the queue's head, was left to another runner while it was the
    /// head, and nothing it was left under moved. A change that waited behind the head is taken
    /// once it is the head itself: its `QUEUED` stand named another head.
    fn left_as_head(&self, op_id: &str) -> bool {
        self.holds(op_id)
            && self.1.get(op_id).is_some_and(|stand| {
                matches!(stand.under, Under::Left { .. }) && stand.under.head() == Some(op_id)
            })
    }

    /// Notes `row`, a visit's: a wait on changes not submitted or stopped (G604) is parked with
    /// the heads it read, and any other outcome ends what was parked for its operation.
    fn note(&mut self, repo: &Repo, row: &Value) {
        let (Some(change), Some(op_id)) = (row["change"].as_str(), row["op_id"].as_str()) else {
            return;
        };
        let waits = if row["reason"].as_str() == Some(Reason::DependencyPending.code()) {
            crate::after::stalled(row.get("detail"))
        } else {
            Vec::new()
        };
        let queued = row["decision"].as_str() == Some("queued")
            && row["reason"].as_str() == Some(Reason::Queued.code());
        let left = left_until(repo, row);
        let held = holder_answered(row);
        let own = Chain::new(&repo.git).head(change).ok().flatten();
        self.1.remove(op_id);
        let Some(own) =
            own.filter(|_| !waits.is_empty() || queued || left.is_some() || held.is_some())
        else {
            self.0.remove(op_id);
            return;
        };
        let mut read = vec![(change.to_owned(), own)];
        read.extend(waits.into_iter().map(|wait| (wait.change, wait.head)));
        self.0.insert(op_id.to_owned(), read);
        let stand = if queued {
            Under::line(repo, false)
                // A head that yielded keeps its place and runs next pass: never parked.
                .filter(|(head, _)| {
                    head.as_deref() != Some(op_id) && row["detail"]["yielded"] != true
                })
                .map(|(head, main)| Stand {
                    under: Under::Queued { head, main },
                    until: Instant::now() + STAND_KEPT,
                })
        } else if let Some((environments, until)) = left {
            Under::line(repo, true).map(|(head, main)| Stand {
                under: Under::Left {
                    head,
                    main,
                    environments,
                },
                until,
            })
        } else if let Some(owner) = held {
            held_until(repo, change, &owner).map(|until| Stand {
                under: Under::Held {
                    change: change.to_owned(),
                    owner,
                },
                until,
            })
        } else {
            return;
        };
        match stand {
            Some(stand) => {
                self.1.insert(op_id.to_owned(), stand);
            }
            None => {
                self.0.remove(op_id);
            }
        }
    }
}

/// G801: the environments `row`, a visit's, left its operation to another live runner for,
/// and until when: a wait for a worker this box has none of (`CHECK_UNAVAILABLE`, with no
/// adapter's reason), while a presence this store holds, as the visit just fetched it, serves
/// them. `None` for any other row.
fn left_until(repo: &Repo, row: &Value) -> Option<(Vec<String>, Instant)> {
    if row["decision"].as_str() != Some("queued")
        || row["reason"].as_str() != Some(Reason::CheckUnavailable.code())
        || !row["detail"]["unavailable"].is_null()
    {
        return None;
    }
    let environments: Vec<String> = row["detail"]["environments"]
        .as_array()?
        .iter()
        .filter_map(|environment| environment.as_str().map(str::to_owned))
        .collect();
    let until = serving_until(repo, &environments)?;
    Some((environments, until))
}

/// G805: the holder `row`, a visit's, answered holds its operation (`detail.lease` `held`, and
/// its `owner`); `None` for any other row.
fn holder_answered(row: &Value) -> Option<String> {
    (row["decision"].as_str() == Some("ok") && row["detail"]["lease"].as_str() == Some("held"))
        .then(|| row["detail"]["owner"].as_str().map(str::to_owned))
        .flatten()
}

/// G805: until when a stand of an operation of `change` that `owner` holds holds: the rest of
/// this pass, [`PASS_BOUND`], while the holder counts as alive by the rule a visit judges it by
/// now (G696, [`publish::sighted`]): this runner first saw the change's chain and lease, as this
/// store holds them, as they are, less than a lease ago. Each pass's fetch of the chains brings
/// the leases, so a holder that beats stays alive; once one that stopped stood a lease, the pass
/// that judges it visits the operation and takes it over, as a visit every pass did. A pass is
/// judged right after its fetch, not later in it: a lease's beat between two fetches is unseen
/// until the next. `None` when it is not alive, this process does not loop, or the store holds
/// no chain of `change`.
fn held_until(repo: &Repo, change: &str, owner: &str) -> Option<Instant> {
    let head = Chain::new(&repo.git).head(change).ok().flatten()?;
    let seen = publish::sighted(&repo.git, change, &head)?;
    (seen.elapsed() < publish::holder_lease(&repo.config, Some(owner)))
        .then(|| Instant::now() + PASS_BOUND)
}

/// G801: until when another runner's presence this store holds serves every one of
/// `environments`, by the rule a visit judges it live by ([`crate::presence::serving_until`]).
fn serving_until(repo: &Repo, environments: &[String]) -> Option<Instant> {
    let me = crate::presence::presence_ref(&Keys::load(&repo.git).executor_name);
    let until = crate::presence::serving_until(&repo.git, &me, environments)?;
    let left = until.checked_sub(crate::presence::reader_now())?;
    Instant::now().checked_add(Duration::from_secs(left))
}

/// How long a pass starts operations before it ends and the next reads every chain again.
const PASS_BOUND: Duration = Duration::from_secs(300);

/// [`PASS_BOUND`]; a test build honours `BAY_TEST_PASS_BOUND_S`, a release build never does.
fn pass_bound() -> Duration {
    if crate::version::TEST_HOOKS
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
    use std::fmt::Write as _;
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
    // BACKLOG 157, G604: a wait on a change that was never submitted, or that stopped, names it
    // and who must act on it.
    if result.reason == Some(Reason::DependencyPending) {
        for dependency in crate::after::stalled(result.detail.as_ref()) {
            let _ = write!(line, "; {}", dependency.clause());
        }
    }
    line
}

/// Runs operation `op_id` of change `id` under this runner's beat, and returns its row of the
/// pass, then one for each other member of a batch it landed ([`publish::members_landed`],
/// G789), which the pass visits no more. Each outcome is one journal line (stderr). While it
/// runs, `arrivals` places what is submitted meanwhile (S1). `took` is when this runner began
/// to read for its take of the queue's head, when that is what it runs ([`publish::execute`]).
fn run_op(
    repo: &Repo,
    (id, op_id): (&str, &str),
    took: Option<Instant>,
    depth: usize,
    beat: &mut crate::presence::Beat,
    arrivals: &mut Arrivals,
) -> Vec<Value> {
    // A pass carries the whole queue, so it can run long: its presence beats before each
    // operation and once a heartbeat during it, naming it, never only at the pass's start, so a
    // busy runner never reads as gone.
    let step = publish::Step::new("read");
    let began = std::time::Instant::now();
    let _ = crate::git::take_times(0);
    let mut placing = || arrivals.step(repo, op_id);
    let result = beat.during(repo, op_id, depth, &mut placing, || {
        publish::execute(repo, id, op_id, &step, took).unwrap_or_else(BayResult::failed)
    });
    // E21: what follows the operation (its receipt, the conflicts it tells the line of) is its
    // own step, never the last one the operation set. The receipt names that last one.
    let last = step.get();
    step.set("record");
    let result = receipted(repo, id, last, result).with_op(op_id.to_owned());
    // BACKLOG 92: what the pass read before the landing is forgotten here ([`forget_if_moved`]),
    // so the abandoned-head check after it reads the queue afresh. G177, G789: main moved, so
    // the changes behind the new head hear of a conflict now, beside the next operation, never
    // ahead of it ([`Arrivals::step`]).
    if result.decision == Decision::Landed {
        repo.forget_reads();
        arrivals.owed = true;
    }
    eprintln!(
        "bay runner: {}",
        journal_line(repo, id, op_id, &result, &step, began)
    );
    let mut rows = vec![row(id, op_id, &result)];
    for (change, member, landed) in publish::members_landed() {
        let landed = landed.with_op(member.clone());
        eprintln!(
            "bay runner: {}, in {op_id}'s batch",
            named(repo, &change, &member, op_line(&member, &landed))
        );
        rows.push(row(&change, &member, &landed));
    }
    rows
}

/// `line`, a journal line of operation `op_id` of change `id`, naming the change by its intent
/// too, after the operation id a reader's tools find first (owner ruling 2026-10-27).
fn named(repo: &Repo, id: &str, op_id: &str, line: String) -> String {
    match crate::change::Chain::new(&repo.git).read(id) {
        Ok((_, record)) if !record.intent.reference.trim().is_empty() => {
            let intent: String = record.intent.reference.chars().take(60).collect();
            line.replacen(op_id, &format!("{op_id} [{intent}]"), 1)
        }
        _ => line,
    }
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
    let line = named(repo, id, op_id, op_line(op_id, result));
    // A fleet finding (2026-10-27): a merge took five to eight minutes on a loaded box, and the
    // line did not say where; it names each step of a second or more.
    let steps: Vec<String> = step
        .seconds()
        .into_iter()
        .map(|(name, secs)| format!("{name} {secs}s"))
        .collect();
    // G669 (a merge run of 65 s with a 1 s check, 30 s of it in "publish"): which Git calls.
    let git = crate::git::take_times(GIT_TIMES_SHOWN);
    if steps.len() > 1 {
        format!(
            "{line} ({}s: {}; git: {git})",
            began.elapsed().as_secs(),
            steps.join(", ")
        )
    } else {
        format!("{line} ({}s; git: {git})", began.elapsed().as_secs())
    }
}

/// The Git commands a runner's line for an operation names.
const GIT_TIMES_SHOWN: usize = 6;

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
///
/// G789: the same thread probes the line for conflicts (G177) that a landing owes, beside the
/// operation that follows the landing, so the probe never stands before the next head.
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
    /// G789: main moved under the line since it was last probed for conflicts.
    owed: bool,
    /// What the probes noted, for the pass's notes.
    notes: Vec<Value>,
}

impl Arrivals {
    /// What the pass read: `records`, its chains. Nothing is listed for a heartbeat.
    fn new(records: &[ChangeRecord]) -> Self {
        Self {
            seen: Self::latest(records).collect(),
            pending: std::collections::VecDeque::new(),
            listed: Instant::now(),
            rows: Vec::new(),
            owed: false,
            notes: Vec::new(),
        }
    }

    /// G789: the arrivals of a pass that takes its head before it reads every chain, by
    /// `chains`, this store's own as the last pass left them. Beside that head's operation the
    /// beat's thread does what the pass did before its visits: it probes the line for conflicts
    /// (G177); it places each operation of `chains` that holds no place, released from the line
    /// or never placed, oldest first, none of them parked (BACKLOG 157, G750), as the pass's
    /// order put them before the head. The remote's new land requests it lists once a heartbeat,
    /// as ever (G680: one listing a pass).
    fn first(repo: &Repo, chains: &[HeldChain], parked: &Parked) -> Self {
        let records: Vec<ChangeRecord> = chains.iter().map(|held| held.record.clone()).collect();
        let mut arrivals = Self::new(&records);
        arrivals.owed = true;
        let queue = Queue::local(&repo.git).unwrap_or_default();
        let mut placeless: Vec<(u64, String, String)> = chains
            .iter()
            .filter(|held| {
                !held.record.ended()
                    && held.record.lane.is_none()
                    && queue.of_change(&held.id).is_none()
            })
            .filter_map(|held| {
                let op = held
                    .record
                    .operations
                    .iter()
                    .rev()
                    .find(|op| runs(op) && !parked.holds(&op.op_id))?;
                Some((held.committed, held.id.clone(), op.request_id.clone()?))
            })
            .collect();
        placeless.sort();
        arrivals.pending = placeless
            .into_iter()
            .map(|(_, change, request)| (change, request))
            .collect();
        arrivals
    }

    /// G789: `records`, the chains the pass read once its heads taken first ran, which this
    /// store's own chains stood for until then. The requests are listed a heartbeat after that
    /// read, as when the pass made its arrivals there (G680: the pass's setup listed them).
    fn know(&mut self, records: &[ChangeRecord]) {
        self.seen.extend(Self::latest(records));
        self.listed = Instant::now();
    }

    /// Each change of `records` with its latest land request; `None` for one that ended.
    fn latest(records: &[ChangeRecord]) -> impl Iterator<Item = (String, Option<String>)> + '_ {
        records.iter().map(|record| {
            let latest = record
                .operations
                .iter()
                .rev()
                .find_map(|op| op.request_id.clone());
            (record.id.clone(), latest.filter(|_| !record.ended()))
        })
    }

    /// One step beside the work on `holding`: a probe owed first, on the line as this store last
    /// read it and the chains of the places it probes as the remote holds them now; then once a
    /// heartbeat it lists the new requests, and it visits at most one, so the work never waits on
    /// more than one visit when it ends.
    fn step(&mut self, repo: &Repo, holding: &str) {
        if std::mem::take(&mut self.owed) {
            if let Ok(queue) = Queue::local(&repo.git) {
                let changes: Vec<String> =
                    probed(&queue).map(|entry| entry.change.clone()).collect();
                let _ = repo.remote_chains(&changes);
                self.notes.extend(early_conflicts(repo, &queue));
            }
            return;
        }
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
    // G760: nor one of a change in the batch this runner lands; a later pass places it.
    if record.ended()
        || record.lane.is_some()
        || record.operations.iter().any(|op| op.op_id == holding)
        || publish::in_running_batch(change)
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
    let _ = crate::git::take_times(0);
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
/// chain and beating its lease for two leases, so this runner takes it between two other
/// operations; `None` when there is no head, it is `just` the operation this runner ran, or its
/// chain or its lease moved within that time (a live holder beats its lease while a check runs,
/// G787).
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
    // A head nobody holds (one that yielded because main moved, or gave its claim back) is
    // taken now, never left to a later pass (a fleet finding, 2026-10-26: a yielded head waited
    // twenty minutes while all three runners pre-checked changes behind it). G797: so is one that
    // kept its place after another runner's worker answered `unavailable`: it holds no lease.
    let me = crate::signing::Keys::load(&repo.git).executor_name;
    if record
        .operations
        .iter()
        .find(|op| op.op_id == head.op_id)
        .is_some_and(|op| {
            runs(op)
                && ((op.owner.is_none()
                    && matches!(op.state, OpState::Submitted | OpState::Admitted))
                    || (op.unavailable.is_some()
                        && op.kept.as_ref().is_some_and(|kept| kept.place == head.seq)
                        && op.owner.as_deref() != Some(me.as_str())))
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
    let op = record.operations.iter().find(|op| op.op_id == head.op_id);
    // G787: the holder's last beat of its lease, which the chain read brought.
    let beat = op.and_then(|op| crate::lease::beat_at(&repo.git, &head.change, op));
    let idle = crate::live::unix_now().saturating_sub(written.max(beat.unwrap_or(0)));
    // G679: a change's own run that stopped holds a short lease, not a runner's.
    let owner = op.and_then(|op| op.owner.as_deref());
    let lease = crate::publish::holder_lease(&repo.config, owner).as_secs();
    (idle > lease * 2).then_some((head.change, head.op_id))
}

/// G438: forgets the pass's shared reads after an operation that moved anything (it landed,
/// was refused or blocked, or gave up its place), so the next visit reads what it changed. A
/// place it took is written to this store's own queue refs, which a shared read sees.
fn forget_if_moved(repo: &Repo, ran: &[Value]) {
    // A landing forgot them before its own fresh read of the queue (`run_op`).
    if ran
        .last()
        .is_some_and(|row| !matches!(row["decision"].as_str(), Some("queued" | "merged")))
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
        if !(op_ended || record.ended()) {
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
/// line takes no check. G585 (a fleet finding, 2026-10-30: a red main's repair, placed behind
/// twenty changes, waited 14 minutes while each pass ran out its bound first): a change that
/// names one it fixes, reverts or supersedes goes before all of them.
fn in_pass_order(chains: &mut [crate::change::HeldChain], queue: &Queue) {
    let place = |id: &str| queue.of_change(id).map_or(u64::MAX, |entry| entry.seq);
    let rejoins = |held: &crate::change::HeldChain| {
        queue.of_change(&held.id).is_none() && held.record.operations.iter().any(runs)
    };
    chains.sort_by_key(|held| {
        let at = if names_a_repair(&held.record) {
            (0, 0)
        } else if rejoins(held) {
            (1, 0)
        } else {
            (2, place(&held.id))
        };
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

/// How many operations of `ran` reached an outcome that gives up a queue place.
fn freed(ran: &[Value]) -> usize {
    ran.iter()
        .filter(|row| {
            row["decision"]
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
/// that gave no clear answer; G600: so is an operation another runner blocked.
fn receipted(repo: &Repo, id: &str, step: &str, result: BayResult) -> BayResult {
    if unchanged(&result) {
        result.with_change(id)
    } else {
        publish::with_receipt(repo, id, step, result)
    }
}

/// Whether a visit found its operation as it was and wrote nothing ([`receipted`]).
fn unchanged(result: &BayResult) -> bool {
    result
        .detail
        .as_ref()
        .is_some_and(|detail| detail["unchanged"] == true)
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

/// G741: the longest a finished operation's queue pair is kept after its release.
const QUEUE_KEPT_S: u64 = 3600;

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
    // G741 (a fleet finding, 2026-11-15: a remote held 1,127 queue places and 1,126 releases,
    // one place live, and listing them took 43 s): a finished operation's pair is kept at most
    // QUEUE_KEPT_S after its release, not the whole retention window. Nothing reads it later:
    // queue waits come from the terminal records, and a deadline count reads only places of
    // operations still waiting.
    let kept = repo
        .config
        .retention_window()
        .min(Duration::from_secs(QUEUE_KEPT_S));
    let cutoff = now.saturating_sub(kept.as_secs());
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
    for entry in probed(queue) {
        let Ok((_, record)) = chain.read(&entry.change) else {
            continue;
        };
        let Some(op) = record
            .operations
            .iter()
            .find(|op| op.op_id == entry.op_id)
            .filter(|op| record.waits(op))
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

/// The places [`early_conflicts`] probes: up to [`EARLY_PROBES`] live ones behind the head. G760,
/// G789: never a member of the batch this runner lands, whose chain its claims and persists
/// write, as the probe may run beside it.
fn probed(queue: &Queue) -> impl Iterator<Item = &crate::queue::Entry> {
    queue
        .entries
        .values()
        .filter(|entry| !queue.released.contains(&entry.seq))
        .skip(1)
        .filter(|entry| !publish::in_running_batch(&entry.change))
        .take(EARLY_PROBES)
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
    use super::{Parked, in_pass_order, op_line};
    use crate::change::{ChangeRecord, HeldChain};
    use crate::queue::{Entry, Queue};
    use crate::result::{BayResult, Decision, Reason};
    use std::collections::HashSet;

    #[test]
    fn a_head_past_its_deadline_keeps_its_place_while_its_chain_moves() {
        use super::{HeadSighting, released_by_deadline};
        use std::time::{Duration, Instant};
        let deadline = Duration::from_secs(100);
        let lease = Duration::from_secs(10);
        let ago = |secs| {
            Instant::now()
                .checked_sub(Duration::from_secs(secs))
                .unwrap()
        };
        let sighting = |first, moved| HeadSighting {
            first: ago(first),
            chain: Some("c".to_owned()),
            moved: ago(moved),
        };
        // Before the deadline nothing is released.
        assert!(!released_by_deadline(&sighting(50, 50), deadline, lease));
        // Past it, a chain that moved within the lease keeps its place.
        assert!(!released_by_deadline(&sighting(150, 3), deadline, lease));
        // A chain that stood still for a lease is released.
        assert!(released_by_deadline(&sighting(150, 20), deadline, lease));
        // Twice the deadline releases a moving one too.
        assert!(released_by_deadline(&sighting(250, 1), deadline, lease));
    }

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

    /// G600: a visit of an operation another runner blocked writes no receipt, whatever its
    /// reason; a visit that blocked it here does.
    #[test]
    fn an_unchanged_visit_of_any_blocked_operation_writes_no_receipt() {
        let visit = |detail| BayResult::blocked(Reason::CheckFailed).with_detail(detail);
        assert!(super::unchanged(&visit(
            serde_json::json!({"unchanged": true})
        )));
        assert!(!super::unchanged(&visit(serde_json::json!({"checks": []}))));
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
        assert!(!reopened.ended());
        let mut first = reopened.operations[0].clone();
        first.op_id = "hxzd0001.0".to_owned();
        first.state = crate::change::OpState::Published;
        reopened.operations.insert(0, first);
        assert!(reopened.ended(), "a published operation ends the change");
    }

    /// BACKLOG 213 (a fleet finding, 2026-11-02: a change landed 10-28 with a later revision
    /// still `admitted`, and three runners named that revision as the operation they held for
    /// five days). An ended change's operation waits for no executor, whatever state it was left
    /// in: no runner names it as held, and no session counts it as waiting.
    #[test]
    fn an_ended_changes_operation_waits_for_no_executor() {
        let open = held("kgsg0001", "admitted", 1).record;
        assert!(
            open.waits(&open.operations[0]),
            "an open change's admitted revision"
        );
        assert_eq!(crate::presence::waiting(&open), 1);

        let mut landed = open.clone();
        let mut first = landed.operations[0].clone();
        first.op_id = "kgsg0001.0".to_owned();
        first.state = crate::change::OpState::Published;
        landed.operations.insert(0, first);
        assert!(
            !landed.waits(&landed.operations[1]),
            "a revision left after the merge"
        );
        assert_eq!(crate::presence::waiting(&landed), 0);

        let mut abandoned = open;
        abandoned.state = crate::change::ChangeState::Abandoned;
        assert!(
            !abandoned.waits(&abandoned.operations[0]),
            "an abandoned change's revision"
        );
        assert_eq!(crate::presence::waiting(&abandoned), 0);
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
        assert!(!open.ended());
        for state in [
            crate::change::ChangeState::Abandoned,
            crate::change::ChangeState::Superseded,
            crate::change::ChangeState::Landed,
            crate::change::ChangeState::NoEffect,
        ] {
            open.state = state;
            assert!(open.ended(), "{state:?}");
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

    /// BACKLOG 157: a wait on a change nobody submitted names it and its actor on its line.
    #[test]
    fn a_wait_on_an_unsubmitted_change_names_it_and_its_actor() {
        let waiting = BayResult::refused(Decision::Queued, Reason::DependencyPending).with_detail(
            serde_json::json!({"pending": [
                {"change": "c1", "state": "ready", "submitted": false, "actor": "ann", "head": "h"},
                {"change": "c2", "state": "ready"},
            ]}),
        );
        assert_eq!(
            op_line("abcd1234.1", &waiting),
            "abcd1234.1 queued DEPENDENCY_PENDING; c1 is not submitted: its actor ann must run bay merge for it"
        );
    }

    /// G604: a wait on a stopped change names it, its blocked operation and who must act on it.
    #[test]
    fn a_wait_on_a_stopped_change_names_its_block_and_who_frees_it() {
        let waiting = BayResult::refused(Decision::Queued, Reason::DependencyPending).with_detail(
            serde_json::json!({"pending": [
                {"change": "c1", "state": "ready", "actor": "ann", "head": "h",
                    "blocked": {"op": "c1.2", "reason": "CHECK_FAILED"}},
                {"change": "c2", "state": "ready", "actor": "bo", "head": "h",
                    "blocked": {"op": "c2.1", "reason": "APPROVAL_PENDING"}},
                {"change": "c3", "state": "ready", "actor": "cy", "head": "h",
                    "blocked": {"op": "c3.1", "reason": null}},
            ]}),
        );
        assert_eq!(
            op_line("abcd1234.1", &waiting),
            "abcd1234.1 queued DEPENDENCY_PENDING; c1 is blocked (c1.2 CHECK_FAILED): its actor ann must act on it, as bay status c1 says; c2 waits for an approval (c2.1): an approver must run bay approve c2; c3 is blocked (c3.1): its actor cy must act on it, as bay status c3 says"
        );
    }

    /// BACKLOG 157: a parked wait stays parked only while every chain it read holds its head.
    #[test]
    fn a_parked_wait_is_visited_again_once_a_chain_it_read_moved() {
        let chains = |dependency: &str| {
            let mut own = held("w", "submitted", 0);
            own.head = "w1".to_owned();
            let mut on = held("d", "failed", 0);
            on.head = dependency.to_owned();
            vec![own, on]
        };
        let parked = || {
            let mut parked = Parked::default();
            parked.0.insert(
                "w.1".to_owned(),
                vec![("w".into(), "w1".into()), ("d".into(), "d1".into())],
            );
            parked
        };
        let mut still = parked();
        still.keep(&chains("d1"), &HashSet::new());
        assert!(still.holds("w.1"));
        let mut moved = parked();
        moved.keep(&chains("d2"), &HashSet::new());
        assert!(!moved.holds("w.1"), "a dependency moved");
        let mut unread = parked();
        unread.keep(&chains("d1"), &HashSet::from(["d".to_owned()]));
        assert!(!unread.holds("w.1"), "a dependency could not be read");
        let mut gone = parked();
        gone.keep(&chains("d1")[1..], &HashSet::new());
        assert!(!gone.holds("w.1"), "its own chain is gone");
    }
}
