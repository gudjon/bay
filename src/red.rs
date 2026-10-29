//! G244: a red trunk is owned until it is fixed, undone or quarantined (the pilot-gaps analysis
//! of 2026-10-20).
//!
//! A post-publication check (`stage = "post"`, G91) judges a landing after it is on trunk. When
//! the newest answer of such a check on trunk is a `product_failure`, trunk is **red** on that
//! check's paths (its `when_paths`, or every path), and the landing that broke it owns it. It is
//! derived from the records on each read, never stored:
//!
//! - Over the newest `[red] window` first-parent commits (`.bay/config.toml` at trunk, default
//!   [`DEFAULT_WINDOW`]), the newest commit with an answered record of the check decides. A
//!   pass there, or above it, is green. N-20: the window is the landing line of those commits
//!   ([`crate::trunkline`]): a bay landing an outside merge pushed off the first-parent line is
//!   judged at that merge, so its suspect range and culprit still name it.
//! - NEW-6: a check no commit in the window answers is never green. When the window is full,
//!   its answer, if any, is older than the window: it is `pending_beyond_window`. When the window
//!   holds all of trunk and a landing in it requires the check, it is `pending`. Either is listed
//!   as [`Pending`], apart from the red checks: unknown, not a failure, so it refuses no landing.
//! - A red check names its `suspect` range ([`Suspect`]): the newest pass older than the failure
//!   (when the window holds one), the first failure after it, and the landings in between.
//! - Culprit finding: when the newest pass sits right below the first failure, one landing is
//!   left in the range, and the evidence names it. That landing is the `culprit`, with the
//!   answers `tested` from the pass up to the newest failure, and it owns the red in place of
//!   the newest failure. A post producer, or an executor with `[runner] bisect_post`
//!   (`crate::bisect`), narrows the range by recording runs on its midpoints.
//! - A `product_failure` is cleared by a later landing whose `Fixes:`, `Reverts:` or
//!   `Supersedes:` names the owning change, or by a quarantine of the check at trunk.
//!
//! While red, the executor refuses a candidate that touches those paths with `MAIN_RED`,
//! naming the owner and the ways out; a change that names the owning change in `--fixes`, an
//! undo of it, or a superseding change is never refused.
//!
//! G195: `bay status --since` judges from what its cursor carries ([`Judged`]): how each check
//! was decided, at which trunk, under which evidence tips and registry. It reads the answers of
//! the landings after that trunk and of those whose note moved (one `diff-tree` per moved
//! producer ref), so its Git work grows with what changed, not with the window. Two reads are
//! bounded by the window instead: while a check is red, its suspect range reads back to its
//! last pass; and while no landing answers a check in a window that holds all of trunk, the
//! landings are read until one requires it.
//!
//! BACKLOG 119: a change the red holds waits in its place in line, and no runner claims it
//! while the red stands. The line passes such places in arrival order ([`holds_place`]), and
//! once main is green the change runs before every change submitted after it.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::git::Git;
use crate::registry::Registry;

/// How many first-parent trunk commits a red judgement reads back unless `[red] window` says.
pub const DEFAULT_WINDOW: usize = 50;

/// The [`Pending`] state of a check whose answer, if any, is older than the window.
pub const PENDING_BEYOND_WINDOW: &str = "pending_beyond_window";

/// The [`Pending`] state of a check no landing answered yet, the window holding all of trunk.
pub const PENDING: &str = "pending";

/// How many first-parent commits of `trunk` a red judgement and the unattributed scan read
/// back: `[red] window` in the config committed at `trunk`.
///
/// # Errors
///
/// The committed config cannot be read or does not parse: the window is then unknown.
pub fn window(git: &Git, trunk: &str) -> Result<usize, String> {
    Ok(crate::config::Config::at(git, trunk)?.red_window())
}

/// Trunk red on one post-publication check. A `bay status --since` cursor carries the red checks it saw
/// in this shape (G244b).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Red {
    pub check: String,
    pub environment: String,
    /// The check's `when_paths`; `None` means every path.
    pub paths: Option<Vec<String>>,
    /// The landing whose post check failed.
    pub landing: String,
    /// That landing's `Change-Id`, when it has one.
    pub change: Option<String>,
    /// Where the failure came in, for a bisect. Judged on each read, never carried in a
    /// cursor: `None` there.
    #[serde(skip)]
    pub suspect: Option<Suspect>,
}

