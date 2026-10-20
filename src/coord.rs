//! The coordination cache `pre-tool` judges a write against (`DESIGN` § 11, § 11a's
//! `PreToolUse` row; `INTERFACES` § 7 `[hooks]`; rulings R4, R5).
//!
//! `<git-common-dir>/bay/coord.json` is written by a bay command that is not a hook, after it
//! fetched trunk (`start`, `start --resume`, `land`, `status`, `undo`, and `executor run` after
//! each pass): the time of that fetch, the trunk it brought, every local change, and for each
//! live one each named input with its state at that trunk and the paths a write to which it
//! covers. The hook only reads it, with bounded cost: one file read, no fetch and no provider
//! run; only when the cache is unreadable or does not list the change does it read that
//! change's record (one local Git read) to learn whether any input could cover the write.
//!
//! What a write is judged by (ruling R4): an input covers the non-historical consumer paths its
//! provider listed, or the change's declared paths when it listed none, or the whole tree when
//! the change declares none. Claims never refuse a write. A write the cache shows covered by an
//! input trunk requires a version of that the change has not observed is refused with
//! `INPUT_UNOBSERVED`, and the repair is `bay start --resume <change>`. An observed input with
//! no validated candidate yet (`INPUT_CHANGED`) refuses no write: the repair is an edit, and
//! refusing it would deadlock (G8); land still refuses the unvalidated candidate. A cache older
//! than `[hooks] cache_max_age_s`, or one that cannot say, gives a write to a declared input a
//! `COVERAGE_UNKNOWN` notice and allows it (ruling R5): never a silent allow, never a guess.
//! So does a config bay cannot read, and an edit tool whose payload names no `file_path` while
//! a stale or unanswered input could cover it.
//! This cache is not the shared cache (`NAMES`), and bay's own operations still check
//! everything they depend on.

use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::change::{CHANGES_PREFIX, Chain, ChangeState};
use crate::inputs::{ConsumerKind, InputState, Judgement, Probe};
use crate::repo::Repo;
use crate::result::Reason;

pub const SCHEMA: &str = "bay.coord/1";

/// The most of the cache a hook reads: a cache larger than this says nothing it can trust.
const MAX_BYTES: u64 = 4 << 20;

/// The deepest path a hook resolves: a write target is never walked further than this.
const MAX_DEPTH: usize = 256;

/// The cache file.
#[must_use]
pub fn path(repo: &Repo) -> PathBuf {
    repo.common_dir.join("bay").join("coord.json")
}

/// What `pre-tool` needs to judge a write, as of one fetch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coord {
    pub schema: String,
    /// When trunk was fetched, in Unix seconds, and the same time for a reader.
    pub fetched_at_s: u64,
    pub fetched_at: String,
    /// The trunk that fetch brought.
    pub trunk: String,
    pub changes: Vec<CoordChange>,
}

/// One live local change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordChange {
    pub change: String,
    pub inputs: Vec<CoordInput>,
}

/// One named input of a change, judged at the cache's trunk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordInput {
    pub input: String,
    pub state: InputState,
    pub observed: Option<String>,
    pub required: Option<String>,
    /// The paths a write to which this input covers; `""` is the whole tree.
    pub covers: Vec<String>,
}

/// The repository and time of this process's last trunk fetch.
static FETCHED: Mutex<Option<(PathBuf, SystemTime)>> = Mutex::new(None);

/// Records that this process fetched trunk into the repository whose main work tree is
/// `work_root` (`Repo::fetch_trunk`).
pub fn fetched(work_root: &Path) {
    if let Ok(mut last) = FETCHED.lock() {
        *last = Some((work_root.to_path_buf(), SystemTime::now()));
    }
}

