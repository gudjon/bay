//! A bay origin (0.7.0): a bay host near a fleet that serves the fleet's bay traffic while a
//! hosted remote stays its upstream.
//!
//! A client whose config names the origin (`[origin] url`) pushes bay's refs and main to it and
//! reads them from it ([`crate::git`] routes each call). The origin keeps the hosted remote
//! current in one of two modes, git config `bay.upstreamMode` on the origin:
//!
//! - `through`: the hosted remote is the authority for every ref. Before the origin takes a
//!   push, its hook sends each pushed ref to the hosted remote, all or none, by compare-and-swap
//!   on the value the push replaces. When the hosted remote refuses because a ref moved there,
//!   the origin takes the hosted remote's values and refuses the push as stale, so the client
//!   reads them and tries again. `bay host serve` mirrors into the origin what other writers
//!   push to the hosted remote directly.
//! - `tail`: the origin is the authority for bay's refs and notes. The hook sends only the
//!   other refs, main among them, so a landing is still a compare-and-swap on the hosted
//!   remote's main. `bay host serve` sends bay's refs to the hosted remote after the origin
//!   takes them, except the refs that say what is live on a machine now, and mirrors the hosted
//!   remote's branches into the origin.
//!
//! A fleet starts in `through` mode, which is safe while a peer that does not read `[origin]`
//! still writes to the hosted remote, and moves to `tail` when `bay host serve` has mirrored no
//! such write for a day. Once the origin tails, its tail state keeps it in `tail` mode whatever
//! its config says, until a person removes the state file: the hosted remote can lag the origin
//! then, and taking the hosted remote's values would undo writes.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::git::Git;
use crate::names;
use crate::result::{BayResult, GIT_CALL_TIMEOUT, GIT_NETWORK_TIMEOUT};

/// The hosted remote an origin keeps current: git config on the origin.
pub const UPSTREAM_KEY: &str = "bay.upstream";
/// `through` (the default) or `tail` ([`Mode`]).
pub const MODE_KEY: &str = "bay.upstreamMode";
/// How often `bay host serve` reads the hosted remote's branches, in seconds (default 5).
pub const POLL_KEY: &str = "bay.pollS";
/// How often `bay host serve` in `through` mode reads every ref it mirrors from the hosted
/// remote, in seconds (default 60). Each read lists every ref the hosted remote holds.
pub const SWEEP_KEY: &str = "bay.sweepS";
/// The configured hook that ends the in-flight mark of a forward the origin took.
pub const ORIGIN_HOOK: &str = "bay-origin";

/// The in-flight marks: one file per push whose refs are being forwarded, one `<ref> <new>`
/// line per ref. `bay host serve` and a refused push leave a marked ref to its push.
const FORWARDING: &str = "bay/forwarding";
/// What the origin last saw the hosted remote hold of each ref it tails: `<oid> <ref>` lines.
const TAIL_STATE: &str = "bay/tail-state";
/// A mark older than this is of a push that ended without its `post-receive` hook.
const MARK_TTL: Duration = Duration::from_secs(120);
/// The part of the hook's bound a forward leaves for what follows it.
const FORWARD_MARGIN: Duration = Duration::from_secs(2);
/// The most refs one tail push sends.
const TAIL_BATCH: usize = 200;
/// How long the tail leaves a ref the hosted remote refused for a reason other than a move.
const REFUSED_BACKOFF: Duration = Duration::from_secs(600);
/// The tail's tick.
const TICK: Duration = Duration::from_secs(1);
/// A fetch of more refs than this may bring a whole store, the first sweep's.
const LARGE_FETCH: usize = 100;
/// The bound of a large fetch.
const LARGE_FETCH_TIMEOUT: Duration = Duration::from_secs(7200);
/// The namespaces the origin owns in `tail` mode.
const OWNED: [&str; 2] = [names::BAY, names::NOTES];
/// Owned refs the origin keeps to itself: they say what is live on a machine now.
const LOCAL: [&str; 3] = [names::LEASE, names::LIVE, names::EXECUTOR];
/// Branches.
const HEADS: &str = "refs/heads/";

/// How an origin keeps its hosted remote current (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Through,
    Tail,
}

