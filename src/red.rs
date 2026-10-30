//! G244: a red trunk is owned until it is fixed, undone or quarantined (the pilot-gaps analysis
//! of 2026-10-20).
//!
//! A post-publication check (`stage = "post"`, G91) judges a landing after it is on trunk. When
//! the newest answer of such a check on trunk is a `product_failure`, trunk is **red** on that
//! check's paths (its `when_paths`, read with their `!` exclusions, or every path), and the
//! landing that broke it owns it. It is derived from the records on each read, never stored:
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
//! - A red check names its `suspect` range ([`Suspect`]): the newest pass older than the failure,
//!   the first failure after it, and the landings in between, on the check's paths or not (a
//!   cause can sit in a file the check does not list).
//! - B5: when the window holds no pass, the range reads on past it, down to `[red] reach`
//!   first-parent commits of main (default [`DEFAULT_REACH`], never less than the window), for
//!   the newest pass. With none there, `last_passed` is `null` and `searched` says how many main
//!   commits it read: never a guessed range end.
//! - Culprit finding: when the newest pass sits right below the first failure, one landing is
//!   left in the range, and the evidence names it. That landing is the `culprit`, with the
//!   answers `tested` from the pass up to the newest failure, and it owns the red in place of
//!   the newest failure. A post producer, or an executor with `[runner] bisect_post`
//!   (`crate::bisect`), narrows the range by recording runs on its midpoints.
//! - BACKLOG 121: without a culprit, the owner (the newest failure) and the first failure are
//!   only where the check was seen failing, never a proven cause; each red row's `say` says
//!   which ([`Red::say`]).
//! - G568, E36 (fleet findings, 2026-10-29): a landing newer than the newest failure whose
//!   `Fixes:`, `Reverts:` or `Supersedes:` names a merge the red names (the owner, a landing of
//!   the suspect range, or one the check was seen failing on since; [`Red::answered_by`])
//!   answers the red: the check is then [`PENDING_FIX`] until a run at or after that landing
//!   answers it, never green before. A fix older than the newest failure landed before a run
//!   that still failed, so it answers nothing. A quarantine of the check at trunk clears it.
//!   E35: while it is `pending_fix`, the check holds the changes on its paths as a red does
//!   ([`holding`]): a fix may answer one failure of several. G572: a repair of another red
//!   passes it ([`blocking`]).
//!   Without a culprit the owner moves to each newer failure, so the red row names one stable
//!   name for `--fixes` ([`Red::fixes`]: the first failure).
//! - G584 (a fleet finding, 2026-10-30: a check that never passed read back to the reach, 545 s
//!   a judgement, and named the newest failure): the walk to a pass stops at the landing whose
//!   base policy lists other post checks and not this one, the landing that added the check
//!   ([`Suspect::added`]). With no pass after it, that landing owns the red, and `--fixes` names
//!   it: the check never passed, so the cause is the check, its setup, or a merge since.
//!
//! While red or `pending_fix`, the executor refuses a candidate that touches those paths with
//! `MAIN_RED`, naming the owner and the ways out; a change that names a merge the red names in
//! `--fixes`, an undo of it, or a superseding change is never refused, and it runs ahead of the
//! line.
//!
//! G195: `bay status --since` judges from what its cursor carries ([`Judged`]): how each check
//! was decided, at which trunk, under which evidence tips and registry. It reads the answers of
//! the landings after that trunk and of those whose note moved (one `diff-tree` per moved
//! producer ref), so its Git work grows with what changed, not with the window. Two reads are
//! bounded by the window instead: while a check is red, its suspect range reads back to its
//! last pass (B5: down to the reach); and while no landing answers a check in a window that
//! holds all of trunk, the landings are read until one requires it. S4: a runner's next
//! judgement keeps the answers of every landing it read, past the window too, whose notes did
//! not move, so a read back to a far pass is paid once per runner, not once per head.
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

/// How many first-parent main commits a red check whose window holds no pass reads back for its
/// last pass unless `[red] reach` says (B5): five default windows, about half a day of a main
/// that takes 500 merges a day. Each commit past the window costs one
/// read of its evidence notes, paid only while a red check has no pass in its window, and once
/// per runner process (S4).
pub const DEFAULT_REACH: usize = 250;

/// The [`Pending`] state of a check whose answer, if any, is older than the window.
pub const PENDING_BEYOND_WINDOW: &str = "pending_beyond_window";

/// The [`Pending`] state of a check no landing answered yet, the window holding all of trunk.
pub const PENDING: &str = "pending";

/// G568: the [`Pending`] state of a red check a landing newer than its newest failure answers
/// ([`Red::answered_by`]), until a run at or after that landing answers it.
pub const PENDING_FIX: &str = "pending_fix";