/// NEW-6: the landings one of which broke a red post check, so an operator can bisect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suspect {
    /// The newest landing older than the failure whose answer is a pass; `None` when the window
    /// holds none, and the range then runs to the window's oldest landing.
    pub passed: Option<Mark>,
    /// The oldest landing after `passed` whose answer is a `product_failure`.
    pub failed: Mark,
    /// The landings after `passed` up to and including `failed`, oldest first.
    pub landings: Vec<Mark>,
    /// The one landing the evidence leaves, when the pass sits right below the first failure.
    pub culprit: Option<Culprit>,
}

/// The landing that broke a red post check, as its evidence pins it: the newest pass is its
/// first parent on trunk, and it failed. It owns the red.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Culprit {
    pub mark: Mark,
    /// The answered landings from the pass up to the newest failure, oldest first.
    pub tested: Vec<Tested>,
}

/// One answered landing of a [`Culprit`]'s range, with its combined state (`pass`,
/// `product_failure`, `flaky` or `unknown`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tested {
    pub mark: Mark,
    pub verdict: String,
}

impl Culprit {
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "landing": self.mark.landing,
            "change": self.mark.change,
            "tested": self.tested.iter().map(|tested| json!({
                "landing": tested.mark.landing,
                "change": tested.mark.change,
                "verdict": tested.verdict,
            })).collect::<Vec<_>>(),
        })
    }
}

/// A landing named in a [`Suspect`] range: its commit and its `Change-Id`, when it has one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    pub landing: String,
    pub change: Option<String>,
}

impl Mark {
    fn of(landing: &Landing) -> Self {
        Self {
            landing: landing.commit.clone(),
            change: landing.change.clone(),
        }
    }

    fn to_json(&self) -> Value {
        json!({"landing": self.landing, "change": self.change})
    }
}

impl Suspect {
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "last_passed": self.passed.as_ref().map(Mark::to_json),
            "first_failed": self.failed.to_json(),
            "landings": self.landings.iter().map(Mark::to_json).collect::<Vec<_>>(),
        })
    }
}

/// NEW-6: a post check trunk is neither red nor green on: no landing in the window answers it.
/// `state` is [`PENDING_BEYOND_WINDOW`] or [`PENDING`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    pub check: String,
    pub environment: String,
    /// The check's `when_paths`; `None` means every path.
    pub paths: Option<Vec<String>>,
    pub state: &'static str,
    /// The window judged, and its oldest landing.
    pub window: usize,
    pub oldest: Option<String>,
}

impl Pending {
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({"check": self.check, "environment": self.environment, "paths": self.paths,
            "state": self.state, "window": self.window, "oldest": self.oldest})
    }
}

/// What a red judgement of trunk found: the red checks, and those no landing in the window
/// answers. A pending check is never read as green.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Judgement {
    pub red: Vec<Red>,
    pub pending: Vec<Pending>,
}

impl Judgement {
    /// The red checks and the pending ones, as `detail.red` and `detail.red_pending`.
    #[must_use]
    pub fn to_json(&self) -> (Value, Value) {
        (
            self.red.iter().map(Red::to_json).collect(),
            self.pending.iter().map(Pending::to_json).collect(),
        )
    }
}

impl Red {
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({"check": self.check, "environment": self.environment, "paths": self.paths,
            "landing": self.landing, "change": self.change,
            "suspect": self.suspect.as_ref().map(Suspect::to_json),
            "culprit": self.culprit().map(Culprit::to_json)})
    }

    /// The landing the evidence pins as the one that broke the check, when it pins one.
    #[must_use]
    pub fn culprit(&self) -> Option<&Culprit> {
        self.suspect
            .as_ref()
            .and_then(|suspect| suspect.culprit.as_ref())
    }

    /// Whether this is the same red as `other`: the same check, owned by the same landing. Its
    /// paths may differ, after a policy edit.
    #[must_use]
    pub fn same(&self, other: &Self) -> bool {
        self.check == other.check
            && self.environment == other.environment
            && self.landing == other.landing
    }

    /// Whether a change touching `changed` lands on this red.
    #[must_use]
    pub fn covers(&self, changed: &[String]) -> bool {
        match &self.paths {
            None => !changed.is_empty(),
            Some(globs) => changed.iter().any(|path| {
                globs
                    .iter()
                    .any(|glob| crate::policy::glob_matches(glob, path))
            }),
        }
    }
}

