//! `bay status --green-tip` (G926): the newest landing of main at which every post check is
//! green, for a promotion that must never pick a red tip (a pre-deploy gate).
//!
//! It fetches main and the evidence notes, then judges main's `[red] window` with the red
//! judgement's own answers ([`crate::red::green_tip`]): a landing is green when each post check,
//! read from it down the line, last answered pass, and no landing between requires the check
//! unanswered. A red, pending or flaky check is never green. `detail.green_tip` is `null` when no
//! landing of the window is green.

use std::path::Path;

use serde_json::json;

use crate::repo::Repo;
use crate::result::BayResult;

#[must_use]
pub fn green_tip(dir: &Path) -> BayResult {
    run(dir).unwrap_or_else(BayResult::failed)
}

fn run(dir: &Path) -> Result<BayResult, String> {
    let repo = Repo::open(dir)?;
    let git = &repo.git;
    let trunk = repo.fetch_trunk().map_err(|error| error.message())?;
    repo.fetch_evidence_once(&crate::signing::Keys::load(git));
    let registry = crate::registry::Registry::current(git)?;
    let tip = crate::red::green_tip(git, &trunk, registry.as_ref())?;
    Ok(BayResult::ok(json!({
        "trunk": trunk,
        "window": crate::red::window(git, &trunk)?,
        "green_tip": tip.map(|tip| json!({
            "commit": tip.commit,
            "change": tip.change,
            "above": tip.above,
        })),
    })))
}
