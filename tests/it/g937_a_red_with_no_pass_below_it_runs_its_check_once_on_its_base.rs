//! G937 (BACKLOG 348, a load-run finding: a post check failed on its first runs, after the merge
//! that broke it, so its range had no good end; no culprit or undo came, and each runner wrote
//! `bisect work (0s)` every 10 s without saying why). A red post check with no pass below it is
//! run once on its range's base, the merge that added the check, as a change of nothing on it
//! under that merge's own policy. A pass there is the range's good end, and the bisect names the
//! culprit as usual. A failure there makes the red the check's own: `bay status` says so, no
//! culprit is named, nothing runs again, and a looping runner says why once.
//!
//! Guards to mutate: in `src/red.rs`, `Answers::on_itself` always `None` (the range keeps no
//! good end); in `src/bisect.rs`, `on_base` run `Under::FirstParent` (the policy there has no
//! such check, so it is not run); in `src/executor/post.rs`, `Post::fresh_lines` keeping no
//! words (each pass writes the row again).

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::talk::fresh_key;
use support::{TestBox, check_files, passing_result, result_json};

/// A post check on `conf/**` that no landing here wakes: the seed policy lists a post check,
/// and not `suite`.
const LINT: &str = "\n[[check]]\nid = \"lint\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check lint\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"conf/**\"]\n";

/// The post check a landing adds, on `src/**`.
const SUITE: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check suite\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

struct Trunk {
    test_box: TestBox,
    repo: PathBuf,
    remote: PathBuf,
    suite_box: PathBuf,
}

/// A trunk with a pre check `tests`, the post check `lint` and `[runner] bisect_post`, whose
/// `suite`, once a landing adds it, fails on every tree where the shell test `fails` holds. A
/// runner elsewhere, `suite-1`, records the `suite` answers it is asked for.
fn trunk(fails: &str) -> Trunk {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let mut files = check_files(
        &format!("[ \"$1\" = suite ] && {fails} && {{ {failing}; exit 0; }}"),
        &passing_result(1),
        1,
        60,
    );
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(LINT);
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\nbisect_post = true\n".to_owned(),
    ));
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

/// A child killed when the test ends, so a failed assertion never leaves it running.
struct Reaped(std::process::Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Trunk {
    /// Lands a change that writes `text` to `path`: its change id and its commit on main.
    fn land(&self, path: &str, text: &str) -> (String, String) {
        let started = self
            .test_box
            .bay_ok(&self.repo, &["start", path, "--paths", path]);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        let file = view.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
        let (code, landed) = result_json(self.test_box.bay(&view).args(["merge", "--wait"]));
        assert_eq!(code, 0, "{landed}");
        (id, landed["detail"]["commit"].as_str().unwrap().to_owned())
    }

    /// Lands `src/l0.rs` to `src/l<n-1>.rs`, oldest first.
    fn land_src(&self, n: usize) -> Vec<(String, String)> {
        (0..n)
            .map(|n| self.land(&format!("src/l{n}.rs"), &format!("fn l{n}() {{}}\n")))
            .collect()
    }

    /// Lands the change that adds `suite` to main's policy.
    fn add_suite(&self) -> (String, String) {
        let policy = self
            .test_box
            .git_ok(
                &self.repo,
                &["show", "refs/remotes/origin/main:.bay/policy.toml"],
            )
            .replacen("revision = 1", "revision = 2", 1);
        self.land(".bay/policy.toml", &format!("{policy}{SUITE}"))
    }

    /// Records `verdict` of `suite` on `landing` from the runner elsewhere.
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

    /// The red rows of `suite`.
    fn red(&self) -> Vec<Value> {
        let red = self.test_box.bay_ok(&self.repo, &["status"])["detail"]["red"].clone();
        red.as_array()
            .unwrap()
            .iter()
            .filter(|row| row["check"] == "suite")
            .cloned()
            .collect()
    }

    /// One runner pass on the trunk's box; its `bisect` rows, when it made any.
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

    /// The `suite` verdicts this box's producer (`local`) recorded on `commit`'s tree, as the
    /// remote holds them: bisect runs, each.
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

    /// The journal of a runner loop on the trunk's box, a pass a second for `runs`, then
    /// stopped by a drain.
    fn looping_journal(&self, runs: Duration) -> String {
        let log = self.test_box.path().join("runner.log");
        let mut child = Reaped(
            self.test_box
                .bay_process(&self.repo)
                .args(["--json", "runner", "run", "--interval-s", "1"])
                .env("BAY_DISABLE_AUTOUPDATE", "1")
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(&log).unwrap())
                .spawn()
                .unwrap(),
        );
        std::thread::sleep(runs);
        self.test_box.bay_ok(&self.repo, &["runner", "drain"]);
        let began = Instant::now();
        while child.0.try_wait().unwrap().is_none() {
            let said = std::fs::read_to_string(&log).unwrap_or_default();
            assert!(
                began.elapsed() < Duration::from_secs(120),
                "the runner never stopped: {said}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        std::fs::read_to_string(&log).unwrap()
    }
}

fn mark((change, commit): &(String, String)) -> Value {
    json!({"merge": commit, "change": change})
}

fn tested((change, commit): &(String, String), verdict: &str) -> Value {
    json!({"merge": commit, "change": change, "verdict": verdict})
}

#[test]
fn a_pass_on_the_base_is_the_good_end_and_the_bisect_names_the_culprit() {
    // GIVEN a check that `src/l3.rs` breaks, added before it, and seen failing only after it
    let trunk = trunk("[ -e src/l3.rs ]");
    let added = trunk.add_suite();
    let landings = trunk.land_src(5);
    trunk.suite(&landings[4].1, "product_failure");
    let red = trunk.red();
    assert_eq!(red.len(), 1, "{red:?}");
    assert_eq!(
        red[0]["merge"],
        json!(added.1),
        "the adder owns it: {red:?}"
    );
    assert_eq!(red[0]["suspect"]["last_passed"], Value::Null, "{red:?}");

    // WHEN a runner pass bisects THEN it runs the check once on the merge that added it
    let report = trunk.runner_pass().expect("the runner runs the base");
    assert_eq!(report[0]["outcome"], "base_passed", "{report}");
    assert_eq!(
        report[0]["tested"],
        json!([tested(&added, "pass")]),
        "{report}"
    );
    assert_eq!(report[0]["runs"], 1, "{report}");
    assert_eq!(trunk.recorded(&added.1), ["pass"]);

    // THEN that pass is the range's good end
    let red = trunk.red();
    let suspect = &red[0]["suspect"];
    assert_eq!(suspect["last_passed"], mark(&added), "{red:?}");
    assert_eq!(suspect["check_added"], Value::Null, "{red:?}");
    assert_eq!(suspect["base_failed"], Value::Null, "{red:?}");
    assert_eq!(
        suspect["merges"],
        json!(landings.iter().map(mark).collect::<Vec<_>>()),
        "{red:?}"
    );

    // AND the next pass bisects above it and names the culprit, which owns the red
    let report = trunk.runner_pass().expect("the runner bisects");
    assert_eq!(report[0]["outcome"], "culprit", "{report}");
    assert_eq!(report[0]["culprit"], mark(&landings[3]), "{report}");
    let red = trunk.red();
    assert_eq!(red[0]["merge"], json!(landings[3].1), "{red:?}");
    assert_eq!(red[0]["culprit"]["change"], json!(landings[3].0), "{red:?}");
}

#[test]
fn a_failure_on_the_base_makes_the_red_the_checks_own() {
    // GIVEN a check that fails on every tree, seen failing after the merge that added it
    let trunk = trunk("true");
    let added = trunk.add_suite();
    let landings = trunk.land_src(2);
    trunk.suite(&landings[1].1, "product_failure");

    // WHEN a runner pass bisects THEN its one run on the merge that added the check fails
    let report = trunk.runner_pass().expect("the runner runs the base");
    assert_eq!(report[0]["outcome"], "base_failed", "{report}");
    assert_eq!(report[0]["culprit"], Value::Null, "{report}");
    assert_eq!(
        report[0]["tested"],
        json!([tested(&added, "product_failure")]),
        "{report}"
    );

    // THEN the red is the check's own: the merge that added it owns it, and no merge is a culprit
    let red = trunk.red();
    assert_eq!(red.len(), 1, "{red:?}");
    let row = &red[0];
    assert_eq!(row["merge"], json!(added.1), "{row}");
    assert_eq!(row["culprit"], Value::Null, "{row}");
    assert_eq!(row["suspect"]["base_failed"], mark(&added), "{row}");
    assert_eq!(row["suspect"]["last_passed"], Value::Null, "{row}");
    let say = row["say"].as_str().unwrap_or_default();
    assert!(
        say.contains(&format!(
            "a run where change {} added it to the policy failed as well, so the red is the \
             check's own",
            added.0
        )),
        "status says so: {row}"
    );

    // AND no pass runs it again: its bisect row says why
    let report = trunk.runner_pass().expect("the bisect row");
    assert_eq!(report[0]["outcome"], "not_run", "{report}");
    let why = report[0]["why"].as_str().unwrap_or_default();
    assert!(why.contains("so the red is the check's own"), "{report}");
    assert_eq!(trunk.recorded(&added.1), ["product_failure"]);

    // AND a looping runner's journal says why once, not each pass
    let journal = trunk.looping_journal(Duration::from_secs(5));
    let said: Vec<&str> = journal
        .lines()
        .filter(|line| line.starts_with("bay runner: bisect suite: not run: "))
        .collect();
    assert_eq!(said.len(), 1, "{journal}");
    assert!(said[0].contains(why), "{journal}");
    assert!(!journal.contains("bisect work"), "{journal}");
}
