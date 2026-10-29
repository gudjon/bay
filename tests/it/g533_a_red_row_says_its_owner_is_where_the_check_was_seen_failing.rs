//! G533 (bay 0.6.1, BACKLOG 121; a fleet finding, 2026-10-28 22:48Z: a red row of scripts-fast
//! named a change the check never judged, and readers took it as the cause). Without a culprit,
//! a red row's owner and its first failure are only where the check was seen failing, never a
//! proven cause: the row's `say` says so, and names how many merges since the last pass the
//! cause is among. Those merges stay in the range whether they touch the check's paths or not,
//! because a cause can sit in a file the check does not list (both of the night's causes did).
//!
//! Guards to mutate, in `src/red.rs`: `Red::say` without its "not a proven cause" clause; the
//! suspect range (`Answers::suspect`) kept to the merges on the check's paths.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::talk::fresh_key;
use support::{TestBox, check_files, passing_result, result_json};

const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

struct Trunk {
    test_box: TestBox,
    repo: PathBuf,
    suite_box: PathBuf,
}

/// G244's trunk: `src/` and `docs/`, a post check `suite` on `src/**` whose records a runner
/// elsewhere writes.
fn trunk() -> Trunk {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(POST);
        }
    }
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("docs/a.md", "a\n")],
        &files,
    );
    let suite_box = test_box.clone_of(&remote, "suite-box");
    let key = fresh_key(&test_box, "suite-1");
    test_box.git_ok(&suite_box, &["config", "bay.producerName", "suite-1"]);
    test_box.git_ok(
        &suite_box,
        &["config", "bay.producerKey", key.to_str().unwrap()],
    );
    Trunk {
        test_box,
        repo,
        suite_box,
    }
}

impl Trunk {
    /// Lands a change that writes `path`: its change id and its commit on main.
    fn land(&self, path: &str) -> (String, String) {
        let dir = Path::new(path).parent().unwrap().to_str().unwrap();
        let started = self
            .test_box
            .bay_ok(&self.repo, &["start", path, "--paths", dir]);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(path), format!("{path}\n")).unwrap();
        let (code, landed) = result_json(self.test_box.bay(&view).args(["merge", "--wait"]));
        assert_eq!(code, 0, "{landed}");
        (id, landed["detail"]["commit"].as_str().unwrap().to_owned())
    }

    fn suite(&self, landing: &str, verdict: &str) {
        let (code, written) = result_json(self.test_box.bay(&self.suite_box).args([
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
            landing,
        ]));
        assert_eq!(code, 0, "{written}");
    }

    fn red(&self) -> Value {
        self.test_box.bay_ok(&self.repo, &["status"])["detail"]["red"].clone()
    }
}

fn mark((change, commit): &(String, String)) -> Value {
    json!({"merge": commit, "change": change})
}

#[test]
fn a_red_row_without_a_culprit_says_its_owner_is_not_a_proven_cause() {
    let trunk = trunk();
    // The suite passes on `first`; `config` lands off its paths and breaks it; `later` lands on
    // its paths, and the suite fails there.
    let first = trunk.land("src/first.rs");
    let config = trunk.land("docs/config.md");
    let later = trunk.land("src/later.rs");
    trunk.suite(&first.1, "pass");
    trunk.suite(&later.1, "product_failure");

    let red = trunk.red();
    assert_eq!(red.as_array().map(Vec::len), Some(1), "{red}");
    let row = &red[0];
    assert_eq!(row["change"], later.0.as_str(), "{red}");
    assert_eq!(row["culprit"], Value::Null, "{red}");
    // The merge off the check's paths stays a suspect.
    assert_eq!(
        row["suspect"]["merges"],
        json!([mark(&config), mark(&later)]),
        "a merge off the check's paths left the range: {red}"
    );
    let say = row["say"].as_str().unwrap_or_default();
    assert!(say.contains(&later.0), "the row names its owner: {red}");
    assert!(
        say.contains("seen failing") && say.contains("not a proven cause"),
        "the row says its owner is where the check was seen failing: {red}"
    );
    assert!(
        say.contains("one of the 2 merges") && say.contains("on its paths or not"),
        "the row says the cause is among the range, off its paths too: {red}"
    );

    // The same row reaches a change the red holds.
    let (_, view) = trunk.test_box.start_and_edit(&trunk.repo);
    let (code, waiting) = result_json(trunk.test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "it waits for main: {waiting}");
    assert_eq!(waiting["reason"], "MAIN_RED", "{waiting}");
    assert_eq!(waiting["detail"]["red"][0]["say"], row["say"], "{waiting}");
}