impl Mode {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Through => "through",
            Self::Tail => "tail",
        }
    }
}

/// An origin's upstream, read from its git config.
#[derive(Clone, Debug)]
pub struct Upstream {
    pub url: String,
    /// The mode the origin works in: its tail state keeps it in `tail` (module docs).
    pub mode: Mode,
    /// The mode its config names.
    pub configured: Mode,
}

/// The upstream of the origin at `git`, `dir` its git directory; `None` for a host with
/// no `bay.upstream`.
///
/// # Errors
///
/// The config cannot be read, or `bay.upstreamMode` names no mode: unknown is never a mode.
pub fn upstream(git: &Git, dir: &Path) -> Result<Option<Upstream>, String> {
    let Some(url) = git
        .config_get(UPSTREAM_KEY)
        .map_err(|error| error.message())?
        .map(|url| url.trim().to_owned())
        .filter(|url| !url.is_empty())
    else {
        return Ok(None);
    };
    let configured = match git
        .config_get(MODE_KEY)
        .map_err(|error| error.message())?
        .as_deref()
        .map(str::trim)
    {
        None | Some("through") => Mode::Through,
        Some("tail") => Mode::Tail,
        Some(other) => return Err(format!("{MODE_KEY} is {other:?}: it is through or tail")),
    };
    let mode = if dir.join(TAIL_STATE).exists() {
        Mode::Tail
    } else {
        configured
    };
    Ok(Some(Upstream {
        url,
        mode,
        configured,
    }))
}

/// Makes the new host at `host` an origin of `url` in `through` mode; `bay` is the binary its
/// hook runs.
///
/// # Errors
///
/// A config write failed.
pub fn configure(host: &Git, bay: &Path, url: &str) -> Result<(), String> {
    let post = format!("'{}' host hook post-receive", bay.display());
    for (key, value) in [
        (UPSTREAM_KEY.to_owned(), url),
        (MODE_KEY.to_owned(), Mode::Through.name()),
        (format!("hook.{ORIGIN_HOOK}.command"), post.as_str()),
        (format!("hook.{ORIGIN_HOOK}.event"), "post-receive"),
        // A client that reads with a filter (a partial clone) is served one.
        ("uploadpack.allowFilter".to_owned(), "true"),
        // In `tail` mode the origin is the only copy of a ref until the tail sends it.
        ("core.fsync".to_owned(), "committed"),
        // A fleet's pushes meet on one table list: wait for its lock, not 100 ms.
        ("reftable.lockTimeout".to_owned(), "5000"),
    ] {
        host.run(&["config", &key, value])
            .map_err(|error| error.message())?;
    }
    Ok(())
}

/// One ref update, as a hook reads it. An absent value is empty or zero.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Update {
    name: String,
    old: String,
    new: String,
}

fn absent(oid: &str) -> bool {
    oid.is_empty() || crate::hook::is_zero(oid)
}

/// The updates of a hook's input, `<old> <new> <ref>` lines.
fn updates(input: &str) -> Vec<Update> {
    input
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let old = words.next()?.to_owned();
            let new = words.next()?.to_owned();
            let name = words.next()?.to_owned();
            Some(Update { name, old, new })
        })
        .collect()
}

fn owned(name: &str) -> bool {
    OWNED.iter().any(|prefix| name.starts_with(prefix))
}

/// Whether the tail sends `name` to the hosted remote.
fn tailed(name: &str) -> bool {
    owned(name) && !LOCAL.iter().any(|prefix| name.starts_with(prefix))
}