/// The longest check id a sentence names ([`shown_check`]).
const MAX_SHOWN_CHECK: usize = 64;

/// R13: the check id a sentence for a person may name: at most [`MAX_SHOWN_CHECK`] characters
/// of `[A-Za-z0-9._:/-]`, else `None`, and the sentence says "a post check" instead.
#[must_use]
pub fn shown_check(check: &str) -> Option<&str> {
    (!check.is_empty()
        && check.len() <= MAX_SHOWN_CHECK
        && check
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:/-".contains(&byte)))
    .then_some(check)
}

/// How many first-parent commits of `trunk` a red judgement and the unattributed scan read
/// back: `[red] window` in the config committed at `trunk`.
///
/// # Errors
///
/// The committed config cannot be read or does not parse: the window is then unknown.
pub fn window(git: &Git, trunk: &str) -> Result<usize, String> {
    Ok(crate::config::Config::at(git, trunk)?.red_window())
}

/// `[red] window` and `[red] reach` (B5) in the config committed at `trunk`, read once.
fn bounds(git: &Git, trunk: &str) -> Result<(usize, usize), String> {
    let config = crate::config::Config::at(git, trunk)?;
    Ok((config.red_window(), config.red_reach()))
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
    /// E35: the landing that answers this red, whose run, or a newer one, decides: the check is
    /// [`PENDING_FIX`], and it holds the changes on its paths as a red does ([`holding`]).
    /// Judged on each read, never carried in a cursor.
    #[serde(skip)]
    pub fix: Option<Mark>,
    /// G576: whether the newest failure changed the check itself, its policy entry or an
    /// adapter file. A post run judges a landing with its first parent's check (G91), so no run
    /// of that landing judges with the new one. Judged on each read, never carried in a cursor.
    #[serde(skip)]
    pub changed_check: bool,
}

/// NEW-6: the landings one of which broke a red post check, so an operator can bisect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suspect {
    /// The newest landing older than the failure whose answer is a pass; `None` when none down
    /// to the reach answers so (B5), and the range then runs to the oldest landing read.
    pub passed: Option<Mark>,
    /// The oldest landing after `passed` whose answer is a `product_failure`.
    pub failed: Mark,
    /// The landings after `passed` up to and including `failed`, oldest first.
    pub landings: Vec<Mark>,
    /// E36: the landings after `failed` up to and including `newest_failed`, oldest first,
    /// where the check was seen failing or did not run; empty when `failed` is the newest.
    pub failing: Vec<Mark>,
    /// The newest landing whose answer is a `product_failure`: the one that decides the red.
    /// Its failing record names the units a post run asks to run again (`crate::bisect`, G540).
    pub newest_failed: Mark,
    /// The one landing the evidence leaves, when the pass sits right below the first failure.
    pub culprit: Option<Culprit>,
    /// B5: with no pass found, how many first-parent commits of main were read back for one;
    /// `None` when `passed` or `added` names where the walk stopped.
    pub searched: Option<usize>,
    /// G584: with no pass found, the landing that added the check: its base policy lists other
    /// post checks and not this one. It owns the red, and the range ends at it.
    pub added: Option<Mark>,
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

    /// How a sentence names it: its change, else its commit, short.
    pub(crate) fn named(&self) -> String {
        match &self.change {
            Some(change) => format!("change {change}"),
            None => format!("merge {}", &self.landing[..self.landing.len().min(12)]),
        }
    }
}

impl Suspect {
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "last_passed": self.passed.as_ref().map(Mark::to_json),
            "first_failed": self.failed.to_json(),
            "landings": self.landings.iter().map(Mark::to_json).collect::<Vec<_>>(),
            "failing": self.failing.iter().map(Mark::to_json).collect::<Vec<_>>(),
            "searched": self.searched,
            "check_added": self.added.as_ref().map(Mark::to_json),
        })
    }
}

/// NEW-6: a post check trunk is neither red nor green on: no landing in the window answers it.
/// `state` is [`PENDING_BEYOND_WINDOW`], [`PENDING`] or [`PENDING_FIX`].
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
    /// [`PENDING_FIX`]: the landing that answers the red, whose run, or a newer one, decides.
    pub fix: Option<Mark>,
}

impl Pending {
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({"check": self.check, "environment": self.environment, "paths": self.paths,
            "state": self.state, "window": self.window, "oldest": self.oldest,
            "fix": self.fix.as_ref().map(Mark::to_json)})
    }
}

