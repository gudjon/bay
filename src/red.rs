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
//!   pass there, or above it, is green.
//! - NEW-6: a check no commit in the window answers is never green. When the window is full,
//!   its answer, if any, is older than the window: it is `pending_beyond_window`. When the window
//!   holds all of trunk and a landing in it requires the check, it is `pending`. Either is listed
//!   as [`Pending`], apart from the red checks: unknown, not a failure, so it refuses no landing.
//! - A red check names its `suspect` range ([`Suspect`]): the newest pass older than the failure
//!   (when the window holds one), the first failure after it, and the landings in between.
//! - Culprit finding: when the newest pass sits right below the first failure, one landing is
//!   left in the range, and the evidence names it. That landing is the `culprit`, with the
//!   answers `tested` from the pass up to the newest failure, and it owns the red in place of
//!   the newest failure. A post producer, or an executor with `[executor] bisect_post`
//!   (`crate::bisect`), narrows the range by recording runs on its midpoints.
//! - A `product_failure` is cleared by a later landing whose `Fixes:`, `Reverts:` or
//!   `Supersedes:` names the owning change, or by a quarantine of the check at trunk.
//!
//! While red, the executor refuses a candidate that touches those paths with `TRUNK_RED`,
//! naming the owner and the ways out; a change that names the owning change in `--fixes`, an
//! undo of it, or a superseding change is never refused.
//!
//! G195: `bay watch --since` judges from what its cursor carries ([`Judged`]): how each check
//! was decided, at which trunk, under which evidence tips and registry. It reads the answers of
//! the landings after that trunk and of those whose note moved (one `diff-tree` per moved
//! producer ref), so its Git work grows with what changed, not with the window. Two reads are
//! bounded by the window instead: while a check is red, its suspect range reads back to its
//! last pass; and while no landing answers a check in a window that holds all of trunk, the
//! landings are read until one requires it.

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

/// Trunk red on one post-publication check. A `bay watch` cursor carries the red checks it saw
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

/// G195: what a red judgement of `trunk` decided, carried in a `bay watch` cursor so that the
/// next judgement reads only what moved since: the landings after `trunk`, and those whose
/// evidence note changed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Judged {
    /// The trunk commit judged.
    pub trunk: String,
    /// The tip of each producer's evidence notes ref then, by producer.
    pub evidence: BTreeMap<String, String>,
    /// The registry commit the records were counted under; `None` when bay ran unsigned.
    pub registry: Option<String>,
    /// How each post check judged was decided.
    pub decided: Vec<Decided>,
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

/// The red checks of `trunk` (G244). A pending check is not among them: it refuses nothing.
///
/// # Errors
///
/// A git fault, or a committed config bay cannot read.
pub fn red(git: &Git, trunk: &str, registry: Option<&Registry>) -> Result<Vec<Red>, String> {
    Ok(state(git, trunk, registry)?.red)
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
    let landings = landings(git, trunk, window)?;
    Ok(decide(git, &policy, &landings, window, registry, None)?.0)
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
    let landings = landings(git, trunk, window)?;
    let known = before.and_then(|before| {
        moved(git, before, &evidence, registry, &landings).map(|moved| (before, moved))
    });
    let (judgement, decided) = decide(git, &policy, &landings, window, registry, known.as_ref())?;
    Ok((
        judgement,
        Some(Judged {
            trunk: trunk.to_owned(),
            evidence,
            registry: registry.map(|registry| registry.commit.clone()),
            decided,
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

/// The red and pending checks over `landings`, the newest `window` of trunk, and how each
/// check was decided. With `known`, a check `known` decided is judged from what moved; any
/// other check is judged over the window.
fn decide(
    git: &Git,
    policy: &crate::policy::Policy,
    landings: &[Landing],
    window: usize,
    registry: Option<&Registry>,
    known: Option<&(&Judged, Moved)>,
) -> Result<(Judgement, Vec<Decided>), String> {
    let mut answers = Answers {
        git,
        registry,
        landings,
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
            if let Some(state) = answers.unanswered(post, window)? {
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
        window: usize,
    ) -> Result<Option<&'static str>, String> {
        if self.landings.len() >= window {
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
) -> Option<Moved> {
    if before.registry.as_deref() != registry.map(|registry| registry.commit.as_str())
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

/// The newest `window` first-parent commits of `trunk`, newest first.
fn landings(git: &Git, trunk: &str, window: usize) -> Result<Vec<Landing>, String> {
    let window = window.to_string();
    let out = git
        .run(&[
            "log",
            "--first-parent",
            "-n",
            &window,
            "--format=%H%x1f%T%x1f%(trailers:key=Change-Id,valueonly,separator=%x2c)%x1f%(trailers:key=Fixes,valueonly,separator=%x2c)%x1f%(trailers:key=Reverts,valueonly,separator=%x2c)%x1f%(trailers:key=Supersedes,valueonly,separator=%x2c)",
            trunk,
        ])
        .map_err(|error| error.message())?;
    Ok(out
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\u{1f}').map(str::trim).collect();
            let (commit, tree, change) = (fields.first()?, fields.get(1)?, fields.get(2)?);
            let names = fields[3..]
                .iter()
                .flat_map(|field| field.split(','))
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect();
            Some(Landing {
                commit: (*commit).to_owned(),
                tree: (*tree).to_owned(),
                change: (!change.is_empty()).then(|| (*change).to_owned()),
                names,
            })
        })
        .collect())
}