/// Writes the cache when this command fetched trunk. A command that did not fetch leaves the
/// cache as it is, so its age stays the age of the last fetch. Best effort: a cache that cannot
/// be written stays old, and the hook says so.
pub fn after_command() {
    let last = FETCHED.lock().ok().and_then(|mut last| last.take());
    let Some((work_root, at)) = last else {
        return;
    };
    if let Ok(repo) = Repo::open(&work_root) {
        let _ = write(&repo, at);
    }
}

/// Writes the cache for the local trunk as fetched at `at`.
///
/// # Errors
///
/// The local cache holds no trunk, a git fault, or the file cannot be written.
pub fn write(repo: &Repo, at: SystemTime) -> Result<(), String> {
    let trunk = repo
        .git
        .run(&["rev-parse", "--verify", &repo.trunk_tracking_ref()])
        .map_err(|error| error.message())?;
    let chain = Chain::new(&repo.git);
    let ids = repo
        .git
        .run(&["for-each-ref", "--format=%(refname)", CHANGES_PREFIX])
        .map_err(|error| error.message())?;
    let mut changes = Vec::new();
    for id in ids
        .lines()
        .map(|name| name.trim_start_matches(CHANGES_PREFIX))
    {
        let Ok((_, record)) = chain.read(id) else {
            continue;
        };
        // A change that is not live, or has landed, is listed with no input: its writes are
        // known to be uncovered, never a gap.
        if !live(record.state)
            || !crate::publish::scan(&repo.git, &record.base_first, &trunk, id)?
                .publications
                .is_empty()
        {
            changes.push(CoordChange {
                change: id.to_owned(),
                inputs: Vec::new(),
            });
            continue;
        }
        let declared = crate::inputs::paths(&record.declared_inputs);
        // A view the user asked for may run a provider on a miss: this is not a hook.
        let inputs = match crate::inputs::judge(repo, &record, &trunk, Probe::Run) {
            Ok(judged) => judged
                .iter()
                .map(|judgement| input_of(judgement, &declared))
                .collect(),
            Err(_) => crate::inputs::names(&record.declared_inputs)
                .into_iter()
                .map(|name| CoordInput {
                    input: name.to_string(),
                    state: InputState::Unknown,
                    observed: record.observed_versions.get(name.as_str()).cloned(),
                    required: None,
                    covers: whole_or(&declared),
                })
                .collect(),
        };
        changes.push(CoordChange {
            change: id.to_owned(),
            inputs,
        });
    }
    let fetched_at_s = at
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let coord = Coord {
        schema: SCHEMA.to_owned(),
        fetched_at_s,
        fetched_at: crate::clock::rfc3339_of(fetched_at_s),
        trunk,
        changes,
    };
    let text = serde_json::to_string(&coord).map_err(|error| error.to_string())?;
    let file = path(repo);
    let dir = file.parent().ok_or("the cache has no directory")?;
    std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    // A whole file or none: a hook never reads half a cache.
    let partial = dir.join(format!("coord.json.{}.tmp", std::process::id()));
    std::fs::write(&partial, text).map_err(|error| format!("{}: {error}", partial.display()))?;
    crate::durable::rename(&partial, &file).map_err(|error| format!("{}: {error}", file.display()))
}

/// The declared paths, or the whole tree when the change declares none.
fn whole_or(declared: &[String]) -> Vec<String> {
    if declared.is_empty() {
        vec![String::new()]
    } else {
        declared.to_vec()
    }
}

/// Ruling R4: the provider's non-historical consumer paths when it lists consumers, else the
/// change's declared paths. A historical consumer never blocks work, so it covers nothing.
fn input_of(judgement: &Judgement, declared: &[String]) -> CoordInput {
    let covers = if judgement.consumers.is_empty() {
        whole_or(declared)
    } else {
        judgement
            .consumers
            .iter()
            .filter(|consumer| consumer.kind != ConsumerKind::Historical)
            .map(|consumer| consumer.path.clone())
            .collect()
    };
    CoordInput {
        input: judgement.input.to_string(),
        state: judgement.state,
        observed: judgement.observed.clone(),
        required: judgement.required.clone(),
        covers,
    }
}