/// What a red judgement of trunk found: the red checks, and those no landing in the window
/// answers. A pending check is never read as green.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Judgement {
    pub red: Vec<Red>,
    pub pending: Vec<Pending>,
    /// E35: the red checks a fix answers whose run has not passed yet ([`PENDING_FIX`], each
    /// also in `pending`, with its fix in [`Red::fix`]). They hold the changes on their paths.
    pub fixing: Vec<Red>,
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
            "culprit": self.culprit().map(Culprit::to_json),
            "fixes": self.fixes(),
            "fix": self.fix.as_ref().map(Mark::to_json),
            "changed_check": self.changed_check,
            "say": self.say()})
    }

    /// BACKLOG 121 (a fleet finding, 2026-10-28: a red row named a change the check never
    /// judged, and readers took it as the cause): what the row's owner is, in one plain
    /// sentence. A culprit is a proven cause: the check passed right below it and failed on it.
    /// Without one, the owner and the first failure are only where the check was seen failing,
    /// and the cause is any merge of the range, on the check's paths or not. `None` for a red a
    /// cursor carries, which holds no range.
    #[must_use]
    pub fn say(&self) -> Option<String> {
        let suspect = self.suspect.as_ref()?;
        let check = &self.check;
        if let Some(fix) = &self.fix {
            return Some(format!(
                "{check}: {} fixes it, and its run has not passed yet; until one passes, changes \
                 on the check's paths wait.",
                fix.named()
            ));
        }
        let said = self.seen(suspect);
        if !self.changed_check {
            return Some(said);
        }
        Some(format!(
            "{said} Its newest failure, {}, changed the check itself, and a post run judges a \
             merge with the check of the merge before it, so no run of it judges with the new \
             check; land one change with --fixes {check}: its post run does.",
            suspect.newest_failed.named()
        ))
    }

    /// [`Red::say`] of a red no fix answers: where the check was seen failing, and what the
    /// evidence says of the cause.
    fn seen(&self, suspect: &Suspect) -> String {
        let check = &self.check;
        if let Some(culprit) = &suspect.culprit {
            return format!(
                "{check}: {} broke it; the check passed on the merge before it and failed on it.",
                culprit.mark.named()
            );
        }
        let seen = if suspect.failed == suspect.newest_failed {
            format!("seen failing at {}", suspect.failed.named())
        } else {
            format!(
                "seen failing at {} first and at {} last",
                suspect.failed.named(),
                suspect.newest_failed.named()
            )
        };
        let cause = match (&suspect.passed, &suspect.added) {
            (Some(passed), _) => format!(
                "the cause is one of the {} merges after its last pass at {}, on its paths or not",
                suspect.landings.len(),
                passed.named()
            ),
            (None, Some(added)) => format!(
                "it has not passed since {} added it to the policy, so that merge owns the red: \
                 the cause is the check itself, its setup, or a merge since, on its paths or not",
                added.named()
            ),
            (None, None) => format!(
                "no merge of the newest {} on main passed it, so bay cannot narrow the cause",
                suspect.searched.unwrap_or_default()
            ),
        };
        format!("{check}: {seen}, which is where it failed, not a proven cause; {cause}.")
    }

    /// The landing the evidence pins as the one that broke the check, when it pins one.
    #[must_use]
    pub fn culprit(&self) -> Option<&Culprit> {
        self.suspect
            .as_ref()
            .and_then(|suspect| suspect.culprit.as_ref())
    }

    /// Whether a change that names `names` (its `Fixes:`, `Reverts:` and `Supersedes:`) answers
    /// this red: they name the owning change, a landing of its suspect range, or one the check
    /// was seen failing on since ([`Suspect::failing`]). Fleet findings: with no culprit pinned
    /// the owner is the newest failure, the landing the check last ran on, so it moves with each
    /// run; three fixes naming the first failure (2026-10-28), then one naming the owner of an
    /// hour before (E36, 2026-10-29), waited behind the red they were written to clear. They
    /// may also name the check itself (`start --fixes <check>`): the one name that never moves,
    /// when a pass between failures starts a new red (an intermittent check, 2026-10-29). It
    /// lets a change through; the landing a red names its fix is `fix_of`'s (E68).
    #[must_use]
    pub fn answered_by(&self, names: &[&String]) -> bool {
        if names.contains(&&self.check) {
            return true;
        }
        let range = self.suspect.iter().flat_map(|suspect| {
            suspect
                .landings
                .iter()
                .chain(&suspect.failing)
                .filter_map(|mark| mark.change.as_ref())
        });
        self.change
            .iter()
            .chain(range)
            .any(|change| names.contains(&change))
    }

    /// E68: whether `names` name this red alone: its check, its owning change, its fix name
    /// ([`Red::fixes`]) or a landing it was seen failing on. A landing of the suspect range is
    /// not one of them: the range of another red can hold it too.
    fn named_by(&self, names: &[&String]) -> bool {
        if names.contains(&&self.check) {
            return true;
        }
        let failing = self
            .suspect
            .iter()
            .flat_map(|suspect| &suspect.failing)
            .filter_map(|mark| mark.change.as_ref());
        self.change
            .iter()
            .chain(self.fixes())
            .chain(failing)
            .any(|change| names.contains(&change))
    }

    /// E36: the one name a fix gives in `--fixes`, stable while the red stands: the landing
    /// that added a check that never passed (G584), else the first failure (the culprit, when
    /// the evidence pins one), else the owning change. The owner moves to each newer failure
    /// while neither pins it.
    #[must_use]
    pub fn fixes(&self) -> Option<&String> {
        self.suspect
            .as_ref()
            .and_then(|suspect| {
                let added = suspect.added.as_ref().and_then(|mark| mark.change.as_ref());
                added.or(suspect.failed.change.as_ref())
            })
            .or(self.change.as_ref())
    }

    /// Whether this is the same red as `other`: the same check, owned by the same landing. Its
    /// paths may differ, after a policy edit.
    #[must_use]
    pub fn same(&self, other: &Self) -> bool {
        self.check == other.check
            && self.environment == other.environment
            && self.landing == other.landing
    }

    /// Whether a change touching `changed` lands on this red: one of them is on the check's
    /// paths, read as its `when_paths` are (G500: a path they exclude is off the red).
    #[must_use]
    pub fn covers(&self, changed: &[String]) -> bool {
        match &self.paths {
            None => !changed.is_empty(),
            Some(globs) => changed
                .iter()
                .any(|path| crate::policy::globs_match(globs, path)),
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

/// The last judgement [`judged`] made in this process, for the next judgement to start from.
struct Last {
    /// Main's tip, the registry and the evidence tips it judged.
    key: String,
    /// What it found.
    judgement: Judgement,
    /// What decided it.
    judged: Option<Judged>,
    /// S4: the post answers it read, by landing commit ([`Answered`]).
    answers: Answered,
}

/// S4: the post answers a judgement read, by landing commit. A landing's answers change only
/// when the notes of its tree move (they are a function of its commit, its tree's records and
/// the registry), so the next judgement reads again only the landings whose notes moved, and
/// those newer than the last one judged.
type Answered = std::collections::HashMap<String, Vec<Value>>;

/// The red checks of `trunk` (G244). A pending check is not among them: it refuses nothing.
///
/// # Errors
///
/// A git fault, or a committed config bay cannot read.
pub fn red(git: &Git, trunk: &str, registry: Option<&Registry>) -> Result<Vec<Red>, String> {
    Ok(judged(git, trunk, registry)?.red)
}

/// E35 (a fleet finding, 2026-10-29): the checks of `trunk` that hold the changes on their
/// paths: the red ones, and those a fix answers whose run has not passed yet ([`PENDING_FIX`]).
/// Before, a fix's landing let every change on the check's paths land unchecked until the next
/// run, though the fix answered one failure of seven. A change that names the red still runs.
///
/// # Errors
///
/// A git fault, or a committed config bay cannot read.
pub fn holding(git: &Git, trunk: &str, registry: Option<&Registry>) -> Result<Vec<Red>, String> {
    let judgement = judged(git, trunk, registry)?;
    Ok(judgement.red.into_iter().chain(judgement.fixing).collect())
}

/// G244: whether a change that names `names` (its `Fixes:`, `Reverts:` and `Supersedes:`)
/// repairs a red post check of `trunk` ([`Red::answered_by`]), so it runs and lands from its own
/// place, ahead of the line. The runner (`crate::publish`) and the host's hook (`crate::hook`)
/// judge by this one rule: a host that judged by fewer names refused the repair the runner ran.
///
/// # Errors
///
/// A git fault, or a committed config bay cannot read.
pub fn repairs(
    git: &Git,
    trunk: &str,
    registry: Option<&Registry>,
    names: &[&String],
) -> Result<bool, String> {
    if names.is_empty() {
        return Ok(false);
    }
    Ok(holding(git, trunk, registry)?
        .iter()
        .any(|red| red.answered_by(names)))
}

/// The ones of `reds`, the holding checks of trunk ([`holding`]), that hold a change changing
/// `changed` whose `Fixes:`, `Reverts:` and `Supersedes:` name `names`: each covers a path it
/// changes, and it does not answer it ([`Red::answered_by`]). G572 (a fleet finding,
/// 2026-10-29: a fix for records-refs waited behind scripts-fast, whose own fix was on main and
/// waited only for its run, and that run waited for a worker): a repair, a change that answers
/// one of `reds`, also passes a check that is [`PENDING_FIX`]. That check's fix is on main
/// already; only its run decides, and the repair runs ahead of the line, so it never waits for
/// it. A change that repairs nothing still waits (E35).
#[must_use]
pub fn blocking<'a>(reds: &'a [Red], changed: &[String], names: &[&String]) -> Vec<&'a Red> {
    let repair = reds.iter().any(|red| red.answered_by(names));
    reds.iter()
        .filter(|red| red.covers(changed) && !red.answered_by(names))
        .filter(|red| !(repair && red.fix.is_some()))
        .collect()
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
    // S4 (a fleet measurement, 2026-10-28, bay 0.5.40-0.5.42: a merging head's `red` step took
    // 69-185 s, median 76 s, on 8 of 14 merges, half of its time to main): from the last one, a
    // judgement still read every landing back to a red check's last pass, and every landing again
    // when the deciding one stopped deciding, each a read of every evidence ref. It reads only
    // the landings whose answers can differ now ([`Answered`]).
    static LAST: std::sync::Mutex<Option<Last>> = std::sync::Mutex::new(None);
    // The registry as the judgement counts it: its commit and, under `signing = "approvals"`,
    // its mode (N-21), so a mode switch at the same commit is never served the old judgement.
    let key = format!(
        "{trunk} {} {:?}",
        registry.map(Registry::judged_key).unwrap_or_default(),
        evidence_tips(git)?
    );
    let (before, answers) = match LAST.lock() {
        Ok(mut last) => match last.as_mut() {
            Some(last) if last.key == key => return Ok(last.judgement.clone()),
            Some(last) => (last.judged.clone(), std::mem::take(&mut last.answers)),
            None => (None, Answered::new()),
        },
        Err(_) => (None, Answered::new()),
    };
    let (judgement, judged, answers) = judge_from(git, trunk, registry, before.as_ref(), &answers)?;
    if let Ok(mut last) = LAST.lock() {
        *last = Some(Last {
            key,
            judgement: judgement.clone(),
            judged,
            answers,
        });
    }
    Ok(judgement)
}