/// One first-parent commit of trunk, with its tree and the changes its lineage trailers name.
struct Landing {
    commit: String,
    tree: String,
    change: Option<String>,
    names: Vec<String>,
}

/// G195: what a red judgement of `trunk` decided, carried in a `bay status --since` cursor so that the
/// next judgement reads only what moved since: the landings after `trunk`, and those whose
/// evidence note changed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Judged {
    /// The trunk commit judged.
    pub trunk: String,
    /// The tip of each producer's evidence notes ref then, by producer.
    pub evidence: BTreeMap<String, String>,
    /// The registry commit the records were counted under, with its mode under `signing =
    /// "approvals"` ([`Registry::judged_key`], N-21); `None` when bay ran unsigned.
    pub registry: Option<String>,
    /// How each post check judged was decided.
    pub decided: Vec<Decided>,
    /// The `[red] window` it was judged over (BACKLOG 113, a review: a judgement from the last one
    /// read only the landings after its trunk, so a window grown since never read the older ones
    /// and missed a red there for the life of the process). A cursor written before it reads as
    /// 0 and is judged in full once.
    #[serde(default)]
    pub window: usize,
}

/// The newest landing in the window whose answer to one post check is a pass or a
/// `product_failure`, which decides the check; `landing` is `None` when no landing did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decided {
    pub check: String,
    pub environment: String,
    pub landing: Option<String>,
    /// Whether the deciding answer is a `product_failure`.
    pub failed: bool,
}

/// What may answer differently since a [`Judged`]: the newest `new` landings of the window,
/// and the landings on `trees`, whose notes moved.
struct Moved {
    new: usize,
    trees: HashSet<String>,
}

/// The last judgement [`judged`] made in this process: its key (main tip, registry, evidence
/// tips), what it found, and what decided it, for the next judgement to start from.
type Last = Option<(String, Judgement, Option<Judged>)>;

/// The red checks of `trunk` (G244). A pending check is not among them: it refuses nothing.
///
/// # Errors
///
/// A git fault, or a committed config bay cannot read.
pub fn red(git: &Git, trunk: &str, registry: Option<&Registry>) -> Result<Vec<Red>, String> {
    Ok(judged(git, trunk, registry)?.red)
}

/// BACKLOG 119: whether one of `reds`, the red checks of `trunk`, covers a path `revision`
/// changes against its merge base with `trunk`: that change cannot land while they stand.
///
/// # Errors
///
/// A git fault.
pub fn covers_change(git: &Git, reds: &[Red], trunk: &str, revision: &str) -> Result<bool, String> {
    if reds.is_empty() {
        return Ok(false);
    }
    let base = git
        .run(&["merge-base", trunk, revision])
        .map_err(|error| error.message())?;
    let changed = crate::check::changed_paths(git, &base, revision)?;
    Ok(reds.iter().any(|red| red.covers(&changed)))
}

/// BACKLOG 119: whether the queue place `entry` waits on `reds`, the red checks of `trunk`: its
/// operation, as this store's chain holds it, changes a path one of them covers, or has ended
/// and lands nothing. The line passes such places in arrival order, and they keep them
/// (`crate::publish`, and the host's hook, `crate::hook`). A place whose chain or operation
/// cannot be read is never passed.
///
/// # Errors
///
/// A git fault.
pub fn holds_place(
    git: &Git,
    reds: &[Red],
    trunk: &str,
    entry: &crate::queue::Entry,
) -> Result<bool, String> {
    let Ok((_, record)) = crate::change::Chain::new(git).read(&entry.change) else {
        return Ok(false);
    };
    let Some(op) = record.operations.iter().find(|op| op.op_id == entry.op_id) else {
        return Ok(false);
    };
    if op.state.is_terminal() || op.state == crate::change::OpState::Blocked {
        return Ok(true);
    }
    covers_change(git, reds, trunk, &op.revision)
}

