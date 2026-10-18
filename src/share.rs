//! `bay share`: hand a change and its evidence to another actor (`DESIGN` § 10a).
//!
//! Pushes `refs/bay/changes/<id>` with every evidence notes ref to a remote, or writes them
//! to a bundle. The receiver fetches them with plain Git and opens the change with
//! `bay start --resume <id>`.

use std::path::Path;

use serde_json::json;

use crate::change::{Chain, change_ref};
use crate::evidence::EVIDENCE_PREFIX;
use crate::ids::is_change_id;
use crate::repo::Repo;
use crate::result::BayResult;

#[must_use]
pub fn share(dir: &Path, id: &str, bundle: Option<&Path>, remote: Option<&str>) -> BayResult {
    run(dir, id, bundle, remote).unwrap_or_else(BayResult::failed)
}

fn run(
    dir: &Path,
    id: &str,
    bundle: Option<&Path>,
    remote: Option<&str>,
) -> Result<BayResult, String> {
    if !is_change_id(id) {
        return Err(format!("{id} is not a change id"));
    }
    let repo = Repo::open(dir)?;
    if Chain::new(&repo.git)
        .head(id)
        .map_err(|error| error.message())?
        .is_none()
    {
        return Err(format!("change {id} is not in this repository"));
    }
    let evidence = repo
        .git
        .run(&["for-each-ref", "--format=%(refname)", EVIDENCE_PREFIX])
        .map_err(|error| error.message())?;
    let mut refs = vec![change_ref(id)];
    refs.extend(evidence.lines().map(str::to_owned));

    if let Some(bundle) = bundle {
        let path = bundle.display().to_string();
        let mut args = vec!["bundle", "create", "-q", path.as_str()];
        args.extend(refs.iter().map(String::as_str));
        repo.git.run(&args).map_err(|error| error.message())?;
        return Ok(BayResult::ok(json!({"bundle": path, "refs": refs})).with_change(id));
    }

    let remote = remote.unwrap_or_else(|| repo.remote()).to_owned();
    let refspecs: Vec<String> = refs.iter().map(|name| format!("{name}:{name}")).collect();
    let mut args = vec!["push", "-q", remote.as_str()];
    args.extend(refspecs.iter().map(String::as_str));
    repo.git.run(&args).map_err(|error| error.message())?;
    Ok(BayResult::ok(json!({"remote": remote, "refs": refs})).with_change(id))
}