/// A test build writes a line to the file `BAY_TEST_RED_TRACE` names for each judgement
/// [`judge`] makes (G430): `<trunk> read <n> in full`, or `<trunk> read <n> from <trunk>` when it
/// started from an earlier one (BACKLOG 97); `<n>` is how many landings' answers it read (S4). A
/// release build never reads the variable.
const TEST_HOOKS: bool = option_env!("BAY_RELEASE_VERSION").is_none();

/// The [`TEST_HOOKS`] trace line of one judgement of `trunk` that read the answers of `read`
/// landings, from `before` when it started from that one.
fn trace(trunk: &str, before: Option<&Judged>, read: usize) {
    if TEST_HOOKS && let Some(path) = std::env::var_os("BAY_TEST_RED_TRACE") {
        use std::io::Write as _;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = match before {
                Some(before) => writeln!(file, "{trunk} read {read} from {}", before.trunk),
                None => writeln!(file, "{trunk} read {read} in full"),
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
    let (window, reach) = bounds(git, trunk)?;
    let line = line(git, trunk, window)?;
    let unread = vec![None; line.landings.len()];
    let past = Past {
        trunk,
        reach,
        read: false,
        kept: None,
    };
    let answers = Answers::new(git, registry, line, unread, past);
    Ok(decide(&policy, window, None, answers)?.0)
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
    let (judgement, judged, _) = judge_from(git, trunk, registry, before, &Answered::new())?;
    Ok((judgement, judged))
}

/// [`judge`], given `answers`, the post answers the judgement `before` read: those of a
/// landing whose notes did not move since stand, unread (S4). With what this judgement read.
fn judge_from(
    git: &Git,
    trunk: &str,
    registry: Option<&Registry>,
    before: Option<&Judged>,
    answers: &Answered,
) -> Result<(Judgement, Option<Judged>, Answered), String> {
    let Some(policy) = post_policy(git, trunk)? else {
        return Ok((Judgement::default(), None, Answered::new()));
    };
    let (window, reach) = bounds(git, trunk)?;
    // The tips before the answers: a record written in between reads as moved next time.
    let evidence = evidence_tips(git)?;
    let line = line(git, trunk, window)?;
    let known = before.and_then(|before| {
        moved(git, before, &evidence, registry, &line.landings, window).map(|moved| (before, moved))
    });
    // S4: only under a judgement `moved` can compare with (the same registry, window and
    // producers), and only for a landing it judged whose notes did not move.
    let carried: Vec<Option<Vec<Value>>> = line
        .landings
        .iter()
        .enumerate()
        .map(|(at, landing)| {
            known
                .as_ref()
                .filter(|(_, moved)| at >= moved.new && !moved.trees.contains(&landing.tree))
                .and_then(|_| answers.get(&landing.commit).cloned())
        })
        .collect();
    // B5: the landings past the window are older than any new one, so only their notes decide.
    let past = Past {
        trunk,
        reach,
        read: false,
        kept: known.as_ref().map(|(_, moved)| (answers, &moved.trees)),
    };
    let (judgement, decided, read) = decide(
        &policy,
        window,
        known.as_ref(),
        Answers::new(git, registry, line, carried, past),
    )?;
    trace(trunk, known.as_ref().map(|(before, _)| *before), read.read);
    let answers = read
        .line
        .landings
        .into_iter()
        .zip(read.rows)
        .filter_map(|(landing, rows)| Some((landing.commit, rows?)))
        .collect();
    Ok((
        judgement,
        Some(Judged {
            trunk: trunk.to_owned(),
            evidence,
            registry: registry.map(Registry::judged_key),
            decided,
            window,
        }),
        answers,
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

/// The red and pending checks over `answers`' landings, the landing line of the newest `window`
/// first-parent commits of trunk, and how each check was decided. With `known`, a check `known`
/// decided is judged from what moved; any other check is judged over the window. With the
/// answers, read and kept, after the judgement.
fn decide<'a>(
    policy: &crate::policy::Policy,
    window: usize,
    known: Option<&(&Judged, Moved)>,
    mut answers: Answers<'a>,
) -> Result<(Judgement, Vec<Decided>, Answers<'a>), String> {
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
            landing: answer.map(|(at, _)| answers.line.landings[at].commit.clone()),
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
                    oldest: answers.line.landings[..answers.line.window]
                        .last()
                        .map(|landing| landing.commit.clone()),
                    fix: None,
                });
            }
            continue;
        };
        if !failed {
            continue;
        }
        // The culprit, when the evidence pins one, owns the red in place of the newest failure.
        let (suspect, owner) = answers.suspect(post, at)?;
        let changed_check = changed_check(answers.git, &suspect.newest_failed.landing, post);
        let landings = &answers.line.landings;
        let red = Red {
            check: post.id.clone(),
            environment: post.environment.clone(),
            paths: entry.when_paths.clone(),
            landing: landings[owner].commit.clone(),
            change: landings[owner].change.clone(),
            suspect: Some(suspect),
            fix: None,
            changed_check,
        };
        // G568: only a landing newer than the newest failure answers it, and the check is
        // pending until a run at or after that landing answers, never green before.
        let fix = fix_of(answers.git, &red, &landings[..at], post);
        if let Some(fix) = fix {
            let fix = Mark::of(fix);
            out.pending.push(Pending {
                check: red.check.clone(),
                environment: red.environment.clone(),
                paths: red.paths.clone(),
                state: PENDING_FIX,
                window,
                oldest: landings[..answers.line.window]
                    .last()
                    .map(|landing| landing.commit.clone()),
                fix: Some(fix.clone()),
            });
            // E35: until that run passes, it holds the changes on its paths.
            out.fixing.push(Red {
                fix: Some(fix),
                ..red
            });
            continue;
        }
        out.red.push(red);
    }
    Ok((out, decided, answers))
}