/// The red and the pending checks of `trunk`, as [`state`] judges them, judged once per main tip,
/// registry and evidence in this process, each judgement starting from the last ([`judge`]).
///
/// # Errors
///
/// A git fault, or a committed config bay cannot read.
pub fn judged(git: &Git, trunk: &str, registry: Option<&Registry>) -> Result<Judgement, String> {
    // G430 (a fleet finding, 2026-10-28): judged once per main tip, registry and evidence in this
    // process. Every head's turn judged the whole window from scratch, five to twelve minutes a
    // head on a loaded box with a window of 250, though nothing it reads had changed since the
    // head before. A new record moves an evidence tip, so it is judged again.
    // BACKLOG 97 (a fleet finding, 2026-10-28: the head's `red` step was its largest, p50 78 s,
    // p90 157 s, because on a busy line main or a note moves before almost every head): the next
    // judgement starts from the last one ([`judge`], G195), so it reads only what moved. The post
    // work shares it (BACKLOG 113), so a pass judges main once, for its heads and its post checks.
    static LAST: std::sync::Mutex<Last> = std::sync::Mutex::new(None);
    // The registry as the judgement counts it: its commit and, under `signing = "approvals"`,
    // its mode (N-21), so a mode switch at the same commit is never served the old judgement.
    let key = format!(
        "{trunk} {} {:?}",
        registry.map(Registry::judged_key).unwrap_or_default(),
        evidence_tips(git)?
    );
    let before = match LAST.lock() {
        Ok(last) => match last.as_ref() {
            Some((judged, judgement, _)) if *judged == key => return Ok(judgement.clone()),
            Some((_, _, before)) => before.clone(),
            None => None,
        },
        Err(_) => None,
    };
    let (judgement, judged) = judge(git, trunk, registry, before.as_ref())?;
    if let Ok(mut last) = LAST.lock() {
        *last = Some((key, judgement.clone(), judged));
    }
    Ok(judgement)
}

/// A test build writes a line to the file `BAY_TEST_RED_TRACE` names for each judgement
/// [`judge`] makes (G430): `<trunk> in full`, or `<trunk> from <trunk>` when it read only what
/// moved since an earlier one (BACKLOG 97); a release build never reads the variable.
const TEST_HOOKS: bool = option_env!("BAY_RELEASE_VERSION").is_none();

/// The [`TEST_HOOKS`] trace line of one judgement of `trunk`, from `before` when it read only
/// what moved since that one.
fn trace(trunk: &str, before: Option<&Judged>) {
    if TEST_HOOKS && let Some(path) = std::env::var_os("BAY_TEST_RED_TRACE") {
        use std::io::Write as _;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = match before {
                Some(before) => writeln!(file, "{trunk} from {}", before.trunk),
                None => writeln!(file, "{trunk} in full"),
            };
        }
    }
}

/// The red and the pending checks of `trunk` (G244, NEW-6).
///
/// # Errors
///
/// A git fault, or a committed config bay cannot read.
pub fn state(git: &Git, trunk: &str, registry: Option<&Registry>) -> Result<Judgement, String> {
    let Some(policy) = post_policy(git, trunk)? else {
        return Ok(Judgement::default());
    };
    let window = window(git, trunk)?;
    let (landings, full) = landings(git, trunk, window)?;
    Ok(decide(git, &policy, &landings, (window, full), registry, None)?.0)
}

/// The red and pending checks of `trunk`, as [`state`] judges them, and what decided them
/// (`None` when the policy has no post check). Given `before`, a judgement of an earlier trunk, it reads the
/// answers only of the landings after `before.trunk` and of those whose notes moved since
/// (G195), so its cost grows with what changed, not with the window. It judges in full when
/// `before` cannot say: trunk left it behind the window or rewrote it, the registry moved, a
/// producer's ref went away, or the landing that decided a check no longer answers it.
///
/// # Errors
///
/// A git fault, or a committed config bay cannot read.
pub fn judge(
    git: &Git,
    trunk: &str,
    registry: Option<&Registry>,
    before: Option<&Judged>,
) -> Result<(Judgement, Option<Judged>), String> {
    let Some(policy) = post_policy(git, trunk)? else {
        return Ok((Judgement::default(), None));
    };
    let window = window(git, trunk)?;
    // The tips before the answers: a record written in between reads as moved next time.
    let evidence = evidence_tips(git)?;
    let (landings, full) = landings(git, trunk, window)?;
    let known = before.and_then(|before| {
        moved(git, before, &evidence, registry, &landings, window).map(|moved| (before, moved))
    });
    trace(trunk, known.as_ref().map(|(before, _)| *before));
    let (judgement, decided) = decide(
        git,
        &policy,
        &landings,
        (window, full),
        registry,
        known.as_ref(),
    )?;
    Ok((
        judgement,
        Some(Judged {
            trunk: trunk.to_owned(),
            evidence,
            registry: registry.map(Registry::judged_key),
            decided,
            window,
        }),
    ))
}