/// The origin's part of a push's judgement, after bay's own checks pass (`pre-receive`): each
/// pushed ref the origin does not own in its mode goes to the hosted remote first, all or none,
/// by compare-and-swap on the value the push replaces. `within` is what is left of the hook's
/// bound.
///
/// # Errors
///
/// The refusal. When a ref moved on the hosted remote it says `stale info`, after the origin
/// took the hosted remote's values, so the client reads them and tries again.
pub fn forward(git: &Git, dir: &Path, input: &str, within: Duration) -> Result<(), String> {
    let Some(upstream) = upstream(git, dir)? else {
        return Ok(());
    };
    let sent: Vec<Update> = updates(input)
        .into_iter()
        .filter(|update| upstream.mode == Mode::Through || !owned(&update.name))
        .collect();
    if sent.is_empty() {
        return Ok(());
    }
    // The origin's own trunk hook judges after this one: a push it refuses never reaches the
    // hosted remote.
    for update in &sent {
        let judged = crate::host::judge_update(git, &update.name, &update.old, &update.new)
            .map_err(|error| {
                format!(
                    "the origin could not judge {}: {}",
                    update.name,
                    error.message()
                )
            })?;
        if let Some(why) = judged {
            return Err(why);
        }
    }
    let within = within.saturating_sub(FORWARD_MARGIN);
    if within < Duration::from_secs(1) {
        return Err("bay origin: the hook's bound left no time to forward the push".to_owned());
    }
    let mark = Mark::write(dir, &sent)?;
    let pushed = push(
        &git.reading_quarantine(),
        &upstream.url,
        &sent,
        true,
        within,
    );
    if pushed
        .as_ref()
        .is_ok_and(|pushed| pushed.rejected.is_empty())
    {
        // The mark stays until the origin takes the push (`post_receive`).
        return Ok(());
    }
    mark.remove();
    let pushed = pushed
        .map_err(|why| format!("bay origin: the hosted remote did not take the push: {why}"))?;
    let refusals = pushed.refusals();
    if !pushed
        .rejected
        .iter()
        .any(|(_, why)| crate::git::ref_moved(why))
    {
        return Err(format!(
            "bay origin: the hosted remote refused the push: {refusals}"
        ));
    }
    let names: BTreeSet<&str> = sent.iter().map(|update| update.name.as_str()).collect();
    let scope: Vec<&str> = names.iter().copied().collect();
    let took = match mirror(
        &git.outside_quarantine(),
        dir,
        &upstream.url,
        &scope,
        &|name| names.contains(name),
    ) {
        Ok(mirrored) => format!(
            "the origin took the hosted remote's value of {} ref(s)",
            mirrored.taken + mirrored.deleted
        ),
        Err(why) => format!("the origin could not take the hosted remote's values: {why}"),
    };
    Err(format!(
        "bay origin: moved on the hosted remote (stale info): {refusals}; {took}"
    ))
}