/// G576 (a fleet finding, 2026-10-29: a change fixed a records lint, its own post run failed
/// on the old lint, and three seats read the red as the fix not working): whether `landing`
/// changed check `post`, its definition digest (the policy entry and each adapter file's
/// object) differing from its first parent's. `false` when either policy cannot be read: it
/// only adds a sentence to the red, and never decides one.
fn changed_check(git: &Git, landing: &str, post: &crate::policy::ManifestEntry) -> bool {
    let digest = |commit: &str| {
        let policy = crate::policy::load_at(git, commit).ok().flatten()?;
        policy
            .post
            .into_iter()
            .find(|entry| entry.id == post.id && entry.environment == post.environment)
            .map(|entry| entry.definition_digest)
    };
    let Ok(parent) = git.run(&["rev-parse", "--verify", "--quiet", &format!("{landing}^1")]) else {
        return false;
    };
    match (digest(landing), digest(parent.trim())) {
        (Some(now), Some(then)) => now != then,
        _ => false,
    }
}

/// E68 (a fleet finding, 2026-10-30: the fix of one red was named the fix of two more, since
/// their suspect ranges held the merge its `Fixes:` named): the fix of `red` among `later`, the
/// landings newer than its newest failure, the oldest first. A landing that names the red alone
/// ([`Red::named_by`]) is its fix; else one that names a landing of its suspect range, when it
/// changed a file on the red's paths or changed the check. Admission is [`Red::answered_by`]
/// still: a change it lets through lands, named the fix or not.
fn fix_of<'l>(
    git: &Git,
    red: &Red,
    later: &'l [Landing],
    post: &crate::policy::ManifestEntry,
) -> Option<&'l Landing> {
    fn names(landing: &Landing) -> Vec<&String> {
        landing.names.iter().collect()
    }
    later
        .iter()
        .rev()
        .find(|landing| red.named_by(&names(landing)))
        .or_else(|| {
            later.iter().rev().find(|landing| {
                red.answered_by(&names(landing))
                    && (on_paths(git, red, &landing.commit)
                        || changed_check(git, &landing.commit, post))
            })
        })
}

