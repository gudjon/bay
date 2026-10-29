//! G530 (bay 0.6.1, B5; a fleet finding, 2026-10-28: three of oz-platform's post checks were red
//! with no pass in their 50-merge window, so bay named no range end, no culprit and no cause). A
//! red post check whose `[red] window` holds no pass reads further back, up to `[red] reach`
//! main commits, for its last pass. With the pass found, the suspect range and the culprit work
//! as they do inside the window: a bisect names the merge that broke it, past the window.
//!
//! Guards to mutate, in `src/red.rs`: `Answers::past` reads nothing (the range has no good end
//! and the bisect does not run); `Answers::suspect` stops at the window's end (the same).

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::talk::fresh_key;
use support::{TestBox, check_files, passing_result, result_json};

/// The post check `suite` on `src/**`, on the shared adapter with `suite` as its argument.
const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check suite\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

struct Trunk {
    test_box: TestBox,
    repo: PathBuf,
    suite_box: PathBuf,
}

/// G244d's trunk: `suite` fails on every tree that holds `src/l3.rs`, so the fourth landing
/// breaks it. A runner elsewhere, `suite-1`, records the answers it is asked for; `config` is
/// `.bay/config.toml`.
fn trunk(config: &str) -> Trunk {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let mut files = check_files(
        &format!("[ \"$1\" = suite ] && [ -e src/l3.rs ] && {{ {failing}; exit 0; }}"),
        &passing_result(1),
        1,
        60,
    );
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(POST);
        }
    }
    files.push((".bay/config.toml".to_owned(), config.to_owned()));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
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

    /// How many first-parent commits main has.
    fn depth(&self) -> u64 {
        self.test_box
            .git_ok(&self.repo, &["fetch", "-q", "origin", "main"]);
        self.test_box
            .git_ok(
                &self.repo,
                &[
                    "rev-list",
                    "--first-parent",
                    "--count",
                    "refs/remotes/origin/main",
                ],
            )
            .trim()
            .parse()
            .unwrap()
    }

    /// One runner pass on the trunk's box; its `bisect` report, when it made one.
    fn runner_pass(&self) -> Option<Value> {
        let ran = self
            .test_box
            .bay_ok(&self.repo, &["runner", "run", "--once"]);
        ran["detail"]["ran"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|entry| entry.get("bisect").cloned())
    }
}

fn mark((change, commit): &(String, String)) -> Value {
    json!({"merge": commit, "change": change})
}

#[test]
fn a_red_reads_past_its_window_for_the_last_pass_and_a_bisect_names_the_culprit_there() {
    // A window of one merge: only main's tip is judged for the newest answer.
    let trunk = trunk("[red]\nwindow = 1\n[runner]\nbisect_post = true\n");
    let landings: Vec<(String, String)> = (0..5)
        .map(|n| trunk.land(&format!("src/l{n}.rs")))
        .collect();

    // The suite fails on the tip and has never passed: bay reads all of main and says so.
    trunk.suite(&landings[4].1, "product_failure");
    let red = trunk.red();
    assert_eq!(red.as_array().map(Vec::len), Some(1), "{red}");
    let suspect = &red[0]["suspect"];
    assert_eq!(suspect["last_passed"], Value::Null, "{red}");
    assert_eq!(suspect["searched"], json!(trunk.depth()), "{red}");

    // It passed on the first landing, four merges below the window.
    trunk.suite(&landings[0].1, "pass");
    let red = trunk.red();
    let suspect = &red[0]["suspect"];
    assert_eq!(
        suspect["last_passed"],
        mark(&landings[0]),
        "the pass past the window is the range's good end: {red}"
    );
    assert_eq!(suspect["searched"], Value::Null, "{red}");
    assert_eq!(suspect["first_failed"], mark(&landings[4]), "{red}");
    assert_eq!(
        suspect["merges"],
        json!([
            mark(&landings[1]),
            mark(&landings[2]),
            mark(&landings[3]),
            mark(&landings[4]),
        ]),
        "{red}"
    );
    assert_eq!(red[0]["culprit"], Value::Null, "{red}");

    // A bisect runs over that range and names the merge that broke it, past the window.
    let report = trunk.runner_pass().expect("the runner bisects");
    assert_eq!(report[0]["outcome"], "culprit", "{report}");
    assert_eq!(report[0]["culprit"], mark(&landings[3]), "{report}");
    let red = trunk.red();
    assert_eq!(red[0]["merge"], landings[3].1.as_str(), "{red}");
    assert_eq!(red[0]["change"], landings[3].0.as_str(), "{red}");
    assert_eq!(red[0]["culprit"]["merge"], landings[3].1.as_str(), "{red}");
    let say = red[0]["say"].as_str().unwrap_or_default();
    assert!(
        say.contains(&landings[3].0) && say.contains("broke"),
        "the row names the culprit as the cause: {red}"
    );
}