/// The origin's `post-receive` hook: ends the mark of each forward whose every ref the
/// origin now holds as the forward sent it.
pub fn post_receive(dir: &Path, input: &str) {
    let taken: HashSet<(String, String)> = updates(input)
        .into_iter()
        .map(|update| (update.name, update.new))
        .collect();
    let Ok(entries) = std::fs::read_dir(dir.join(FORWARDING)) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let all_taken = text
            .lines()
            .filter_map(|line| line.split_once(' '))
            .all(|(name, new)| taken.contains(&(name.to_owned(), new.to_owned())));
        if all_taken {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// A forward's in-flight mark (module [`FORWARDING`]).
struct Mark(PathBuf);

impl Mark {
    /// Writes the mark whole before the forward starts, so no reader sees part of it.
    fn write(dir: &Path, sent: &[Update]) -> Result<Self, String> {
        let at = dir.join(FORWARDING);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let path = at.join(format!("{}-{nanos}", std::process::id()));
        let writing = path.with_extension("new");
        let text = sent.iter().fold(String::new(), |mut text, update| {
            let _ = writeln!(text, "{} {}", update.name, update.new);
            text
        });
        std::fs::create_dir_all(&at)
            .and_then(|()| std::fs::write(&writing, text))
            .and_then(|()| crate::durable::rename(&writing, &path))
            .map_err(|error| format!("bay origin: cannot write the in-flight mark: {error}"))?;
        Ok(Self(path))
    }

    fn remove(self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The refs pushes are forwarding now. A mark past [`MARK_TTL`] is removed.
fn marks(dir: &Path) -> HashSet<String> {
    let mut held = HashSet::new();
    let Ok(entries) = std::fs::read_dir(dir.join(FORWARDING)) else {
        return held;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some() {
            continue;
        }
        let age = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok());
        if age.is_some_and(|age| age > MARK_TTL) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            held.extend(
                text.lines()
                    .filter_map(|line| line.split_once(' ').map(|(name, _)| name.to_owned())),
            );
        }
    }
    held
}

/// What the hosted remote said of a push.
#[derive(Debug, Default, PartialEq, Eq)]
struct Pushed {
    taken: Vec<String>,
    rejected: Vec<(String, String)>,
}

impl Pushed {
    /// `git push --porcelain`'s lines: `<flag>\t<from>:<to>\t<summary>`, `!` for a refused ref.
    fn read(out: &str) -> Self {
        let mut pushed = Self::default();
        for line in out.lines() {
            let mut fields = line.split('\t');
            let (Some(flag), Some(spec), Some(summary)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let name = spec.rsplit_once(':').map_or(spec, |(_, to)| to).to_owned();
            if flag == "!" {
                pushed.rejected.push((name, summary.to_owned()));
            } else {
                pushed.taken.push(name);
            }
        }
        pushed
    }

    fn refusals(&self) -> String {
        let mut shown: Vec<String> = self
            .rejected
            .iter()
            .take(5)
            .map(|(name, why)| format!("{name} {why}"))
            .collect();
        if self.rejected.len() > shown.len() {
            shown.push(format!("and {} more", self.rejected.len() - shown.len()));
        }
        shown.join(", ")
    }
}

/// Pushes each of `updates` to `url`: its `new` value (absent deletes) by compare-and-swap
/// on its `old` (absent: the ref must not exist there), all or none when `atomic`.
///
/// # Errors
///
/// The push failed with no ref refused: the hosted remote did not answer, or refused it whole.
fn push(
    git: &Git,
    url: &str,
    updates: &[Update],
    atomic: bool,
    within: Duration,
) -> Result<Pushed, String> {
    let mut args: Vec<String> = vec!["push".to_owned(), "--porcelain".to_owned()];
    if atomic {
        args.push("--atomic".to_owned());
    }
    for update in updates {
        let expect = if absent(&update.old) { "" } else { &update.old };
        args.push(format!("--force-with-lease={}:{expect}", update.name));
    }
    args.push(url.to_owned());
    for update in updates {
        args.push(if absent(&update.new) {
            format!(":{}", update.name)
        } else {
            format!("{}:{}", update.new, update.name)
        });
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, out, err) = git
        .run_status_for(&args, within)
        .map_err(|error| error.message())?;
    let pushed = Pushed::read(&out);
    if code != 0 && pushed.rejected.is_empty() {
        return Err(if err.is_empty() { out } else { err });
    }
    Ok(pushed)
}

/// The refs under `scope` (prefixes or names) the origin holds, by name.
fn held(git: &Git, scope: &[&str]) -> Result<BTreeMap<String, String>, String> {
    let mut args = vec!["for-each-ref", "--format=%(objectname) %(refname)"];
    args.extend(scope);
    let out = git.run(&args).map_err(|error| error.message())?;
    Ok(out
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(oid, name)| (name.to_owned(), oid.to_owned()))
        .collect())
}

/// The refs the hosted remote holds, by name: its branches alone when `heads`, else all of
/// them. A read of named refs lists them all too: Git sends no prefix for a pattern.
fn listing(git: &Git, url: &str, heads: bool) -> Result<BTreeMap<String, String>, String> {
    let mut args = vec!["ls-remote", "--refs"];
    if heads {
        args.push("--heads");
    }
    args.push(url);
    let out = git.run(&args).map_err(|error| error.message())?;
    Ok(out
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(oid, name)| (name.to_owned(), oid.to_owned()))
        .collect())
}

/// What a mirror pass did.
#[derive(Clone, Copy, Debug, Default)]
struct Mirrored {
    /// Refs set to the hosted remote's value.
    taken: usize,
    /// Refs removed because the hosted remote has none.
    deleted: usize,
    /// Of those, refs of bay's own: another writer wrote them to the hosted remote.
    strays: usize,
    /// Refs left as they were: a write moved them first.
    left: usize,
}

/// Takes into the origin the hosted remote's value of each ref under `scope` that `wanted`
/// selects, where the two differ, by compare-and-swap on the origin's value read before the
/// hosted remote is listed. A ref a push is forwarding now is left to that push: its own update
/// would fail if this one came first.
fn mirror(
    git: &Git,
    dir: &Path,
    url: &str,
    scope: &[&str],
    wanted: &dyn Fn(&str) -> bool,
) -> Result<Mirrored, String> {
    let before = held(git, scope)?;
    let heads = scope.iter().all(|prefix| prefix.starts_with(HEADS));
    let hosted: BTreeMap<String, String> = listing(git, url, heads)?
        .into_iter()
        .filter(|(name, _)| scope.iter().any(|prefix| name.starts_with(prefix)))
        .collect();
    // Read after the listing: a value the listing holds from a forward was marked before it.
    let busy = marks(dir);
    let names: BTreeSet<&String> = before.keys().chain(hosted.keys()).collect();
    let changes: Vec<(&String, Option<&String>, Option<&String>)> = names
        .into_iter()
        .filter(|name| wanted(name) && !busy.contains(*name))
        .map(|name| (name, hosted.get(name), before.get(name)))
        .filter(|(_, new, old)| new != old)
        .collect();
    let mut mirrored = Mirrored::default();
    if changes.is_empty() {
        return Ok(mirrored);
    }
    // The listing holds each value taken, so the fetch asks for those objects by id: it names no
    // ref and lists none. A fetch of names sends a prefix per name and matches each name against
    // every ref the remote sends back, so its cost grows with the square of the refs taken: a
    // first sweep of tens of thousands of refs matched names for its whole run and took no pack.
    let taking = changes.iter().filter(|(_, new, _)| new.is_some()).count();
    let wanted: BTreeSet<&str> = changes
        .iter()
        .filter_map(|(_, new, _)| new.map(String::as_str))
        .collect();
    if !wanted.is_empty() {
        let bound = if taking > LARGE_FETCH {
            LARGE_FETCH_TIMEOUT
        } else {
            GIT_NETWORK_TIMEOUT
        };
        let input = wanted.iter().fold(String::new(), |mut input, oid| {
            input.push_str(oid);
            input.push('\n');
            input
        });
        git.run_bytes_for(
            &[
                "fetch",
                "--stdin",
                "--no-tags",
                "--no-write-fetch-head",
                url,
            ],
            &[],
            Some(input.as_bytes()),
            bound,
        )
        .map_err(|error| error.message())?;
    }
    let mut input = String::new();
    for (name, new, old) in &changes {
        match (new, old) {
            (Some(new), Some(old)) => {
                let _ = writeln!(input, "update {name} {new} {old}");
            }
            (Some(new), None) => {
                let _ = writeln!(input, "create {name} {new}");
            }
            (None, Some(old)) => {
                let _ = writeln!(input, "delete {name} {old}");
            }
            (None, None) => {}
        }
    }
    // Each update stands alone: one that a write moved first is refused and named, and the
    // others are made.
    let out = git
        .run_bytes_for(
            &["update-ref", "--stdin", "--batch-updates"],
            &[],
            Some(input.as_bytes()),
            GIT_CALL_TIMEOUT,
        )
        .map_err(|error| error.message())?;
    let refused: HashSet<String> = String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|line| line.strip_prefix("rejected "))
        .filter_map(|rest| rest.split(' ').next().map(str::to_owned))
        .collect();
    for (name, new, _) in &changes {
        if refused.contains(*name) {
            mirrored.left += 1;
            continue;
        }
        if new.is_some() {
            mirrored.taken += 1;
        } else {
            mirrored.deleted += 1;
        }
        if owned(name) {
            mirrored.strays += 1;
        }
    }
    Ok(mirrored)
}