/// The policy at `trunk`, when it has a post check.
fn post_policy(git: &Git, trunk: &str) -> Result<Option<crate::policy::Policy>, String> {
    match crate::policy::load_at(git, trunk) {
        Ok(Some(policy)) if !policy.post.is_empty() => Ok(Some(policy)),
        Ok(_) | Err(crate::policy::LoadError::Invalid(_)) => Ok(None),
        Err(crate::policy::LoadError::Git(why)) => Err(why),
    }
}

/// The red and pending checks over `landings`, the landing line of the newest `window`
/// first-parent commits of trunk (`full` when the window held that many), and how each
/// check was decided. With `known`, a check `known` decided is judged from what moved; any
/// other check is judged over the window.
fn decide(
    git: &Git,
    policy: &crate::policy::Policy,
    landings: &[Landing],
    (window, full): (usize, bool),
    registry: Option<&Registry>,
    known: Option<&(&Judged, Moved)>,
) -> Result<(Judgement, Vec<Decided>), String> {
    let mut answers = Answers {
        git,
        registry,
        landings,
        full,
        rows: vec![None; landings.len()],
    };
    let mut out = Judgement::default();
    let mut decided = Vec::new();
    for post in &policy.post {
        let id = crate::evidence::CheckId::new(&post.id, &post.environment);
        let Some(entry) = policy.check(&id) else {
            continue;
        };
        if policy.is_quarantined(entry) {
            continue;
        }
        let carried = known.and_then(|(before, moved)| {
            before
                .decided
                .iter()
                .find(|then| then.check == post.id && then.environment == post.environment)
                .map(|then| (then, moved))
        });
        let answer = match carried {
            Some((then, moved)) => answers.since(post, then, moved)?,
            None => answers.newest(post)?,
        };
        decided.push(Decided {
            check: post.id.clone(),
            environment: post.environment.clone(),
            landing: answer.map(|(at, _)| landings[at].commit.clone()),
            failed: answer.is_some_and(|(_, failed)| failed),
        });
        let Some((at, failed)) = answer else {
            // NEW-6: no answer in the window is never green.
            if let Some(state) = answers.unanswered(post)? {
                out.pending.push(Pending {
                    check: post.id.clone(),
                    environment: post.environment.clone(),
                    paths: entry.when_paths.clone(),
                    state,
                    window,
                    oldest: landings.last().map(|landing| landing.commit.clone()),
                });
            }
            continue;
        };
        if !failed {
            continue;
        }
        // A later landing that fixes, undoes or supersedes the owner clears it.
        let answered = |owner: usize| {
            landings[owner].change.as_ref().is_some_and(|change| {
                landings[..owner]
                    .iter()
                    .any(|later| later.names.contains(change))
            })
        };
        if answered(at) {
            continue;
        }
        // The culprit, when the evidence pins one, owns the red in place of the newest failure.
        let (suspect, owner) = answers.suspect(post, at)?;
        if owner != at && answered(owner) {
            continue;
        }
        let landing = &landings[owner];
        out.red.push(Red {
            check: post.id.clone(),
            environment: post.environment.clone(),
            paths: entry.when_paths.clone(),
            landing: landing.commit.clone(),
            change: landing.change.clone(),
            suspect: Some(suspect),
        });
    }
    Ok((out, decided))
}

/// The post answers of the window's landings, each read once and only when asked.
struct Answers<'a> {
    git: &'a Git,
    registry: Option<&'a Registry>,
    landings: &'a [Landing],
    /// Whether the window read as many first-parent commits as it holds: trunk may go on
    /// below it.
    full: bool,
    rows: Vec<Option<Vec<Value>>>,
}

