//! G584 (a fleet finding, 2026-10-30: a post check that never passed read back to the reach,
//! 545 s a judgement, and its red named the newest failure). The walk back for a red post
//! check's last pass stops at the landing whose base policy lists other post checks and not this
//! one: the landing that added the check. With no pass after it, that landing owns the red, ends
//! its range, and is the name `--fixes` gives (`check_added`); the row never says how far it
//! read, since it did not read past it. A pass after the landing that added the check is the
//! range's good end, as before.
//!
//! Guards to mutate, in `src/red.rs`: `Answers::unlisted` always `false` (the walk runs past
//! the landing to the reach, and the newest failure owns the red); `Answers::suspect` with the
//! owner never the landing that added the check; `Red::fixes` without it.
//!
//! G598: a full window off the check's paths owes nothing when the walk past it comes to the
//! landing that added the check before any landing requires it. Guard to mutate, in
//! `src/red.rs`: `Answers::clear_past` without the `unlisted` arm (it is `pending_beyond_window`).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use serde_json::{Value, json};
use support::talk::fresh_key;
use support::{TestBox, check_files, passing_result, result_json};

/// A post check on `conf/**` that no landing here wakes: the seed policy lists a post check,
/// and not `suite`.
const LINT: &str = "\n[[check]]\nid = \"lint\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"conf/**\"]\n";

/// The post check a landing adds.
const SUITE: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

struct Trunk {
    test_box: TestBox,
    repo: PathBuf,
    suite_box: PathBuf,
}

/// A trunk whose policy has a pre check `tests` and the post check `lint`, and no `suite`,
/// with `config` as its `.bay/config.toml`, and a runner elsewhere that writes `suite` records.
fn trunk(config: &str) -> Trunk {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(LINT);
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

    /// Lands a change that writes `src/<name>.rs`.
    fn land_src(&self, name: &str) -> (String, String) {
        let path = format!("src/{name}.rs");
        self.land(&path, &format!("fn {name}() {{}}\n"))
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
}

fn mark((change, commit): &(String, String)) -> Value {
    json!({"merge": commit, "change": change})
}

#[test]
fn a_check_that_never_passed_is_owned_by_the_merge_that_added_it() {
    // Window 1: the landing that added the check lies past the window, so the walk reads on.
    let trunk = trunk("[red]\nwindow = 1\n");
    trunk.land_src("b");
    let added = trunk.add_suite();
    let landings: Vec<(String, String)> = ["l0", "l1", "l2"]
        .iter()
        .map(|name| trunk.land_src(name))
        .collect();
    trunk.suite(&landings[0].1, "product_failure");
    trunk.suite(&landings[2].1, "product_failure");

    let red = trunk.red();
    assert_eq!(red.len(), 1, "{red:?}");
    let row = &red[0];
    assert_eq!(row["merge"], json!(added.1), "the adder owns it: {row}");
    assert_eq!(row["change"], json!(added.0), "{row}");
    assert_eq!(
        row["fixes"],
        json!(added.0),
        "--fixes names the adder: {row}"
    );
    let suspect = &row["suspect"];
    assert_eq!(suspect["check_added"], mark(&added), "{row}");
    assert_eq!(suspect["last_passed"], Value::Null, "{row}");
    assert_eq!(
        suspect["searched"],
        Value::Null,
        "it read no further: {row}"
    );
    assert_eq!(suspect["first_failed"], mark(&landings[0]), "{row}");
    assert_eq!(
        suspect["merges"],
        json!([mark(&added), mark(&landings[0])]),
        "the range ends at the adder: {row}"
    );
    assert_eq!(
        suspect["failing"],
        json!([mark(&landings[1]), mark(&landings[2])]),
        "{row}"
    );
    let say = row["say"].as_str().unwrap_or_default();
    assert!(
        say.contains(&format!("since change {} added it to the policy", added.0)),
        "the row names the adder: {row}"
    );
    assert!(
        say.contains(&format!("at change {} last", landings[2].0)),
        "and where it was seen failing: {row}"
    );
}

#[test]
fn a_pass_after_the_merge_that_added_the_check_is_the_good_end() {
    let trunk = trunk("[red]\nwindow = 1\n");
    trunk.add_suite();
    let landings: Vec<(String, String)> = ["l0", "l1", "l2"]
        .iter()
        .map(|name| trunk.land_src(name))
        .collect();
    trunk.suite(&landings[0].1, "pass");
    trunk.suite(&landings[2].1, "product_failure");

    let red = trunk.red();
    assert_eq!(red.len(), 1, "{red:?}");
    let row = &red[0];
    let suspect = &row["suspect"];
    assert_eq!(suspect["last_passed"], mark(&landings[0]), "{row}");
    assert_eq!(suspect["check_added"], Value::Null, "{row}");
    assert_eq!(row["change"], json!(landings[2].0), "{row}");
    assert_eq!(
        suspect["merges"],
        json!([mark(&landings[1]), mark(&landings[2])]),
        "{row}"
    );
}

/// The states of `suite`'s `red_pending` rows.
fn suite_pending(trunk: &Trunk) -> Value {
    let status = trunk.test_box.bay_ok(&trunk.repo, &["status"])["detail"].clone();
    status["red_pending"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["check"] == "suite")
        .map(|row| row["state"].clone())
        .collect()
}

#[test]
fn a_full_window_off_its_paths_owes_nothing_back_to_the_merge_that_added_the_check() {
    let trunk = trunk("[red]\nwindow = 2\n");
    let _ = trunk.add_suite();

    // GIVEN the landing that added the check past a full window of landings off its paths
    // WHEN status judges THEN no landing since it required the check: nothing is pending.
    for n in 1..=2 {
        let _ = trunk.land(&format!("docs/d{n}.md"), "d\n");
    }
    assert_eq!(trunk.red(), Vec::<Value>::new());
    assert_eq!(suite_pending(&trunk), json!([]));

    // GIVEN a landing on its paths in the window, not yet answered THEN it is pending.
    let _ = trunk.land_src("b");
    assert_eq!(suite_pending(&trunk), json!(["pending_beyond_window"]));
}