/// What the origin last saw the hosted remote hold of each tailed ref; `None` before a tail.
fn read_state(path: &Path) -> Option<BTreeMap<String, String>> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(
        text.lines()
            .filter_map(|line| line.split_once(' '))
            .map(|(oid, name)| (name.to_owned(), oid.to_owned()))
            .collect(),
    )
}

fn write_state(path: &Path, state: &BTreeMap<String, String>) -> Result<(), String> {
    let text = state.iter().fold(String::new(), |mut text, (name, oid)| {
        let _ = writeln!(text, "{oid} {name}");
        text
    });
    let writing = path.with_extension("new");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(&writing, text)
        .and_then(|()| crate::durable::rename(&writing, path))
        .map_err(|error| format!("bay origin: cannot write the tail state: {error}"))
}

/// What a tail pass did.
#[derive(Clone, Copy, Debug, Default)]
struct Tailed {
    /// Refs the hosted remote took.
    sent: usize,
    /// Refs another writer had moved on the hosted remote: the origin's value goes next pass.
    strays: usize,
    /// Refs the hosted remote refused for another reason, left for [`REFUSED_BACKOFF`].
    refused: usize,
    /// Refs whose value the hosted remote does not yet hold.
    pending: usize,
}

/// The tail of an origin in `tail` mode, kept across passes.
#[derive(Default)]
struct Tail {
    /// The origin's table list when a pass last left nothing to send.
    seen: Option<String>,
    refused: HashMap<String, Instant>,
}

