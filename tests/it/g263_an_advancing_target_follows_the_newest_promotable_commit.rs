//! G263 (N-7): an advancing target follows the newest promotable commit.
//!
//! A `[[target]]` with `advance = true` is moved by the executor: each `bay runner run` pass
//! fast-forwards its ref on the remote to the newest commit of its source that `bay status`
//! names promotable (`detail.targets[].promotable`), never to one whose required check is red,
//! and names the move in `detail.advanced` as `{target, from, to}`. A target promoted from
//! another advances from that target's tip as the same pass left it. A target without `advance`
//! is never moved by the executor.
//!
//! The guards: `if !target.advance` in `crate::target::advance` (make it `if false`, and the
//! second case's `qa` moves), and `if failed.is_empty()` in `Read::promotable` of
//! `crate::target` (make it `if true`, and the first pass picks the red landing, which the
//! re-judgement refuses, so `staging` never reaches the green one).

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// A post-publication check every landing wakes: the suite a runner records after the merge.
const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n";

/// The `staging` target, requiring the post check, which the executor advances.
const ADVANCING: &str =
    "trunk = \"main\"\n\n[[target]]\nname = \"staging\"\nrequires = [\"suite\"]\nadvance = true\n";

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

    /// One executor pass: its `detail.advanced`, `null` when it moved nothing.
    fn pass(&self) -> Value {
        let ran = self
            .test_box
            .bay_ok(&self.repo, &["runner", "run", "--once"]);
        ran["detail"]["advanced"].clone()
    }

    /// The commit the remote's `refs/heads/<name>` names, `None` when it holds no such ref.
    fn remote_tip(&self, name: &str) -> Option<String> {
        let listed = self.test_box.git_ok(
            &self.repo,
            &["ls-remote", "origin", &format!("refs/heads/{name}")],
        );
        listed.split_whitespace().next().map(str::to_owned)
    }
}

/// The one move of `name` in a pass's `detail.advanced`.
fn moved<'a>(advanced: &'a Value, name: &str) -> &'a Value {
    advanced
        .as_array()
        .unwrap_or_else(|| panic!("no detail.advanced: {advanced}"))
        .iter()
        .find(|row| row["target"] == name)
        .unwrap_or_else(|| panic!("no move of {name}: {advanced}"))
}

#[test]
fn an_advancing_target_moves_to_the_newest_green_landing_and_never_to_a_red_one() {
    let hosted = hosted(ADVANCING);
    let (_, green) = hosted.land("src/green.rs", None);
    let (broke, red) = hosted.land("src/red.rs", None);
    hosted.suite(&green, "pass");
    hosted.suite(&red, "product_failure");
    assert_eq!(hosted.remote_tip("staging"), None);

    // One pass: staging is created at the green landing, below the red tip of trunk.
    let advanced = hosted.pass();
    let row = moved(&advanced, "staging");
    assert_eq!(row["from"], Value::Null, "{advanced}");
    assert_eq!(row["to"], green.as_str(), "{advanced}");
    assert_eq!(row["error"], Value::Null, "{advanced}");
    assert_eq!(
        hosted.remote_tip("staging").as_deref(),
        Some(green.as_str())
    );

    // Nothing newer is green: the next pass moves nothing.
    assert_eq!(hosted.pass(), Value::Null);
    assert_eq!(
        hosted.remote_tip("staging").as_deref(),
        Some(green.as_str())
    );

    // A fix lands and its suite passes: the next pass fast-forwards staging to it.
    let (_, fixed) = hosted.land("src/fix.rs", Some(&broke));
    hosted.suite(&fixed, "pass");
    let advanced = hosted.pass();
    let row = moved(&advanced, "staging");
    assert_eq!(row["from"], green.as_str(), "{advanced}");
    assert_eq!(row["to"], fixed.as_str(), "{advanced}");
    assert_eq!(row["error"], Value::Null, "{advanced}");
    assert_eq!(
        hosted.remote_tip("staging").as_deref(),
        Some(fixed.as_str())
    );
}

#[test]
fn a_target_without_advance_stays_and_a_chained_target_follows_its_source() {
    // `production` is listed before its source: the pass still moves `staging` first.
    let config = "trunk = \"main\"\n\n\
                  [[target]]\nname = \"production\"\nfrom = \"staging\"\nrequires = [\"suite\"]\nadvance = true\n\n\
                  [[target]]\nname = \"staging\"\nrequires = [\"suite\"]\nadvance = true\n\n\
                  [[target]]\nname = \"qa\"\nrequires = [\"suite\"]\n";
    let hosted = hosted(config);
    let (_, landed) = hosted.land("src/one.rs", None);
    hosted.suite(&landed, "pass");

    let advanced = hosted.pass();
    assert_eq!(
        moved(&advanced, "staging")["to"],
        landed.as_str(),
        "{advanced}"
    );
    assert_eq!(
        moved(&advanced, "production")["to"],
        landed.as_str(),
        "{advanced}"
    );
    assert!(
        advanced
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["target"] != "qa"),
        "{advanced}"
    );
    assert_eq!(
        hosted.remote_tip("staging").as_deref(),
        Some(landed.as_str())
    );
    assert_eq!(
        hosted.remote_tip("production").as_deref(),
        Some(landed.as_str())
    );
    // `qa` is promotable, and only an operator moves it.
    assert_eq!(hosted.remote_tip("qa"), None);
    let status = hosted.test_box.bay_ok(&hosted.repo, &["status"]);
    let qa = status["detail"]["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "qa")
        .unwrap_or_else(|| panic!("{status}"))
        .clone();
    assert_eq!(qa["promotable"]["commit"], landed.as_str(), "{qa}");
}
