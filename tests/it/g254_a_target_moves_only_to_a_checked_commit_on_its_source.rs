//! G254 (NEW-5, NEW-9; owner ruling 2026-10-20): a target moves only to a checked commit on its
//! source.
//!
//! A `[[target]]` (`staging`) is a ref an operator moves to a commit already on its source, the
//! trunk. `bay status` names, under `detail.targets`, its tip, how far it lags, the commits on
//! it the trunk does not reach (`drift`), its required checks and the newest promotable commit.
//! On a bay host the hook refuses, with `PROMOTION_REFUSED`, a push that moves it to a commit off
//! the trunk's first-parent line, or one whose required post check has not passed; a recorded
//! pass makes the same push succeed. While a `[[hold]]` is in force only a landing that
//! carries `Fixes:` is promoted. On a forge, which enforces nothing, the same reading detects a
//! target moved off the trunk as drift, and `bay status --since` names the move.

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::talk::from_now;
use support::{TestBox, check_files, passing_result, result_json, unchecked_files};

/// A post-publication check every landing wakes: the suite a runner records after the merge.
const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n";

/// The `staging` target, requiring the post check.
const STAGING: &str =
    "trunk = \"main\"\n\n[[target]]\nname = \"staging\"\nrequires = [\"suite\"]\n";

struct Hosted {
    test_box: TestBox,
    repo: PathBuf,
}

/// A bay host with a registry and trunk seeded under a policy with a pre check `tests` and the
/// post check `suite`, and `config` committed on trunk. The repository's clone holds the agent,
/// executor and producer keys.
fn hosted(config: &str) -> Hosted {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    let mut files = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(POST);
        }
    }
    files.push((".bay/config.toml".to_owned(), config.to_owned()));
    // Trunk is seeded before the registry exists: from then on only executors publish.
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    Hosted { test_box, repo }
}

impl Hosted {
    /// Starts a change writing `path` (`--fixes <fixes>` when given), lands it and returns
    /// (change, landed commit).
    fn land(&self, path: &str, fixes: Option<&str>) -> (String, String) {
        let intent = format!("CARD-{}", path.replace(['/', '.'], "-"));
        let mut args = vec!["start", intent.as_str(), "--paths", "src/"];
        if let Some(fixed) = fixes {
            args.extend(["--fixes", fixed]);
        }
        let started = self.test_box.bay_ok(&self.repo, &args);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(path), format!("// {path}\n")).unwrap();
        let (code, landed) = result_json(self.test_box.bay(&view).args(["merge", "--wait"]));
        assert_eq!(code, 0, "{landed}");
        let commit = landed["detail"]["commit"].as_str().unwrap().to_owned();
        (id, commit)
    }

    /// Records the post check `suite` as `verdict` for `commit`, signed by the producer key.
    fn suite(&self, commit: &str, verdict: &str) {
        let (code, written) = result_json(self.test_box.bay(&self.repo).args([
            "evidence",
            "record",
            "--kind",
            "check",
            "--check",
            "suite",
            "--environment",
            "local",
            "--verdict",
            verdict,
            "--commit",
            commit,
        ]));
        assert_eq!(code, 0, "{written}");
    }

    fn staging(&self) -> Value {
        target(&self.test_box, &self.repo, "staging")
    }
}

/// The `detail.targets` row of `name` in `bay status`.
fn target(test_box: &TestBox, repo: &Path, name: &str) -> Value {
    let status = test_box.bay_ok(repo, &["status"]);
    status["detail"]["targets"]
        .as_array()
        .unwrap_or_else(|| panic!("no detail.targets: {status}"))
        .iter()
        .find(|row| row["name"] == name)
        .unwrap_or_else(|| panic!("no target {name}: {status}"))
        .clone()
}