impl Tail {
    /// Sends the hosted remote each tailed ref whose value it does not hold, by compare-and-swap
    /// on the value the origin last saw it hold. A pass whose table list is the one the last
    /// quiet pass saw has nothing to send.
    fn pass(&mut self, git: &Git, dir: &Path, url: &str) -> Result<Tailed, String> {
        let tables = std::fs::read_to_string(dir.join("reftable").join("tables.list")).ok();
        let path = dir.join(TAIL_STATE);
        let kept = read_state(&path);
        if kept.is_some() && tables.is_some() && tables == self.seen {
            return Ok(Tailed::default());
        }
        let mut state = match kept {
            Some(state) => state,
            None => listing(git, url, false)?
                .into_iter()
                .filter(|(name, _)| tailed(name))
                .collect(),
        };
        let origin: BTreeMap<String, String> = held(git, &OWNED)?
            .into_iter()
            .filter(|(name, _)| tailed(name))
            .collect();
        let now = Instant::now();
        self.refused
            .retain(|_, at| now.duration_since(*at) < REFUSED_BACKOFF);
        let due = differences(&origin, &state, &self.refused);
        let mut tailed = Tailed::default();
        let mut moved = Vec::new();
        let mut failed = None;
        for batch in due.chunks(TAIL_BATCH) {
            let pushed = match push(git, url, batch, false, GIT_NETWORK_TIMEOUT) {
                Ok(pushed) => pushed,
                Err(why) => {
                    failed = Some(why);
                    break;
                }
            };
            let sent: HashSet<&String> = pushed.taken.iter().collect();
            for update in batch.iter().filter(|update| sent.contains(&update.name)) {
                if absent(&update.new) {
                    state.remove(&update.name);
                } else {
                    state.insert(update.name.clone(), update.new.clone());
                }
                tailed.sent += 1;
            }
            for (name, why) in pushed.rejected {
                if crate::git::ref_moved(&why) {
                    moved.push(name);
                } else {
                    eprintln!("bay origin: the hosted remote refused {name}: {why}");
                    self.refused.insert(name, now);
                    tailed.refused += 1;
                }
            }
        }
        if failed.is_none() && !moved.is_empty() {
            match listing(git, url, false) {
                Ok(hosted) => {
                    for name in moved {
                        let there = hosted.get(&name);
                        eprintln!(
                            "bay origin: STRAY {name}: the hosted remote holds {}, the origin {}; the origin's value goes next",
                            there.map_or("nothing", String::as_str),
                            origin.get(&name).map_or("nothing", String::as_str),
                        );
                        match there {
                            Some(oid) => state.insert(name, oid.clone()),
                            None => state.remove(&name),
                        };
                        tailed.strays += 1;
                    }
                }
                Err(why) => failed = Some(why),
            }
        }
        write_state(&path, &state)?;
        if let Some(why) = failed {
            return Err(why);
        }
        tailed.pending = differences(&origin, &state, &self.refused).len();
        if tailed.pending == 0 {
            self.seen = tables;
        }
        Ok(tailed)
    }
}

/// The updates that bring `state` to `origin`, leaving the refs in `refused`.
fn differences(
    origin: &BTreeMap<String, String>,
    state: &BTreeMap<String, String>,
    refused: &HashMap<String, Instant>,
) -> Vec<Update> {
    let names: BTreeSet<&String> = origin.keys().chain(state.keys()).collect();
    names
        .into_iter()
        .filter(|name| !refused.contains_key(*name))
        .filter_map(|name| {
            let (new, old) = (origin.get(name), state.get(name));
            (new != old).then(|| Update {
                name: name.clone(),
                old: old.cloned().unwrap_or_default(),
                new: new.cloned().unwrap_or_default(),
            })
        })
        .collect()
}