/// E68: whether `landing` changed a file on `red`'s paths against its first parent; a check
/// with no `when_paths` is on every path. `false` when Git cannot compare the two.
fn on_paths(git: &Git, red: &Red, landing: &str) -> bool {
    let Ok(paths) = crate::check::changed_paths(git, &format!("{landing}^1"), landing) else {
        return false;
    };
    paths.iter().any(|path| {
        red.paths
            .as_ref()
            .is_none_or(|globs| crate::policy::globs_match(globs, path))
    })
}

/// N-20, B5: the landings a red judgement reads, newest first: the landing line of the window,
/// then, once a red check's window holds no pass, the line past it down to the reach.
struct Line {
    landings: Vec<Landing>,
    /// How many of `landings` are the window's.
    window: usize,
    /// How many first-parent commits of main `landings` covers.
    depth: usize,
    /// Whether the window read as many first-parent commits as it holds: trunk may go on
    /// below it.
    full: bool,
}

/// B5: how a red judgement reads past its window for a red check's last pass.
struct Past<'a> {
    trunk: &'a str,
    /// `[red] reach`: how many first-parent commits of main it reads back at most.
    reach: usize,
    /// Whether the landings past the window were read.
    read: bool,
    /// S4: the answers the judgement before read, and the trees whose notes moved since; `None`
    /// when there is no judgement to keep them from.
    kept: Option<(&'a Answered, &'a HashSet<String>)>,
}