/// What `pre-tool` answers for one write.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Silent (§ 11a: "otherwise silent").
    Allow,
    /// Exit 2 with this one line (`INPUT_UNOBSERVED`).
    Refuse {
        reason: Reason,
        line: String,
        detail: Value,
    },
    /// Allowed, with this `additionalContext` (`COVERAGE_UNKNOWN`, ruling R5).
    Notice { text: String, detail: Value },
}

/// Judges an edit tool's write at `now`, from the cache only. `file` is the tool's target
/// (absolute, or relative to `cwd`), or `None` when its payload names none. The repository and
/// view that hold the target judge it, wherever the session runs (ruling R-B3-3).
#[must_use]
pub fn judge(cwd: &Path, file: Option<&Path>, now: SystemTime) -> Verdict {
    let target = file.and_then(|file| resolve(&cwd.join(file)));
    let anchor = match &target {
        Some(target) => target
            .ancestors()
            .find(|dir| dir.is_dir())
            .map_or_else(|| cwd.to_path_buf(), Path::to_path_buf),
        None => cwd.to_path_buf(),
    };
    let repo = match Repo::open_known(&anchor) {
        Ok(repo) => repo,
        // No repository holds the target, so no view does.
        Err((None, _)) => return Verdict::Allow,
        Err((Some(common), why)) => return unopened(&common, target.as_deref(), &why),
    };
    let views = repo.views_dir();
    let Some(target) = target else {
        let id = resolve(cwd)
            .and_then(|cwd| locate(&views, &cwd))
            .map(|(id, _)| id);
        return judge_in(&repo, id.as_deref(), None, now);
    };
    match locate(&views, &target) {
        Some((id, path)) if !path.is_empty() => judge_in(&repo, Some(&id), Some(&path), now),
        _ => Verdict::Allow,
    }
}

/// A repository bay cannot open (its config does not parse): a write into one of its views, or
/// one whose target is unknown, cannot be judged, so it is a notice.
fn unopened(common: &Path, target: Option<&Path>, why: &str) -> Verdict {
    let why = format!("bay cannot open this repository ({why})");
    match target {
        Some(target) => match locate(&common.join("bay").join("views"), target) {
            Some((id, path)) if !path.is_empty() => {
                unknown(Some(&id), Some(&path), &why, None, None)
            }
            _ => Verdict::Allow,
        },
        None => unknown(None, None, &why, None, None),
    }
}

/// Judges a write in change `id`'s view (any change's, when `id` is `None`) of `path` (an
/// unknown path when `None`). A change the cache cannot speak for is looked up in its record,
/// so a change with no live input stays silent.
fn judge_in(repo: &Repo, id: Option<&str>, path: Option<&str>, now: SystemTime) -> Verdict {
    let coord = match read(repo) {
        Ok(coord) => coord,
        Err(why) if id.is_none_or(|id| may_cover(repo, id)) => {
            return unknown(id, path, &why, None, None);
        }
        Err(_) => return Verdict::Allow,
    };
    if let Some(id) = id
        && !coord.changes.iter().any(|change| change.change == id)
        && !may_cover(repo, id)
    {
        return Verdict::Allow;
    }
    judge_cached(&coord, id, path, repo.config.cache_max_age(), now)
}

/// Whether change `id` is live and declares a named input, from its record (one local read): an
/// unreadable record may.
fn may_cover(repo: &Repo, id: &str) -> bool {
    match Chain::new(&repo.git).read(id) {
        Ok((_, record)) => {
            live(record.state) && !crate::inputs::names(&record.declared_inputs).is_empty()
        }
        Err(_) => true,
    }
}

/// A change whose inputs can still refuse a write.
fn live(state: ChangeState) -> bool {
    matches!(
        state,
        ChangeState::Open | ChangeState::Invalidated | ChangeState::Repairing | ChangeState::Ready
    )
}