/// `bay host serve <path>`: keeps the origin at `path` and its hosted remote current (module docs)
/// until stopped. `once` makes one whole pass and reports it.
#[must_use]
pub fn serve(path: &Path, once: bool) -> BayResult {
    run_serve(path, once).unwrap_or_else(BayResult::failed)
}

/// What one pass of `bay host serve` did.
#[derive(Default)]
struct Pass {
    mirrored: Option<Mirrored>,
    tailed: Option<Tailed>,
}

impl Pass {
    fn quiet(&self) -> bool {
        self.mirrored
            .is_none_or(|m| m.taken + m.deleted + m.left == 0)
            && self
                .tailed
                .is_none_or(|t| t.sent + t.strays + t.refused == 0)
    }

    fn detail(&self, path: &Path, upstream: &Upstream) -> Value {
        json!({
            "origin": path.display().to_string(),
            "upstream": shown(&upstream.url),
            "mode": upstream.mode.name(),
            "configured": upstream.configured.name(),
            "mirrored": self.mirrored.map(|m| json!({
                "taken": m.taken,
                "deleted": m.deleted,
                "strays": m.strays,
                "left": m.left,
            })),
            "tailed": self.tailed.map(|t| json!({
                "sent": t.sent,
                "strays": t.strays,
                "refused": t.refused,
                "pending": t.pending,
            })),
        })
    }
}

/// A URL as bay shows it: without the credentials it may carry.
#[must_use]
pub fn shown(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('/') {
            Some((authority, path)) => match authority.rsplit_once('@') {
                Some((_, host)) => format!("{scheme}://{host}/{path}"),
                None => url.to_owned(),
            },
            None => url.to_owned(),
        },
        None => url.to_owned(),
    }
}

fn seconds(git: &Git, key: &str, default: u64) -> Duration {
    Duration::from_secs(
        git.config_get(key)
            .ok()
            .flatten()
            .and_then(|text| text.trim().parse().ok())
            .filter(|&seconds| seconds > 0)
            .unwrap_or(default),
    )
}

/// When each kind of pass is next due.
#[derive(Default)]
struct Due {
    polled: Option<Instant>,
    swept: Option<Instant>,
}

fn run_serve(path: &Path, once: bool) -> Result<BayResult, String> {
    let git = Git::new(path);
    let mut tail = Tail::default();
    let mut due = Due::default();
    // G892: a serving origin moves to a new bay between passes, as a runner does.
    let mut binary = crate::executor::OwnBinary::now("origin");
    loop {
        let upstream = upstream(&git, path)?.ok_or_else(|| {
            format!(
                "{} has no {UPSTREAM_KEY}: it is a host, not an origin",
                path.display()
            )
        })?;
        let mut pass = Pass::default();
        let round = serve_round(&git, path, &upstream, once, &mut tail, &mut due, &mut pass);
        if once {
            round?;
            return Ok(BayResult::ok(pass.detail(path, &upstream)));
        }
        match round {
            Ok(()) if pass.quiet() => {}
            Ok(()) => eprintln!("bay origin: {}", pass.detail(path, &upstream)),
            Err(why) => eprintln!("bay origin: {why}"),
        }
        binary.between_passes();
        std::thread::sleep(TICK);
    }
}

