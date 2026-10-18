//! bay's own gate, recorded as bay evidence: `scripts/gate.sh` runs each check on the exact tree
//! it pushes and records a producer-signed `bay.evidence/1` record of it on that tree, under
//! `refs/notes/bay/evidence/<producer>`. A guarantee is tested when its G-test passed in a gate
//! whose records verify (`AGENTS.md`).
//!
//! ```text
//! gate_evidence record <producer> <key> <check> <environment>   a pass of <check> on HEAD's tree
//! gate_evidence verify <allowed-signers> [<commit>] <check>...   every <check> has a verified pass
//! ```

use std::path::Path;
use std::process::ExitCode;

use bay::evidence::{self, CheckId, EvidenceRecord, Producer, Subject, Verdict};
use bay::git::Git;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let git = Git::new(Path::new("."));
    let result = match args.first().map(String::as_str) {
        Some("record") if args.len() == 5 => record(&git, &args[1], &args[2], &args[3], &args[4]),
        Some("verify") if args.len() >= 3 => verify(&git, &args[1], &args[2..]),
        _ => Err("usage: gate_evidence record <producer> <key> <check> <environment> | verify <allowed-signers> [<commit>] <check>...".to_owned()),
    };
    match result {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(why) => {
            eprintln!("gate_evidence: {why}");
            ExitCode::FAILURE
        }
    }
}

fn rev(git: &Git, name: &str) -> Result<String, String> {
    git.run(&["rev-parse", "--verify", name])
        .map_err(|error| error.message())
}

fn record(
    git: &Git,
    producer: &str,
    key: &str,
    check: &str,
    environment: &str,
) -> Result<String, String> {
    bay::names::operator_name(producer)?;
    let commit = rev(git, "HEAD")?;
    let tree = rev(git, "HEAD^{tree}")?;
    let base_tree = rev(git, "HEAD^^{tree}").unwrap_or_default();
    let mut record = EvidenceRecord {
        schema: evidence::EVIDENCE_SCHEMA.to_owned(),
        id: format!("{producer}/{check}-{environment}-{}", &commit[..12]),
        kind: "check".to_owned(),
        subject: Subject {
            tree,
            commit,
            base_tree,
        },
        check: CheckId {
            id: check.to_owned(),
            environment: environment.to_owned(),
        },
        // bay's own repository has no acceptance policy; the gate is its policy.
        policy_revision: 0,
        manifest_digest: "gate".to_owned(),
        producer: Producer {
            name: producer.to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        verdict: Verdict::Pass,
        at: now_rfc3339(git)?,
        why: None,
        units: Vec::new(),
        sandbox: None,
        signature: None,
    };
    record.sign(Path::new(key))?;
    evidence::append(git, producer, &record)?;
    Ok(record.id)
}

fn verify(git: &Git, allowed: &str, rest: &[String]) -> Result<String, String> {
    // An optional commit comes first; everything after it names a check.
    let (commit, checks) = match rest.split_first() {
        Some((first, checks)) if rev(git, &format!("{first}^{{commit}}")).is_ok() => {
            (rev(git, &format!("{first}^{{commit}}"))?, checks)
        }
        _ => (rev(git, "HEAD")?, rest),
    };
    let tree = rev(git, &format!("{commit}^{{tree}}"))?;
    let records = evidence::records_for(git, &tree)?;
    let mut missing = Vec::new();
    for check in checks {
        let verified = records.iter().any(|record| {
            record.check.id == *check
                && record.verdict == Verdict::Pass
                && record.subject.tree == tree
                && record.verifies(Path::new(allowed))
        });
        if !verified {
            missing.push(check.as_str());
        }
    }
    if missing.is_empty() {
        Ok(format!(
            "{commit}: a verified pass for {} on tree {tree}",
            checks.join(", ")
        ))
    } else {
        Err(format!(
            "{commit}: no verified pass for {} on tree {tree}",
            missing.join(", ")
        ))
    }
}

/// The current time as RFC 3339 UTC, from `date`, so the example adds no dependency.
fn now_rfc3339(git: &Git) -> Result<String, String> {
    let _ = git;
    let out = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .map_err(|error| error.to_string())?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}