/// Pushes `refspec` from `repo`: whether the remote took it, and its stderr.
fn push(test_box: &TestBox, repo: &Path, refspec: &str) -> (bool, String) {
    let out = test_box
        .git(repo)
        .args(["push", "origin", refspec])
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A commit on `parent` with `parent`'s tree, made here and on no branch: not on the trunk.
fn side_commit(test_box: &TestBox, repo: &Path, parent: &str) -> String {
    test_box.git_ok(
        repo,
        &[
            "commit-tree",
            &format!("{parent}^{{tree}}"),
            "-p",
            parent,
            "-m",
            "a hotfix made straight on the target",
        ],
    )
}

#[test]
fn a_bay_host_promotes_only_a_checked_commit_on_trunk() {
    let hosted = hosted(STAGING);
    let (test_box, repo) = (&hosted.test_box, &hosted.repo);
    let (_, first) = hosted.land("src/first.rs", None);

    // No target ref yet, and nothing promotable: the suite has not answered for the landing.
    let row = hosted.staging();
    assert_eq!(row["ref"], "refs/heads/staging", "{row}");
    assert_eq!(row["from"], "main", "{row}");
    assert_eq!(row["tip"], Value::Null, "{row}");
    assert_eq!(row["from_tip"], first.as_str(), "{row}");
    assert_eq!(row["promotable"]["commit"], Value::Null, "{row}");
    let blocked = &row["promotable"]["blocked_by"][0];
    assert_eq!(blocked["rule"], "check", "{row}");
    assert_eq!(blocked["check"], "suite", "{row}");
    assert_eq!(blocked["state"], "pending", "{row}");

    // A commit not on trunk is refused, naming the rule.
    let side = side_commit(test_box, repo, &first);
    let (accepted, refused) = push(test_box, repo, &format!("{side}:refs/heads/staging"));
    assert!(!accepted, "{refused}");
    assert!(refused.contains("PROMOTION_REFUSED"), "{refused}");
    assert!(
        refused.contains("is not on the first-parent line of main"),
        "{refused}"
    );

    // A trunk commit whose required post check has not passed is refused.
    let (accepted, refused) = push(test_box, repo, &format!("{first}:refs/heads/staging"));
    assert!(!accepted, "{refused}");
    assert!(refused.contains("PROMOTION_REFUSED"), "{refused}");
    assert!(refused.contains("check suite is pending"), "{refused}");

    // A recorded pass makes it promotable, and the same push lands.
    hosted.suite(&first, "pass");
    let row = hosted.staging();
    assert_eq!(row["promotable"]["commit"], first.as_str(), "{row}");
    assert_eq!(row["promotable"]["behind_from"], 0, "{row}");
    let (accepted, said) = push(test_box, repo, &format!("{first}:refs/heads/staging"));
    assert!(accepted, "{said}");

    let row = hosted.staging();
    assert_eq!(row["tip"], first.as_str(), "{row}");
    assert_eq!(row["on_from"], true, "{row}");
    assert_eq!(row["lag"], 0, "{row}");
    assert_eq!(row["drift"]["count"], 0, "{row}");
    assert_eq!(row["checks"][0]["state"], "pass", "{row}");
    assert_eq!(row["tip_failures"], serde_json::json!([]), "{row}");
    assert_eq!(row["promotable"]["at_from"], true, "{row}");

    // Trunk moves on: the target lags by one, and the new landing waits for its suite.
    let (_, second) = hosted.land("src/second.rs", None);
    let row = hosted.staging();
    assert_eq!(row["lag"], 1, "{row}");
    assert_eq!(row["from_tip"], second.as_str(), "{row}");
    assert_eq!(row["promotable"]["commit"], Value::Null, "{row}");
    hosted.suite(&second, "product_failure");
    let (accepted, refused) = push(test_box, repo, &format!("{second}:refs/heads/staging"));
    assert!(!accepted, "{refused}");
    assert!(
        refused.contains("check suite is product_failure"),
        "{refused}"
    );
    // A move back is no fast-forward.
    let (accepted, refused) = push(test_box, repo, &format!("+{first}^:refs/heads/staging"));
    assert!(!accepted, "{refused}");
    assert!(refused.contains("does not fast-forward"), "{refused}");
}

#[test]
fn a_freeze_refuses_a_plain_promotion_and_takes_a_fix() {
    let config = format!(
        "{STAGING}\n[[hold]]\ntarget = \"staging\"\nend = \"{}\"\nreason = \"release week\"\n",
        from_now(3600)
    );
    let hosted = hosted(&config);
    let (test_box, repo) = (&hosted.test_box, &hosted.repo);
    let (broke, plain) = hosted.land("src/plain.rs", None);
    hosted.suite(&plain, "pass");

    let row = hosted.staging();
    assert_eq!(row["freeze"]["reason"], "release week", "{row}");
    assert_eq!(row["promotable"]["commit"], Value::Null, "{row}");
    assert_eq!(
        row["promotable"]["blocked_by"][0]["rule"], "freeze",
        "{row}"
    );
    let (accepted, refused) = push(test_box, repo, &format!("{plain}:refs/heads/staging"));
    assert!(!accepted, "{refused}");
    assert!(refused.contains("PROMOTION_REFUSED"), "{refused}");
    assert!(refused.contains("is frozen until"), "{refused}");

    // A landing that carries `Fixes:` is promoted through the freeze, once its suite passed.
    let (_, fix) = hosted.land("src/fix.rs", Some(&broke));
    hosted.suite(&fix, "pass");
    assert_eq!(hosted.staging()["promotable"]["commit"], fix.as_str());
    let (accepted, said) = push(test_box, repo, &format!("{fix}:refs/heads/staging"));
    assert!(accepted, "{said}");
    assert_eq!(hosted.staging()["tip"], fix.as_str());
}

#[test]
fn on_a_forge_a_target_moved_off_trunk_is_drift_and_watch_names_it() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let mut files = unchecked_files();
    files.push((
        ".bay/config.toml".to_owned(),
        "trunk = \"main\"\n\n[[target]]\nname = \"staging\"\n\n[[target]]\nname = \"production\"\nfrom = \"staging\"\nsoak_s = 3600\n".to_owned(),
    ));
    let seeded = test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);

    // The forge takes any push: a hotfix made straight on staging.
    let side = side_commit(&test_box, &repo, &seeded);
    let (accepted, said) = push(&test_box, &repo, &format!("{side}:refs/heads/staging"));
    assert!(accepted, "{said}");
    let row = target(&test_box, &repo, "staging");
    assert_eq!(row["tip"], side.as_str(), "{row}");
    assert_eq!(row["on_from"], false, "{row}");
    assert_eq!(row["drift"]["count"], 1, "{row}");
    assert_eq!(row["drift"]["commits"], serde_json::json!([side]), "{row}");
    assert_eq!(row["tip_failures"][0]["rule"], "not_on_from", "{row}");
    assert_eq!(
        row["promotable"]["blocked_by"][0]["rule"], "not_fast_forward",
        "{row}"
    );
    let watched = test_box.bay_ok(&repo, &["status", "--since"]);
    let events = watched["detail"]["events"].as_array().unwrap();
    let unpromotable = events
        .iter()
        .find(|event| event["kind"] == "target_unpromotable")
        .unwrap_or_else(|| panic!("{watched}"));
    assert_eq!(unpromotable["target"], "staging", "{unpromotable}");
    assert_eq!(unpromotable["commit"], side.as_str(), "{unpromotable}");
    let cursor = watched["detail"]["cursor"].as_str().unwrap().to_owned();

    // Moved back onto trunk: no drift, and the watch from the cursor names the move.
    let (accepted, said) = push(&test_box, &repo, &format!("+{seeded}:refs/heads/staging"));
    assert!(accepted, "{said}");
    let row = target(&test_box, &repo, "staging");
    assert_eq!(row["on_from"], true, "{row}");
    assert_eq!(row["drift"]["count"], 0, "{row}");
    let watched = test_box.bay_ok(&repo, &["status", "--since", &cursor]);
    let moved = watched["detail"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "target_moved")
        .unwrap_or_else(|| panic!("{watched}"))
        .clone();
    assert_eq!(moved["commit"], seeded.as_str(), "{moved}");

    // Production is promoted from staging, and only after its soak.
    let row = target(&test_box, &repo, "production");
    assert_eq!(row["from"], "staging", "{row}");
    assert_eq!(row["from_tip"], seeded.as_str(), "{row}");
    assert_eq!(row["promotable"]["commit"], Value::Null, "{row}");
    assert_eq!(row["promotable"]["blocked_by"][0]["rule"], "soak", "{row}");
}