/// One tick of `bay host serve`: the tail in `tail` mode, then whichever mirror is due. A
/// pass is due again a whole interval after it started, whether it worked or not.
fn serve_round(
    git: &Git,
    path: &Path,
    upstream: &Upstream,
    once: bool,
    tail: &mut Tail,
    due: &mut Due,
    pass: &mut Pass,
) -> Result<(), String> {
    if upstream.mode == Mode::Tail {
        pass.tailed = Some(tail.pass(git, path, &upstream.url)?);
    }
    let now = Instant::now();
    let elapsed = |at: Option<Instant>, every: Duration| {
        once || at.is_none_or(|at| now.duration_since(at) >= every)
    };
    if upstream.mode == Mode::Through && elapsed(due.swept, seconds(git, SWEEP_KEY, 60)) {
        due.swept = Some(now);
        due.polled = Some(now);
        pass.mirrored = Some(mirror(
            git,
            path,
            &upstream.url,
            &[HEADS, names::BAY, names::NOTES],
            &|_| true,
        )?);
    } else if elapsed(due.polled, seconds(git, POLL_KEY, 5)) {
        due.polled = Some(now);
        pass.mirrored = Some(mirror(git, path, &upstream.url, &[HEADS], &|_| true)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_push_reads_as_the_refs_it_took_and_refused() {
        let out = "To /x/hosted.git\n*\tabc:refs/bay/changes/a\t[new reference]\n \tdef:refs/heads/main\t1..2\n-\t:refs/bay/live/m\t[deleted]\n!\tabc:refs/bay/queue/q\t[rejected] (stale info)\nDone\n";
        let pushed = Pushed::read(out);
        assert_eq!(
            pushed.taken,
            ["refs/bay/changes/a", "refs/heads/main", "refs/bay/live/m"]
        );
        assert_eq!(
            pushed.rejected,
            [(
                "refs/bay/queue/q".to_owned(),
                "[rejected] (stale info)".to_owned()
            )]
        );
        assert!(crate::git::ref_moved(&pushed.refusals()));
    }

    #[test]
    fn the_tail_sends_bays_refs_but_not_what_is_live_on_a_machine() {
        assert!(tailed("refs/bay/changes/a"));
        assert!(tailed("refs/notes/bay/evidence/x"));
        assert!(!tailed("refs/bay/live/m"));
        assert!(!tailed("refs/bay/lease/l"));
        assert!(!tailed("refs/bay/executor/e"));
        assert!(!tailed("refs/heads/main"));
    }

    #[test]
    fn a_shown_url_carries_no_credentials() {
        assert_eq!(
            shown("https://user:secret@example.invalid/o/r.git"),
            "https://example.invalid/o/r.git"
        );
        assert_eq!(shown("ssh://h/o/r.git"), "ssh://h/o/r.git");
        assert_eq!(shown("/srv/r.git"), "/srv/r.git");
    }

    #[test]
    fn a_mark_holds_its_refs_until_the_origin_takes_them() {
        let dir = tempfile::TempDir::new().unwrap();
        let sent = updates("0000 aaaa refs/bay/changes/a\nbbbb 0000 refs/heads/main\n");
        let mark = Mark::write(dir.path(), &sent).unwrap();
        let busy = marks(dir.path());
        assert!(busy.contains("refs/bay/changes/a") && busy.contains("refs/heads/main"));
        post_receive(dir.path(), "0000 aaaa refs/bay/changes/a\n");
        assert_eq!(marks(dir.path()).len(), 2, "one ref is not taken yet");
        post_receive(
            dir.path(),
            "0000 aaaa refs/bay/changes/a\nbbbb 0000 refs/heads/main\n",
        );
        assert!(marks(dir.path()).is_empty());
        mark.remove();
    }

    #[test]
    fn the_tail_state_reads_as_it_was_written() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(TAIL_STATE);
        assert!(read_state(&path).is_none());
        let state: BTreeMap<String, String> = [
            ("refs/bay/changes/a".to_owned(), "aaaa".to_owned()),
            ("refs/notes/bay/evidence/x".to_owned(), "bbbb".to_owned()),
        ]
        .into();
        write_state(&path, &state).unwrap();
        assert_eq!(read_state(&path), Some(state.clone()));
        let origin: BTreeMap<String, String> = [
            ("refs/bay/changes/a".to_owned(), "cccc".to_owned()),
            ("refs/bay/changes/b".to_owned(), "dddd".to_owned()),
        ]
        .into();
        let due = differences(&origin, &state, &HashMap::new());
        let named: Vec<(&str, &str, &str)> = due
            .iter()
            .map(|u| (u.name.as_str(), u.old.as_str(), u.new.as_str()))
            .collect();
        assert_eq!(
            named,
            [
                ("refs/bay/changes/a", "aaaa", "cccc"),
                ("refs/bay/changes/b", "", "dddd"),
                ("refs/notes/bay/evidence/x", "bbbb", ""),
            ]
        );
    }
}