/// The cache's age at `now` in seconds, or why it has none.
fn age_of(coord: &Coord, now: SystemTime) -> Result<u64, String> {
    let fetched = UNIX_EPOCH
        .checked_add(Duration::from_secs(coord.fetched_at_s))
        .ok_or_else(|| {
            format!(
                "the coordination cache's fetch time ({} s) is out of range",
                coord.fetched_at_s
            )
        })?;
    now.duration_since(fetched)
        .map(|age| age.as_secs())
        .map_err(|_| {
            format!(
                "the coordination cache says it was fetched in the future ({})",
                coord.fetched_at
            )
        })
}

/// Judges a write from a cache that was read: change `id` (any change when `None`), `path` (an
/// unknown path when `None`), the cache at most `max` old.
fn judge_cached(
    coord: &Coord,
    id: Option<&str>,
    path: Option<&str>,
    max: Duration,
    now: SystemTime,
) -> Verdict {
    let age = age_of(coord, now);
    let Some(id) = id else {
        return coord
            .changes
            .iter()
            .map(|change| judge_change(change, None, &age, max))
            .find(|verdict| *verdict != Verdict::Allow)
            .unwrap_or(Verdict::Allow);
    };
    if let Some(change) = coord.changes.iter().find(|change| change.change == id) {
        return judge_change(change, path, &age, max);
    }
    let why = format!(
        "the coordination cache (fetched {}) does not hold this change",
        coord.fetched_at
    );
    unknown(Some(id), path, &why, age.ok(), None)
}

/// One change's answer: an unknown `path` is covered by every input that covers any path.
fn judge_change(
    change: &CoordChange,
    path: Option<&str>,
    age: &Result<u64, String>,
    max: Duration,
) -> Verdict {
    let id = Some(change.change.as_str());
    let covering: Vec<&CoordInput> = change
        .inputs
        .iter()
        .filter(|input| match path {
            Some(path) => input.covers.iter().any(|outer| covers(outer, path)),
            None => !input.covers.is_empty(),
        })
        .collect();
    let Some(first) = covering.first() else {
        return Verdict::Allow;
    };
    let age = match age {
        Ok(age) if *age <= max.as_secs() => *age,
        Ok(age) => {
            let why = format!(
                "the coordination cache is {age} s old ([hooks] cache_max_age_s = {}), so it \
                 cannot say whether {} is stale",
                max.as_secs(),
                first.input
            );
            return unknown(id, path, &why, Some(*age), Some(&first.input));
        }
        Err(why) => return unknown(id, path, why, None, Some(&first.input)),
    };
    if let Some(stale) = covering
        .iter()
        .find(|input| input.state == InputState::Unobserved)
    {
        if let Some(path) = path {
            return refusal(&change.change, path, stale);
        }
        let why = format!(
            "the tool names no file_path, so bay cannot say whether {}, which trunk moved past \
             what this change observed, covers its write",
            stale.input
        );
        return unknown(id, None, &why, Some(age), Some(&stale.input));
    }
    if let Some(gap) = covering
        .iter()
        .find(|input| input.state == InputState::Unknown)
    {
        let why = format!(
            "no provider answer for {} was known at the last fetch",
            gap.input
        );
        return unknown(id, path, &why, Some(age), Some(&gap.input));
    }
    Verdict::Allow
}

/// The `COVERAGE_UNKNOWN` notice: the write is allowed, and the agent is told why bay could not
/// judge it (ruling R5).
fn unknown(
    id: Option<&str>,
    path: Option<&str>,
    why: &str,
    age: Option<u64>,
    input: Option<&str>,
) -> Verdict {
    let what = path.map_or_else(|| "an edit".to_owned(), str::to_owned);
    let place = id.map_or_else(String::new, |id| format!(" in change {id}"));
    Verdict::Notice {
        text: format!(
            "bay: COVERAGE_UNKNOWN for {what}{place}: {why}; the write is allowed unchecked, and \
             `bay status` refreshes the cache"
        ),
        detail: json!({
            "reason": Reason::CoverageUnknown.code(),
            "change": id,
            "path": path,
            "input": input,
            "cache_age_s": age,
            "why": why,
        }),
    }
}