impl Answers<'_> {
    /// How landing `at` answers `post`: `Some(true)` for a `product_failure`, `Some(false)`
    /// for a pass, `None` when it does not decide (not answered yet, or not required of it).
    fn decides(
        &mut self,
        at: usize,
        post: &crate::policy::ManifestEntry,
    ) -> Result<Option<bool>, String> {
        let state = self.row(at, post)?.and_then(|row| row["state"].as_str());
        Ok(match state {
            Some("pass") => Some(false),
            Some("product_failure") => Some(true),
            _ => None,
        })
    }

    /// Landing `at`'s answer row of `post`, read once.
    fn row(
        &mut self,
        at: usize,
        post: &crate::policy::ManifestEntry,
    ) -> Result<Option<&Value>, String> {
        if self.rows[at].is_none() {
            self.rows[at] = Some(crate::publish::post_answers(
                self.git,
                &self.landings[at].commit,
                self.registry,
                true,
            )?);
        }
        Ok(self.rows[at].as_ref().and_then(|rows| {
            rows.iter()
                .find(|row| row["check"] == post.id && row["environment"] == post.environment)
        }))
    }

    /// Whether landing `at`'s diff requires `post` (its `when_paths`, judged at its base).
    fn requires(&mut self, at: usize, post: &crate::policy::ManifestEntry) -> Result<bool, String> {
        Ok(self
            .row(at, post)?
            .is_some_and(|row| row["required"] == true))
    }

    /// NEW-6: the [`Pending`] state of `post`, which no landing of the window decides, or
    /// `None` when nothing is owed: the window holds all of trunk and no landing requires it.
    /// A full window may stand on an older answer it cannot see, so it is never green.
    fn unanswered(
        &mut self,
        post: &crate::policy::ManifestEntry,
    ) -> Result<Option<&'static str>, String> {
        if self.full {
            return Ok(Some(PENDING_BEYOND_WINDOW));
        }
        for at in 0..self.landings.len() {
            if self.requires(at, post)? {
                return Ok(Some(PENDING));
            }
        }
        Ok(None)
    }

    /// NEW-6: the suspect range of `post`, red from landing `at`: older landings are read back
    /// to the newest pass, and the oldest failure after it is the first failure. With it, the
    /// landing that owns the red: the culprit when the pass sits right below the first failure,
    /// else `at`.
    fn suspect(
        &mut self,
        post: &crate::policy::ManifestEntry,
        at: usize,
    ) -> Result<(Suspect, usize), String> {
        let mut first = at;
        let mut passed = None;
        for older in at + 1..self.landings.len() {
            match self.decides(older, post)? {
                Some(true) => first = older,
                Some(false) => {
                    passed = Some(older);
                    break;
                }
                None => {}
            }
        }
        let end = passed.unwrap_or(self.landings.len());
        // Landings are newest first: the pass right below the first failure is at `first + 1`.
        let culprit = match passed {
            Some(older) if older == first + 1 => Some(self.culprit(post, first, at, older)?),
            _ => None,
        };
        let owner = if culprit.is_some() { first } else { at };
        Ok((
            Suspect {
                passed: passed.map(|older| Mark::of(&self.landings[older])),
                failed: Mark::of(&self.landings[first]),
                landings: self.landings[first..end]
                    .iter()
                    .rev()
                    .map(Mark::of)
                    .collect(),
                culprit,
            },
            owner,
        ))
    }

    /// The [`Culprit`] of `post` at landing `first`, with every answered landing from the pass
    /// at `passed` up to the newest failure at `at`, oldest first. Each was read for the range.
    fn culprit(
        &mut self,
        post: &crate::policy::ManifestEntry,
        first: usize,
        at: usize,
        passed: usize,
    ) -> Result<Culprit, String> {
        let mut tested = Vec::new();
        for index in (at..=passed).rev() {
            let state = self
                .row(index, post)?
                .and_then(|row| row["state"].as_str())
                .filter(|state| *state != "pending")
                .map(str::to_owned);
            if let Some(verdict) = state {
                tested.push(Tested {
                    mark: Mark::of(&self.landings[index]),
                    verdict,
                });
            }
        }
        Ok(Culprit {
            mark: Mark::of(&self.landings[first]),
            tested,
        })
    }

    /// The newest landing that decides `post`, and whether it failed.
    fn newest(
        &mut self,
        post: &crate::policy::ManifestEntry,
    ) -> Result<Option<(usize, bool)>, String> {
        for at in 0..self.landings.len() {
            if let Some(failed) = self.decides(at, post)? {
                return Ok(Some((at, failed)));
            }
        }
        Ok(None)
    }

    /// [`Self::newest`] from `then`, how `post` was decided before, reading only the landings
    /// `moved` names: every other landing answers as it did, so none newer than `then`'s
    /// decided, and `then`'s still does. When `then`'s landing no longer decides, the older
    /// landings are unknown and the whole window is read.
    fn since(
        &mut self,
        post: &crate::policy::ManifestEntry,
        then: &Decided,
        moved: &Moved,
    ) -> Result<Option<(usize, bool)>, String> {
        let carried = then.landing.as_ref().and_then(|commit| {
            self.landings
                .iter()
                .position(|landing| &landing.commit == commit)
        });
        // A review (2026-10-28): a landing that decided before and is no longer on the landing
        // line (an outside merge moved it off) says nothing of the older landings, which may
        // still decide: the whole window is read.
        if then.landing.is_some() && carried.is_none() {
            return self.newest(post);
        }
        for at in 0..self.landings.len() {
            if at < moved.new || moved.trees.contains(&self.landings[at].tree) {
                if let Some(failed) = self.decides(at, post)? {
                    return Ok(Some((at, failed)));
                }
                if carried == Some(at) {
                    return self.newest(post);
                }
            } else if carried == Some(at) {
                return Ok(Some((at, then.failed)));
            }
        }
        Ok(None)
    }
}