/// How many landings' answers [`Answers`] reads in one batch at most: the landing asked for
/// and the unread ones older than it, which a walk to a pass asks for next.
const READ_AHEAD: usize = 64;

/// The post answers of the line's landings, each read once, in batches from the one asked.
struct Answers<'a> {
    git: &'a Git,
    registry: Option<&'a Registry>,
    line: Line,
    /// Each landing's answers where they are known (S4).
    rows: Vec<Option<Vec<Value>>>,
    /// How many landings' answers were read (S4).
    read: usize,
    past: Past<'a>,
}

impl<'a> Answers<'a> {
    fn new(
        git: &'a Git,
        registry: Option<&'a Registry>,
        line: Line,
        rows: Vec<Option<Vec<Value>>>,
        past: Past<'a>,
    ) -> Self {
        Self {
            git,
            registry,
            line,
            rows,
            read: 0,
            past,
        }
    }

    /// B5: reads the landings past the window, down to the reach, once: whether there were any.
    /// A landing the judgement before read whose notes did not move keeps its answers (S4).
    fn past(&mut self) -> Result<bool, String> {
        if self.past.read || !self.line.full || self.past.reach <= self.line.depth {
            return Ok(false);
        }
        self.past.read = true;
        let deeper = line(self.git, self.past.trunk, self.past.reach)?;
        let known = self.line.landings.len();
        // The window's line is the start of the deeper one: one commit, one walk.
        let commits = |landings: &[Landing]| -> Vec<String> {
            landings
                .iter()
                .map(|landing| landing.commit.clone())
                .collect()
        };
        if deeper.landings.get(..known).map(commits) != Some(commits(&self.line.landings)) {
            return Err(format!(
                "the landing line of {} read two ways",
                self.past.trunk
            ));
        }
        self.line.depth = deeper.depth;
        for landing in deeper.landings.into_iter().skip(known) {
            let kept = self
                .past
                .kept
                .filter(|(_, moved)| !moved.contains(&landing.tree))
                .and_then(|(answers, _)| answers.get(&landing.commit).cloned());
            self.rows.push(kept);
            self.line.landings.push(landing);
        }
        Ok(self.line.landings.len() > known)
    }

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
            // The unread landings from `at` on, older ones after it, read in one batch (a fleet
            // finding, 2026-10-29: one read a landing was six git processes, 594 for a red
            // window of 99, most of one `bay status --all`).
            let run = self.rows[at..]
                .iter()
                .take(READ_AHEAD)
                .take_while(|row| row.is_none())
                .count();
            let landings: Vec<(&str, &str)> = self.line.landings[at..at + run]
                .iter()
                .map(|landing| (landing.commit.as_str(), landing.tree.as_str()))
                .collect();
            let answers =
                crate::publish::post_answers_many(self.git, &landings, self.registry, true)?;
            self.read += run;
            for (offset, rows) in answers.into_iter().enumerate() {
                self.rows[at + offset] = Some(rows);
            }
        }
        Ok(self.rows[at].as_ref().and_then(|rows| {
            rows.iter()
                .find(|row| row["check"] == post.id && row["environment"] == post.environment)
        }))
    }

    /// G584: whether landing `at`'s base policy lists other post checks and not `post`, so the
    /// landing added it. A base with no post checks, or no valid policy, says nothing.
    fn unlisted(&mut self, at: usize, post: &crate::policy::ManifestEntry) -> Result<bool, String> {
        let listed = self.row(at, post)?.is_some();
        Ok(!listed && self.rows[at].as_ref().is_some_and(|rows| !rows.is_empty()))
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
        if self.line.full {
            return Ok(Some(PENDING_BEYOND_WINDOW));
        }
        for at in 0..self.line.window {
            if self.requires(at, post)? {
                return Ok(Some(PENDING));
            }
        }
        Ok(None)
    }

    /// NEW-6: the suspect range of `post`, red from landing `at`: older landings are read back
    /// to the newest pass, past the window down to the reach when it holds none (B5), and the
    /// oldest failure after it is the first failure. G584: the walk stops at the landing that
    /// added the check, which ends the range. With it, the landing that owns the red: the
    /// culprit when the pass sits right below the first failure, else the landing that added
    /// the check, else `at`.
    fn suspect(
        &mut self,
        post: &crate::policy::ManifestEntry,
        at: usize,
    ) -> Result<(Suspect, usize), String> {
        let mut first = at;
        let mut passed = None;
        let mut added = None;
        let mut older = at + 1;
        while older < self.line.landings.len() || self.past()? {
            if self.unlisted(older, post)? {
                added = Some(older);
                break;
            }
            match self.decides(older, post)? {
                Some(true) => first = older,
                Some(false) => {
                    passed = Some(older);
                    break;
                }
                None => {}
            }
            older += 1;
        }
        let end = passed
            .or(added.map(|added| added + 1))
            .unwrap_or(self.line.landings.len());
        // Landings are newest first: the pass right below the first failure is at `first + 1`.
        let culprit = match passed {
            Some(older) if older == first + 1 => Some(self.culprit(post, first, at, older)?),
            _ => None,
        };
        let owner = match (&culprit, added) {
            (Some(_), _) => first,
            (None, Some(added)) => added,
            (None, None) => at,
        };
        let landings = &self.line.landings;
        Ok((
            Suspect {
                passed: passed.map(|older| Mark::of(&landings[older])),
                failed: Mark::of(&landings[first]),
                landings: landings[first..end].iter().rev().map(Mark::of).collect(),
                failing: landings[at..first].iter().rev().map(Mark::of).collect(),
                newest_failed: Mark::of(&landings[at]),
                culprit,
                searched: (passed.is_none() && added.is_none()).then_some(self.line.depth),
                added: added.map(|added| Mark::of(&landings[added])),
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
                    mark: Mark::of(&self.line.landings[index]),
                    verdict,
                });
            }
        }
        Ok(Culprit {
            mark: Mark::of(&self.line.landings[first]),
            tested,
        })
    }

    /// The newest landing of the window that decides `post`, and whether it failed.
    fn newest(
        &mut self,
        post: &crate::policy::ManifestEntry,
    ) -> Result<Option<(usize, bool)>, String> {
        for at in 0..self.line.window {
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
            self.line.landings[..self.line.window]
                .iter()
                .position(|landing| &landing.commit == commit)
        });
        // A review (2026-10-28): a landing that decided before and is no longer on the landing
        // line (an outside merge moved it off) says nothing of the older landings, which may
        // still decide: the whole window is read.
        if then.landing.is_some() && carried.is_none() {
            return self.newest(post);
        }
        for at in 0..self.line.window {
            if at < moved.new || moved.trees.contains(&self.line.landings[at].tree) {
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
/// line placed at that merge.
fn line(git: &Git, trunk: &str, window: usize) -> Result<Line, String> {
    let line = crate::trunkline::walk(
        git,
        trunk,
        window,
        None,
        "%T%x1f%(trailers:key=Fixes,valueonly,separator=%x2c)%x1f%(trailers:key=Reverts,valueonly,separator=%x2c)%x1f%(trailers:key=Supersedes,valueonly,separator=%x2c)",
    )?;
    let depth = line.first_parent;
    let landings: Vec<Landing> = line
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
    Ok(Line {
        window: landings.len(),
        landings,
        depth,
        full: depth >= window,
    })
}