/// The `INPUT_UNOBSERVED` refusal of a write of `path` in change `id` that `stale` covers.
fn refusal(id: &str, path: &str, stale: &CoordInput) -> Verdict {
    let version = |version: &Option<String>| version.as_deref().unwrap_or("none").to_owned();
    let line = format!(
        "bay: refused INPUT_UNOBSERVED: {path} is covered by {} (observed {}, required {}); \
         repair: bay start --resume {id}",
        stale.input,
        version(&stale.observed),
        version(&stale.required),
    );
    Verdict::Refuse {
        reason: Reason::InputUnobserved,
        line,
        detail: json!({
            "reason": Reason::InputUnobserved.code(),
            "change": id,
            "path": path,
            "input": stale.input,
            "observed": stale.observed,
            "required": stale.required,
        }),
    }
}

/// Whether a cover path holds `path` (`""` holds every path).
fn covers(outer: &str, path: &str) -> bool {
    outer.is_empty() || crate::inputs::within(outer, path)
}

/// Reads the cache, at most [`MAX_BYTES`] of it.
fn read(repo: &Repo) -> Result<Coord, String> {
    let file = path(repo);
    let opened = std::fs::File::open(&file)
        .map_err(|error| format!("no readable coordination cache ({error})"))?;
    let mut text = String::new();
    opened
        .take(MAX_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|error| format!("the coordination cache cannot be read ({error})"))?;
    parse(&text)
}

/// A cache's text, which says nothing when it is larger than [`MAX_BYTES`], does not parse, or
/// is another schema.
fn parse(text: &str) -> Result<Coord, String> {
    if text.len() as u64 > MAX_BYTES {
        return Err(format!(
            "the coordination cache is larger than {MAX_BYTES} bytes"
        ));
    }
    let coord: Coord = serde_json::from_str(text)
        .map_err(|error| format!("the coordination cache does not parse ({error})"))?;
    if coord.schema != SCHEMA {
        return Err(format!(
            "the coordination cache is {}, not {SCHEMA}",
            coord.schema
        ));
    }
    Ok(coord)
}

/// `file` as the system resolves it, though it need not exist: its longest existing prefix is
/// canonicalized (so a symbolic link, and `..` after one, resolve as the system resolves them),
/// and the rest, which holds no link, is folded. `None` past [`MAX_DEPTH`] components.
pub(crate) fn resolve(file: &Path) -> Option<PathBuf> {
    let parts: Vec<Component> = file.components().collect();
    if parts.len() > MAX_DEPTH {
        return None;
    }
    for split in (1..=parts.len()).rev() {
        let prefix: PathBuf = parts[..split].iter().collect();
        let Ok(mut resolved) = prefix.canonicalize() else {
            continue;
        };
        for part in &parts[split..] {
            match part {
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::CurDir => {}
                other => resolved.push(other),
            }
        }
        return Some(resolved);
    }
    None
}

