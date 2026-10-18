//! Terminal records (`DESIGN` § 5, § 9a): `refs/bay/log/<executor>`, one append-only
//! chain per executor, no shared tip. Each commit's tree holds `record.json`, a compact
//! terminal record: op id, request digest, outcome, published commit, evidence and policy ids.
//! It outlives the change's drafts, so `status` explains an outcome after retention and gc.

use serde_json::Value;

use crate::change::RECORD_IDENTITY;
use crate::git::Git;
use crate::repo::Repo;
use crate::signing::Signer;

pub const LOG_PREFIX: &str = "refs/bay/log/";

#[must_use]
pub fn log_ref(executor: &str) -> String {
    format!("{LOG_PREFIX}{executor}")
}

/// Fetches every executor's log from the remote (best effort: offline keeps what is local).
pub fn fetch(repo: &Repo) {
    let refspec = format!("+{LOG_PREFIX}*:{LOG_PREFIX}*");
    let _ = repo
        .git
        .run(&["fetch", "-q", "--no-tags", repo.remote(), &refspec]);
}

/// Appends `record` to this executor's log and pushes it. Only this executor writes its log.
///
/// # Errors
///
/// A git fault while writing locally; a failed push is retried by the next append.
pub fn append(
    repo: &Repo,
    executor: &str,
    signer: Option<&Signer>,
    record: &Value,
) -> Result<String, String> {
    let git = &repo.git;
    let name = log_ref(executor);
    let _ = git.run(&[
        "fetch",
        "-q",
        "--no-tags",
        repo.remote(),
        &format!("{name}:{name}"),
    ]);
    let mut json = serde_json::to_string_pretty(record).map_err(|error| error.to_string())?;
    json.push('\n');
    let blob = git
        .run_with(
            &["hash-object", "-w", "--stdin"],
            &[],
            Some(json.as_bytes()),
        )
        .map_err(|error| error.message())?;
    let entry = format!("100644 blob {blob}\trecord.json\n");
    let tree = git
        .run_with(&["mktree"], &[], Some(entry.as_bytes()))
        .map_err(|error| error.message())?;
    let parent = git.ref_target(&name).map_err(|error| error.message())?;
    let mut args: Vec<String> = Vec::new();
    if let Some(signer) = signer {
        args.extend(signer.config_args());
    }
    args.extend(["commit-tree".to_owned(), tree]);
    if let Some(parent) = &parent {
        args.extend(["-p".to_owned(), parent.clone()]);
    }
    args.push(
        if signer.is_some() {
            "-S"
        } else {
            "--no-gpg-sign"
        }
        .to_owned(),
    );
    args.extend([
        "-m".to_owned(),
        format!(
            "terminal record {}",
            record["op_id"].as_str().unwrap_or_default()
        ),
    ]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let identity: Vec<(String, String)> = signer.map_or_else(
        || {
            RECORD_IDENTITY
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect()
        },
        Signer::identity,
    );
    let envs: Vec<(&str, &str)> = identity
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let commit = git
        .run_with(&args, &envs, None)
        .map_err(|error| error.message())?;
    let update = match &parent {
        Some(parent) => format!("update {name} {commit} {parent}\n"),
        None => format!("create {name} {commit}\n"),
    };
    git.run_with(&["update-ref", "--stdin"], &[], Some(update.as_bytes()))
        .map_err(|error| error.message())?;
    let _ = git.run(&["push", "-q", repo.remote(), &format!("{name}:{name}")]);
    Ok(commit)
}

/// Every terminal record for change `id`, from every executor log this repository holds.
///
/// # Errors
///
/// A git fault.
pub fn records_for(git: &Git, id: &str) -> Result<Vec<Value>, String> {
    let logs = git
        .run(&["for-each-ref", "--format=%(refname)", LOG_PREFIX])
        .map_err(|error| error.message())?;
    let mut out = Vec::new();
    for name in logs.lines() {
        let commits = git
            .run(&["rev-list", name])
            .map_err(|error| error.message())?;
        for commit in commits.lines() {
            let Ok(text) = git.run(&["cat-file", "blob", &format!("{commit}:record.json")]) else {
                continue;
            };
            if let Ok(record) = serde_json::from_str::<Value>(&text)
                && record["change"] == id
            {
                out.push(record);
            }
        }
    }
    Ok(out)
}
