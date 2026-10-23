//! G244d: a red post check names its culprit by bisection.
//!
//! A post check that tests only some trunk tips leaves a suspect range, and the newest failure
//! is not always the landing that broke it. With `[runner] bisect_post = true`, an executor
//! that serves the check's environment runs it on the range's midpoints, first parent, records
//! each verdict as ordinary post evidence, and runs the suspect once more to rule out a flake,
//! in at most log2(n) + 1 runs. The pinned landing is the red's `culprit`, and it owns the
//! red: `bay status` names it, and `MAIN_RED` names its change. Off by default.

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
    remote: PathBuf,
    suite_box: PathBuf,
}

/// A trunk whose pre check `tests` always passes and whose post check `suite` fails on every
/// tree that holds `src/l3.rs`: the fourth landing breaks it. A runner elsewhere, `suite-1`,
/// records the post answers it was asked for; `config` is `.bay/config.toml`.
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
        remote,
        suite_box,
    }
}

impl Trunk {
    /// Lands a change that writes `path`: its change id and its commit on trunk.
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

    /// Five landings on `src/`, oldest first: `src/l0.rs` to `src/l4.rs`. The suite passed on
    /// the first and failed on the last, so the suspect range is the four after the first.
    fn red_over_four(&self) -> Vec<(String, String)> {
        let landings: Vec<(String, String)> =
            (0..5).map(|n| self.land(&format!("src/l{n}.rs"))).collect();
        self.suite(&landings[0].1, "pass");
        self.suite(&landings[4].1, "product_failure");
        landings
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

    /// One executor pass on the trunk's box; its `bisect` report, when it made one.
    fn executor_pass(&self) -> Option<Value> {
        let ran = self
            .test_box
            .bay_ok(&self.repo, &["runner", "run", "--once"]);
        ran["detail"]["ran"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|entry| entry.get("bisect").cloned())
    }

    /// The `suite` verdicts this box's producer (`local`) recorded on `commit`'s tree, as the
    /// remote holds them.
    fn recorded(&self, commit: &str) -> Vec<String> {
        let tree = self
            .test_box
            .git_bare(&self.remote, &["rev-parse", &format!("{commit}^{{tree}}")]);
        let notes = self.test_box.git_bare(
            &self.remote,
            &[
                "notes",
                "--ref=refs/notes/bay/evidence/local",
                "show",
                tree.trim(),
            ],
        );
        let records: Vec<Value> = serde_json::from_str(&notes).unwrap();
        records
            .iter()
            .filter(|record| record["check"]["id"] == "suite")
            .inspect(|record| assert!(record["bisect"].is_object(), "{record}"))
            .map(|record| record["verdict"].as_str().unwrap().to_owned())
            .collect()
    }
}

fn mark((change, commit): &(String, String)) -> Value {
    json!({"merge": commit, "change": change})
}

fn tested((change, commit): &(String, String), verdict: &str) -> Value {
    json!({"merge": commit, "change": change, "verdict": verdict})
}

#[test]
fn a_bisect_names_the_landing_that_broke_the_post_check_not_the_newest_failure() {
    let trunk = trunk("[runner]\nbisect_post = true\n");
    let landings = trunk.red_over_four();
    let red = trunk.red();
    assert_eq!(red[0]["merge"], landings[4].1.as_str(), "{red}");
    assert_eq!(red[0]["culprit"], Value::Null, "{red}");

    // The midpoint passes, the next fails, and a rerun there fails again: three runs.
    let report = trunk.executor_pass().expect("the executor bisects");
    let report = &report[0];
    assert_eq!(report["outcome"], "culprit", "{report}");
    assert_eq!(report["culprit"], mark(&landings[3]), "{report}");
    assert_eq!(
        report["tested"],
        json!([
            tested(&landings[2], "pass"),
            tested(&landings[3], "product_failure"),
            tested(&landings[3], "product_failure"),
        ]),
        "{report}"
    );
    assert_eq!(report["runs"], 3, "{report}");
    assert_eq!(report["bound"], 3, "log2(4) + 1: {report}");

    // Each run is ordinary post evidence on the landing's tree, pushed.
    assert_eq!(trunk.recorded(&landings[2].1), ["pass"]);
    assert_eq!(
        trunk.recorded(&landings[3].1),
        ["product_failure", "product_failure"]
    );

    // The culprit owns the red, not the newest failure.
    let red = trunk.red();
    assert_eq!(red.as_array().map(Vec::len), Some(1), "{red}");
    assert_eq!(red[0]["merge"], landings[3].1.as_str(), "{red}");
    assert_eq!(red[0]["change"], landings[3].0.as_str(), "{red}");
    let culprit = &red[0]["culprit"];
    assert_eq!(culprit["merge"], landings[3].1.as_str(), "{red}");
    assert_eq!(culprit["change"], landings[3].0.as_str(), "{red}");
    assert_eq!(
        culprit["tested"],
        json!([
            tested(&landings[2], "pass"),
            tested(&landings[3], "product_failure"),
            tested(&landings[4], "product_failure"),
        ]),
        "{red}"
    );
    assert_eq!(red[0]["suspect"]["merges"], json!([mark(&landings[3])]));

    // A pinned range is not bisected again.
    assert_eq!(trunk.executor_pass(), None);

    // MAIN_RED names the culprit's change as the owner.
    let (_, view) = trunk.test_box.start_and_edit(&trunk.repo);
    let (code, refused) = result_json(trunk.test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "MAIN_RED", "{refused}");
    assert_eq!(
        refused["detail"]["red"][0]["change"],
        landings[3].0.as_str(),
        "{refused}"
    );
    assert_eq!(
        refused["detail"]["owners"],
        json!([landings[3].0]),
        "{refused}"
    );
}

#[test]
fn without_bisect_post_the_executor_runs_no_post_check() {
    let trunk = trunk("");
    let landings = trunk.red_over_four();
    assert_eq!(trunk.executor_pass(), None);
    let red = trunk.red();
    assert_eq!(red[0]["merge"], landings[4].1.as_str(), "{red}");
    assert_eq!(red[0]["culprit"], Value::Null, "{red}");
    assert_eq!(
        red[0]["suspect"]["merges"].as_array().map(Vec::len),
        Some(4)
    );
}