/// The change whose view under `views` holds the resolved path `file`, and `file`'s path in that
/// view (`/`-separated, empty for the view itself).
fn locate(views: &Path, file: &Path) -> Option<(String, String)> {
    let views = views.canonicalize().ok()?;
    let inner = file.strip_prefix(&views).ok()?;
    let mut parts = inner.components();
    let id = parts.next()?.as_os_str().to_str()?.to_owned();
    if !crate::ids::is_change_id(&id) {
        return None;
    }
    let path: Vec<&str> = parts
        .map(|part| part.as_os_str().to_str())
        .collect::<Option<_>>()?;
    Some((id, path.join("/")))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::{Coord, CoordChange, CoordInput, InputState, Verdict, covers, judge_cached, parse};

    #[test]
    fn an_empty_cover_is_the_whole_tree_and_a_directory_holds_its_files() {
        assert!(covers("", "anything/at/all"));
        assert!(covers("src/", "src/a.rs"));
        assert!(covers("src", "src/a.rs"));
        assert!(covers("src/a.rs", "src/a.rs"));
        assert!(!covers("src/", "srcs/a.rs"));
        assert!(!covers("src/", "notes/a.md"));
    }

    fn cache(fetched_at_s: u64, state: InputState) -> Coord {
        Coord {
            schema: super::SCHEMA.to_owned(),
            fetched_at_s,
            fetched_at: String::new(),
            trunk: String::new(),
            changes: vec![CoordChange {
                change: "c1".to_owned(),
                inputs: vec![CoordInput {
                    input: "contract:api".to_owned(),
                    state,
                    observed: None,
                    required: None,
                    covers: vec!["src/".to_owned()],
                }],
            }],
        }
    }

    /// The notice's reason and age, or `None` for another verdict.
    fn notice(verdict: &Verdict) -> Option<(String, Option<u64>)> {
        match verdict {
            Verdict::Notice { detail, .. } => Some((
                detail["reason"].as_str()?.to_owned(),
                detail["cache_age_s"].as_u64(),
            )),
            _ => None,
        }
    }

    #[test]
    fn a_cache_that_cannot_say_is_a_notice_never_an_allow() {
        let now = UNIX_EPOCH + Duration::from_secs(1000);
        let max = Duration::from_secs(30);
        let judge = |coord: &Coord, id, path| judge_cached(coord, Some(id), path, max, now);
        let unknown = Some(("COVERAGE_UNKNOWN".to_owned(), Some(10)));
        // A fresh cache with no provider answer for the covering input.
        let gap = cache(990, InputState::Unknown);
        assert_eq!(notice(&judge(&gap, "c1", Some("src/a.rs"))), unknown);
        // A fetch time in the future, or past what a clock holds.
        for at in [2000, u64::MAX] {
            let ahead = cache(at, InputState::Clear);
            let verdict = judge(&ahead, "c1", Some("src/a.rs"));
            assert_eq!(
                notice(&verdict).map(|(reason, _)| reason).as_deref(),
                Some("COVERAGE_UNKNOWN")
            );
        }
        // A cache that does not hold the change says how old it is.
        let fresh = cache(990, InputState::Clear);
        assert_eq!(notice(&judge(&fresh, "c2", Some("src/a.rs"))), unknown);
        // A fresh answer covers; a path no input covers is silent.
        assert_eq!(judge(&fresh, "c1", Some("src/a.rs")), Verdict::Allow);
        let stale = cache(990, InputState::Unobserved);
        assert_eq!(judge(&stale, "c1", Some("docs/a.md")), Verdict::Allow);
        assert!(matches!(
            judge(&stale, "c1", Some("src/a.rs")),
            Verdict::Refuse { .. }
        ));
        // An unknown target under a stale input is a notice, in its change or in any.
        assert_eq!(notice(&judge(&stale, "c1", None)), unknown);
        assert_eq!(notice(&judge_cached(&stale, None, None, max, now)), unknown);
        assert_eq!(judge_cached(&fresh, None, None, max, now), Verdict::Allow);
    }

    #[test]
    fn a_cache_too_large_unparsed_or_of_another_schema_says_nothing() {
        let good = serde_json::to_string(&cache(1, InputState::Clear)).unwrap();
        assert!(parse(&good).is_ok());
        let large = format!(
            "{good}{}",
            " ".repeat(usize::try_from(super::MAX_BYTES).unwrap())
        );
        assert!(parse(&large).unwrap_err().contains("larger"));
        assert!(parse("{").unwrap_err().contains("does not parse"));
        let other = good.replace(super::SCHEMA, "bay.coord/2");
        assert!(parse(&other).unwrap_err().contains("bay.coord/2"));
    }
}
