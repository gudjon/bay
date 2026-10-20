//! G244: a red trunk is owned until it is fixed, undone or quarantined (the pilot-gaps analysis
//! of 2026-10-20).
//!
//! A post-publication check (`stage = "post"`, G91) judges a landing after it is on trunk. When
//! the newest answer of such a check on trunk is a `product_failure`, trunk is **red** on that
//! check's paths (its `when_paths`, or every path), and the landing that broke it owns it. It is
//! derived from the records on each read, never stored:
//!
//! - Over the newest [`WINDOW`] first-parent commits, the newest commit with an answered record
//!   of the check decides. A pass there, or above it, is green.
//! - A `product_failure` is cleared by a later landing whose `Fixes:`, `Reverts:` or
//!   `Supersedes:` names the owning change, or by a quarantine of the check at trunk.
//!
//! While red, the executor refuses a candidate that touches those paths with `TRUNK_RED`,
//! naming the owner and the ways out; a change that names the owning change in `--fixes`, an
//! undo of it, or a superseding change is never refused.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::git::Git;
use crate::registry::Registry;

/// How many first-parent trunk commits a red judgement reads back.
pub const WINDOW: usize = 50;

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
}

impl Red {
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({"check": self.check, "environment": self.environment, "paths": self.paths,
            "landing": self.landing, "change": self.change})
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

/// One first-parent commit of trunk, with the changes its lineage trailers name.
struct Landing {
    commit: String,
    change: Option<String>,
    names: Vec<String>,
}

/// The red checks of `trunk` (G244).
///
/// # Errors
///
/// A git fault.
pub fn red(git: &Git, trunk: &str, registry: Option<&Registry>) -> Result<Vec<Red>, String> {
    let policy = match crate::policy::load_at(git, trunk) {
        Ok(Some(policy)) if !policy.post.is_empty() => policy,
        Ok(_) | Err(crate::policy::LoadError::Invalid(_)) => return Ok(Vec::new()),
        Err(crate::policy::LoadError::Git(why)) => return Err(why),
    };
    let landings = landings(git, trunk)?;
    let mut answers: Vec<Option<Vec<Value>>> = vec![None; landings.len()];
    let mut out = Vec::new();
    for post in &policy.post {
        let id = crate::evidence::CheckId::new(&post.id, &post.environment);
        let Some(entry) = policy.check(&id) else {
            continue;
        };
        if policy.is_quarantined(entry) {
            continue;
        }
        for (at, landing) in landings.iter().enumerate() {
            if answers[at].is_none() {
                answers[at] = Some(crate::publish::post_answers(
                    git,
                    &landing.commit,
                    registry,
                    true,
                )?);
            }
            let state = answers[at]
                .as_ref()
                .and_then(|rows| {
                    rows.iter().find(|row| {
                        row["check"] == post.id && row["environment"] == post.environment
                    })
                })
                .and_then(|row| row["state"].as_str())
                .unwrap_or("none")
                .to_owned();
            match state.as_str() {
                "pass" => break,
                "product_failure" => {
                    // A later landing that fixes, undoes or supersedes the owner clears it.
                    let answered = landing.change.as_ref().is_some_and(|change| {
                        landings[..at]
                            .iter()
                            .any(|later| later.names.contains(change))
                    });
                    if !answered {
                        out.push(Red {
                            check: post.id.clone(),
                            environment: post.environment.clone(),
                            paths: entry.when_paths.clone(),
                            landing: landing.commit.clone(),
                            change: landing.change.clone(),
                        });
                    }
                    break;
                }
                // Not answered on this landing yet, or not required of it: an older one decides.
                _ => {}
            }
        }
    }
    Ok(out)
}

/// The newest [`WINDOW`] first-parent commits of `trunk`, newest first.
fn landings(git: &Git, trunk: &str) -> Result<Vec<Landing>, String> {
    let window = WINDOW.to_string();
    let out = git
        .run(&[
            "log",
            "--first-parent",
            "-n",
            &window,
            "--format=%H%x1f%(trailers:key=Change-Id,valueonly,separator=%x2c)%x1f%(trailers:key=Fixes,valueonly,separator=%x2c)%x1f%(trailers:key=Reverts,valueonly,separator=%x2c)%x1f%(trailers:key=Supersedes,valueonly,separator=%x2c)",
            trunk,
        ])
        .map_err(|error| error.message())?;
    Ok(out
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\u{1f}').map(str::trim).collect();
            let (commit, change) = (fields.first()?, fields.get(1)?);
            let names = fields[2..]
                .iter()
                .flat_map(|field| field.split(','))
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect();
            Some(Landing {
                commit: (*commit).to_owned(),
                change: (!change.is_empty()).then(|| (*change).to_owned()),
                names,
            })
        })
        .collect())
}
