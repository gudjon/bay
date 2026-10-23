//! G167. A policy refuses a key it does not know.
//!
//! `.bay/policy.toml` is judged at the accepted base, and a key bay does not read is a
//! criterion nobody applies: a typo (`stagee`), or a key a newer bay reads, on an older bay. So
//! an unknown key or table at any level makes the policy invalid, `POLICY_INVALID` naming the
//! key and its table, as `.bay/config.toml` already refuses one. Unknown is never pass: a newer
//! policy on an older bay is refused, never misread. A candidate that adds an unknown key to
//! its own policy is refused the same way, naming the candidate's policy.

// The landing cases run shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use bay::git::Git;
use bay::policy::{LoadError, load_at};
use support::{TestBox, check_files, passing_result, result_json};

/// The one `[[check]]` entry every policy here has, with `extra` lines.
fn check(extra: &str) -> String {
    format!(
        "\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n{extra}"
    )
}

/// A trunk whose policy is `policy` and whose check passes, pushed.
fn trunk(policy: &str) -> (TestBox, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[
            ("src/app.txt", "app\n"),
            ("scripts/api-version", "#!/bin/sh\necho 1\n"),
        ],
        &[
            (
                "scripts/bay-check".to_owned(),
                check_files("true", &passing_result(1), 1, 60)[0].1.clone(),
            ),
            (".bay/policy.toml".to_owned(), policy.to_owned()),
        ],
    );
    (test_box, repo)
}

fn start(test_box: &TestBox, repo: &Path, intent: &str) -> PathBuf {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", "src/", ".bay/"]);
    PathBuf::from(started["detail"]["view"].as_str().unwrap())
}

/// The policy at trunk's tip as `bay::policy::load_at` reads it.
fn load(repo: &Path) -> Result<Option<bay::policy::Policy>, LoadError> {
    load_at(&Git::new(repo), "HEAD")
}

fn invalid(repo: &Path) -> String {
    match load(repo) {
        Err(LoadError::Invalid(why)) => why,
        Ok(_) => panic!("the policy loaded"),
        Err(LoadError::Git(why)) => panic!("git fault: {why}"),
    }
}

#[test]
fn a_misspelled_check_key_refuses_the_landing_naming_it() {
    // `stagee = "post"` read as nothing would make a check meant for after publication block
    // every landing, or one meant for before it run never: the author's intent is unknown.
    let (test_box, repo) = trunk(&format!("revision = 1\n{}", check("stagee = \"post\"\n")));
    let view = start(&test_box, &repo, "TYPO-1");
    std::fs::write(view.join("src/app.txt"), "app two\n").unwrap();
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "POLICY_INVALID", "{refused}");
    assert_eq!(refused["detail"]["policy"], "base", "{refused}");
    let why = refused["detail"]["why"].as_str().unwrap_or_default();
    assert!(why.contains("`stagee`"), "{refused}");
    assert!(why.contains("[[check]]"), "{refused}");
}

#[test]
fn an_unknown_key_or_table_at_any_level_makes_the_policy_invalid() {
    let base = format!("revision = 1\n{}", check(""));
    for (text, key, table) in [
        // An unknown top-level table, and a top-level key.
        (
            format!("{base}\n[checks]\nid = \"x\"\n"),
            "checks",
            "the top level",
        ),
        (
            format!("revisions = 2\n{base}"),
            "revisions",
            "the top level",
        ),
        (format!("{base}stagee = \"post\"\n"), "stagee", "[[check]]"),
        (
            format!(
                "{base}\n[[quarantine]]\nid = \"tests\"\nenvironment = \"local\"\nreason = \"red\"\nend = \"2099-01-01\"\nuntil = \"2099-01-01\"\n"
            ),
            "until",
            "[[quarantine]]",
        ),
        (
            format!(
                "{base}\n[[input]]\nname = \"contract:api\"\nprovider = \"scripts/api-version\"\nadapter = [\"scripts/api-version\"]\nphse = \"expand\"\n"
            ),
            "phse",
            "[[input]]",
        ),
        (
            format!("{base}\n[profile.strict]\nrequire = [\"merged\"]\nrequire_sigend = true\n"),
            "require_sigend",
            "[profile.strict]",
        ),
        (
            format!("{base}\n[agent]\nrequire_actors = true\n"),
            "require_actors",
            "[agent]",
        ),
        (
            format!("{base}\n[agent.extra]\nkey = 1\n"),
            "extra",
            "[agent]",
        ),
    ] {
        let (_box, repo) = trunk(&text);
        let why = invalid(&repo);
        assert!(why.contains(&format!("`{key}`")), "{text}\n{why}");
        assert!(why.contains(table), "{text}\n{why}");
    }
}

#[test]
fn every_documented_key_is_accepted() {
    let text = format!(
        "revision = 3\n{}{}\n[[quarantine]]\nid = \"later\"\nenvironment = \"local\"\nreason = \"red on trunk\"\nend = \"2099-01-01\"\n\n[[input]]\nname = \"contract:api\"\nprovider = \"scripts/api-version\"\nadapter = [\"scripts/api-version\"]\npath = [\"/usr/bin\", \"/bin\"]\nphase = \"expand\"\ntimeout_s = 30\n\n[profile.strict]\nrequire = [\"merged\", \"checked\"]\nrequire_signed = true\n\n[agent]\nrequire_actor = true\n",
        check(
            "timeout_s = 60\ninputs = [\"commit\"]\nfixtures = [\"data@sha256:0000000000000000000000000000000000000000000000000000000000000000\"]\npath = [\"/usr/bin\", \"/bin\"]\nenv = [\"DATABASE_URL\"]\npaths = [\"src/\"]\nmaterialize_timeout_s = 300\nnetwork = true\nstage = \"pre\"\nwhen_paths = [\"src/**\"]\n"
        ),
        check("stage = \"post\"\n").replace("id = \"tests\"", "id = \"later\""),
    );
    let (_box, repo) = trunk(&text);
    let policy = load(&repo)
        .unwrap_or_else(|error| panic!("{}\n{text}", String::from(error)))
        .unwrap();
    assert_eq!(policy.revision, 3);
    assert!(policy.agent.require_actor);
    assert_eq!(policy.inputs.len(), 1);
    assert_eq!(policy.checks.len(), 2);
    // The unchecked form, its one key beside `revision`.
    let (_box, repo) = trunk("revision = 1\nunchecked = true\n");
    assert!(load(&repo).unwrap().unwrap().unchecked);
}

#[test]
fn a_candidate_that_adds_an_unknown_key_to_its_own_policy_is_refused() {
    // The candidate's policy is the next policy once it lands: an unknown key in it is refused
    // now, naming the candidate, so trunk never holds a policy that bay cannot read.
    let (test_box, repo) = trunk(&format!("revision = 1\n{}", check("")));
    let view = start(&test_box, &repo, "NEXT-1");
    std::fs::write(
        view.join(".bay/policy.toml"),
        format!("revision = 2\n{}", check("network_access = true\n")),
    )
    .unwrap();
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "POLICY_INVALID", "{refused}");
    assert_eq!(refused["detail"]["policy"], "candidate", "{refused}");
    let why = refused["detail"]["why"].as_str().unwrap_or_default();
    assert!(why.contains("`network_access`"), "{refused}");
}