/// The tip of each producer's evidence notes ref, by producer.
fn evidence_tips(git: &Git) -> Result<BTreeMap<String, String>, String> {
    let refs = git
        .run(&[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            crate::evidence::EVIDENCE_PREFIX,
        ])
        .map_err(|error| error.message())?;
    Ok(refs
        .lines()
        .filter_map(|line| {
            let (name, tip) = line.split_once(' ')?;
            let producer = name.strip_prefix(crate::evidence::EVIDENCE_PREFIX)?;
            Some((producer.to_owned(), tip.to_owned()))
        })
        .collect())
}

/// What may answer differently since `before`, or `None` when `before` cannot say: its trunk
/// is not in `landings` (trunk moved past the window, or was rewritten), the registry moved,
/// a producer's ref went away, or a moved ref cannot be compared. The trees come from one
/// `diff-tree` per moved ref, and one `ls-tree` per new one: a note's path is its tree's id.
fn moved(
    git: &Git,
    before: &Judged,
    evidence: &BTreeMap<String, String>,
    registry: Option<&Registry>,
    landings: &[Landing],
    window: usize,
) -> Option<Moved> {
    if before.window != window
        || before.registry != registry.map(Registry::judged_key)
        || before
            .evidence
            .keys()
            .any(|producer| !evidence.contains_key(producer))
    {
        return None;
    }
    let new = landings
        .iter()
        .position(|landing| landing.commit == before.trunk)?;
    let mut trees = HashSet::new();
    for (producer, tip) in evidence {
        let paths = match before.evidence.get(producer) {
            Some(then) if then == tip => continue,
            Some(then) => git.run(&["diff-tree", "-r", "--name-only", then, tip]),
            None => git.run(&["ls-tree", "-r", "--name-only", tip]),
        }
        .ok()?;
        trees.extend(paths.lines().map(|path| path.replace('/', "")));
    }
    Some(Moved { new, trees })
}

/// N-20: the landing line ([`crate::trunkline`]) of the newest `window` first-parent commits
/// of `trunk`, newest first, with each bay landing an outside merge pushed off the first-parent
/// line placed at that merge; and whether the window was full.
fn landings(git: &Git, trunk: &str, window: usize) -> Result<(Vec<Landing>, bool), String> {
    let line = crate::trunkline::walk(
        git,
        trunk,
        window,
        None,
        "%T%x1f%(trailers:key=Fixes,valueonly,separator=%x2c)%x1f%(trailers:key=Reverts,valueonly,separator=%x2c)%x1f%(trailers:key=Supersedes,valueonly,separator=%x2c)",
    )?;
    let full = line.first_parent >= window;
    let landings = line
        .entries
        .into_iter()
        .filter_map(|entry| {
            let (tree, lineage) = entry.extra.split_first()?;
            let names = lineage
                .iter()
                .flat_map(|field| field.split(','))
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect();
            Some(Landing {
                tree: tree.clone(),
                change: (!entry.change.is_empty()).then(|| entry.change.clone()),
                commit: entry.commit,
                names,
            })
        })
        .collect();
    Ok((landings, full))
}
