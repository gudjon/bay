//! G195: `bay status --since` and `bay evidence missing --since` scale with the commits after
//! their cursor, not with trunk's history (the grit-learnings measurement items).
//!
//! A watch from a cursor reads the landings after the cursor's trunk, and judges a red trunk
//! from what the cursor carries: it reads the post answers only of the new landings and of
//! those whose evidence note moved. `bay evidence missing --since` walks the commits after its
//! rev. Read from the trace a test build writes (`BAY_TEST_GIT_TRACE`, G171), one new commit
//! costs the same git spawns after 32 earlier commits as after 5. And the judgement from a
//! cursor is the full one: the same red as `bay status` and a watch with no cursor.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use assert_cmd::Command;
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;
use support::talk::fresh_key;
use support::{TestBox, check_files, passing_result, result_json};

const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

/// A trunk with a post check `suite`, whose records producer `suite-1` writes from its own
/// clone, and commits pushed to it outside bay.
struct Trunk {
    test_box: TestBox,
    repo: PathBuf,
    suite_box: PathBuf,
}

impl Trunk {
    fn new() -> Self {
        let test_box = TestBox::new();
        let repo = test_box.repo_any("repo");
        let remote = test_box.path().join("repo-remote.git");
        let mut files = check_files("true", &passing_result(1), 1, 60);
        for (path, text) in &mut files {
            if path == ".bay/policy.toml" {
                text.push_str(POST);
            }
        }
        test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
        let suite_box = test_box.clone_of(&remote, "suite-box");
        let key = fresh_key(&test_box, "suite-1");
        test_box.git_ok(&suite_box, &["config", "bay.producerName", "suite-1"]);
        test_box.git_ok(
            &suite_box,
            &["config", "bay.producerKey", key.to_str().unwrap()],
        );
        Self {
            test_box,
            repo,
            suite_box,
        }
    }

    /// Commits one new file under `src/` for each name, pushes them to trunk outside bay, and
    /// returns the commits, oldest first.
    fn push(&self, names: &[String]) -> Vec<String> {
        let commits = names
            .iter()
            .map(|name| {
                let path = format!("src/{name}.rs");
                std::fs::write(self.repo.join(&path), format!("// {name}\n")).unwrap();
                self.test_box.git_ok(&self.repo, &["add", &path]);
                self.test_box
                    .git_ok(&self.repo, &["commit", "-q", "-m", name]);
                self.test_box.git_ok(&self.repo, &["rev-parse", "HEAD"])
            })
            .collect();
        self.test_box
            .git_ok(&self.repo, &["push", "-q", "origin", "HEAD:main"]);
        commits
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

    /// `bay status --since` outside any view, from `since`: its detail.
    fn watch(&self, since: Option<&str>) -> Value {
        let mut args = vec!["status", "--since"];
        if let Some(cursor) = since {
            args.push(cursor);
        }
        self.test_box.bay_ok(&self.repo, &args)["detail"].clone()
    }

    /// The red checks `bay status` judges over the whole window, as `(check, landing)`.
    fn status_red(&self) -> Vec<(String, String)> {
        let status = self.test_box.bay_ok(&self.repo, &["status"]);
        let mut red: Vec<(String, String)> = status["detail"]["red"]
            .as_array()
            .unwrap_or_else(|| panic!("{status}"))
            .iter()
            .map(owned)
            .collect();
        red.sort();
        red
    }
}

fn owned(red: &Value) -> (String, String) {
    (
        red["check"].as_str().unwrap().to_owned(),
        red["merge"].as_str().unwrap().to_owned(),
    )
}

/// The `(check, landing)` of each event of `kind` in a watch's detail, sorted.
fn events(detail: &Value, kind: &str) -> Vec<(String, String)> {
    let mut listed: Vec<(String, String)> = detail["events"]
        .as_array()
        .unwrap_or_else(|| panic!("{detail}"))
        .iter()
        .filter(|event| event["kind"] == kind)
        .map(owned)
        .collect();
    listed.sort();
    listed
}

fn cursor(detail: &Value) -> String {
    detail["cursor"].as_str().unwrap().to_owned()
}

/// The git subcommands one run of `cmd` spawned, by count, from bay's own process only, and
/// its result.
fn spawns(test_box: &TestBox, name: &str, cmd: &mut Command) -> (BTreeMap<String, usize>, Value) {
    let trace = test_box.path().join(format!("{name}-trace.jsonl"));
    let (code, result) = result_json(cmd.env("BAY_TEST_GIT_TRACE", &trace));
    assert_eq!(code, 0, "{result}");
    let text = std::fs::read_to_string(&trace).expect("the trace was written");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {line}")))
        .collect();
    let pid = lines.first().map(|line| line["pid"].clone());
    let mut counts = BTreeMap::new();
    for line in lines
        .iter()
        .filter(|line| Some(&line["pid"]) == pid.as_ref())
    {
        *counts
            .entry(line["sub"].as_str().unwrap_or_default().to_owned())
            .or_insert(0) += 1;
    }
    (counts, result)
}

/// The spawns of a watch from a cursor and of an evidence query from a rev, after one new
/// commit on a trunk of `history` commits pushed after the seed.
fn after_one_new_commit(history: usize) -> (BTreeMap<String, usize>, BTreeMap<String, usize>) {
    let trunk = Trunk::new();
    let names: Vec<String> = (0..history).map(|n| format!("h{n}")).collect();
    let old = trunk.push(&names).pop().unwrap();
    let since = cursor(&trunk.watch(None));
    let new = trunk.push(&["new".to_owned()]).pop().unwrap();

    let (watch, watched) = spawns(
        &trunk.test_box,
        "watch",
        trunk
            .test_box
            .bay(&trunk.repo)
            .args(["status", "--since", &since]),
    );
    let landings = &watched["detail"]["merges"];
    assert_eq!(landings.as_array().map(Vec::len), Some(1), "{watched}");
    assert_eq!(landings[0]["commit"], new.as_str(), "{watched}");

    let (missing, answered) = spawns(
        &trunk.test_box,
        "missing",
        trunk.test_box.bay(&trunk.repo).args([
            "evidence",
            "missing",
            "--kind",
            "check",
            "--producer",
            "suite-1",
            "--since",
            &old,
        ]),
    );
    assert_eq!(answered["detail"]["checked"], 1, "{answered}");
    (watch, missing)
}

#[test]
fn one_new_commit_costs_the_same_spawns_on_a_long_trunk_as_on_a_short_one() {
    let (short_watch, short_missing) = after_one_new_commit(5);
    let (long_watch, long_missing) = after_one_new_commit(32);
    let total = |counts: &BTreeMap<String, usize>| counts.values().sum::<usize>();
    assert_eq!(
        total(&long_watch),
        total(&short_watch),
        "a watch from a cursor grew with history: {long_watch:?} against {short_watch:?}"
    );
    assert_eq!(
        total(&long_missing),
        total(&short_missing),
        "evidence missing --since grew with history: {long_missing:?} against {short_missing:?}"
    );
}

#[test]
fn a_watch_from_a_cursor_judges_red_as_the_full_walk_does() {
    let trunk = Trunk::new();
    let names: Vec<String> = (0..12).map(|n| format!("h{n}")).collect();
    let history = trunk.push(&names);
    let first = trunk.watch(None);
    assert!(events(&first, "main_red").is_empty(), "{first}");

    // A failure answered on an older landing, then one new commit: the watch from the cursor
    // reads the note that moved, and turns red as the full walk does.
    trunk.suite(&history[3], "product_failure");
    let new = trunk.push(&["new".to_owned()]).pop().unwrap();
    let watched = trunk.watch(Some(&cursor(&first)));
    let landings = &watched["merges"];
    assert_eq!(landings.as_array().map(Vec::len), Some(1), "{watched}");
    assert_eq!(landings[0]["commit"], new.as_str(), "{watched}");
    let turned = events(&watched, "main_red");
    assert_eq!(
        turned,
        [("suite".to_owned(), history[3].clone())],
        "{watched}"
    );
    assert_eq!(trunk.status_red(), turned);
    assert_eq!(events(&trunk.watch(None), "main_red"), turned);
    let seen = cursor(&watched);
    let again = trunk.watch(Some(&seen));
    assert!(events(&again, "main_red").is_empty(), "{again}");
    assert!(events(&again, "main_red_cleared").is_empty(), "{again}");

    // A pass on a newer old landing, with no new commit: it clears the red, as the full walk
    // says.
    trunk.suite(&history[8], "pass");
    let cleared = trunk.watch(Some(&seen));
    assert_eq!(
        events(&cleared, "main_red_cleared"),
        [("suite".to_owned(), history[3].clone())],
        "{cleared}"
    );
    assert!(events(&cleared, "main_red").is_empty(), "{cleared}");
    assert_eq!(trunk.status_red().len(), 0);
    assert_eq!(events(&trunk.watch(None), "main_red").len(), 0);

    // A failure on a landing older than that pass changes nothing; one on the newest turns red.
    trunk.suite(&history[5], "product_failure");
    trunk.suite(&new, "product_failure");
    let next = trunk.watch(Some(&cursor(&cleared)));
    let turned = events(&next, "main_red");
    assert_eq!(turned, [("suite".to_owned(), new.clone())], "{next}");
    assert_eq!(trunk.status_red(), turned);
}

impl Trunk {
    /// main's tip on the remote, with every bay ref and note fetched into the main checkout, so
    /// the library reads the records the remote holds.
    fn fetched_tip(&self) -> String {
        self.test_box.git_ok(
            &self.repo,
            &[
                "fetch",
                "-q",
                "origin",
                "+refs/heads/main:refs/remotes/origin/main",
                "+refs/notes/bay/*:refs/notes/bay/*",
                "+refs/bay/*:refs/bay/*",
            ],
        );
        self.test_box
            .git_ok(&self.repo, &["rev-parse", "refs/remotes/origin/main"])
    }
}

/// BACKLOG 113: the runner's post work now judges main from the last judgement (`red::judged`,
/// as the heads do since 0.5.38), and reads more of it than the red owner: the pending checks
/// order its post checks, and a red check's suspect range is where the culprit finding runs its
/// midpoints. At every step of a red that turns, clears and turns again, the judgement from the
/// last one equals the full judgement in every field.
#[test]
fn the_judgement_from_the_last_one_is_the_full_one_in_every_field() {
    let trunk = Trunk::new();
    let git = bay::git::Git::new(&trunk.repo);
    let names: Vec<String> = (0..12).map(|n| format!("h{n}")).collect();
    let history = trunk.push(&names);
    let mut before: Option<bay::red::Judged> = None;
    let step = |label: &str, before: &mut Option<bay::red::Judged>| {
        let tip = trunk.fetched_tip();
        let registry = bay::registry::Registry::current(&git).unwrap();
        let full = bay::red::state(&git, &tip, registry.as_ref()).unwrap();
        let (from_last, judged) =
            bay::red::judge(&git, &tip, registry.as_ref(), before.as_ref()).unwrap();
        assert_eq!(
            from_last, full,
            "{label}: the judgement from the last one differs"
        );
        *before = judged;
        full
    };
    let none = step("no record", &mut before);
    assert!(none.red.is_empty(), "{none:?}");
    trunk.suite(&history[2], "pass");
    trunk.suite(&history[3], "product_failure");
    trunk.push(&["new".to_owned()]);
    let red = step("a failure, then a new commit", &mut before);
    assert_eq!(red.red.len(), 1, "{red:?}");
    trunk.suite(&history[8], "pass");
    let cleared = step("a later pass, no new commit", &mut before);
    assert!(cleared.red.is_empty(), "{cleared:?}");
    trunk.suite(&history[5], "product_failure");
    let newest = trunk.push(&["newest".to_owned()]).pop().unwrap();
    trunk.suite(&newest, "product_failure");
    let again = step("an older failure and the newest", &mut before);
    assert_eq!(again.red.len(), 1, "{again:?}");
    let more = trunk.push(&["n1".to_owned(), "n2".to_owned(), "n3".to_owned()]);
    trunk.suite(&more[2], "product_failure");
    let ranged = step("a red with a range of several landings", &mut before);
    assert_eq!(ranged.red.len(), 1, "{ranged:?}");
}

impl Trunk {
    /// Writes `path` with `text` in the main checkout, commits it and pushes it to trunk outside
    /// bay; returns the commit.
    fn push_file(&self, path: &str, text: &str) -> String {
        let full = self.repo.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, text).unwrap();
        self.test_box.git_ok(&self.repo, &["add", path]);
        self.test_box
            .git_ok(&self.repo, &["commit", "-q", "-m", path]);
        self.test_box
            .git_ok(&self.repo, &["push", "-q", "origin", "HEAD:main"]);
        self.test_box.git_ok(&self.repo, &["rev-parse", "HEAD"])
    }
}

impl Trunk {
    /// Producer `suite-1` answers post check `check` on `landing` with `verdict`.
    fn answer(&self, check: &str, landing: &str, verdict: &str) {
        let (code, written) = result_json(self.test_box.bay(&self.suite_box).args([
            "evidence",
            "record",
            "--kind",
            "check",
            "--check",
            check,
            "--environment",
            "local",
            "--verdict",
            verdict,
            "--commit",
            landing,
        ]));
        assert_eq!(code, 0, "{written}");
    }
}

/// BACKLOG 113, from a review: the judgement from the last one equals the full one also where its
/// shortcuts and its window reach: a new commit with no record while red (the carried decision
/// answers), a second post check answered only long ago (pending in a small window), and a `[red]
/// window` grown between two judgements, which reaches that old answer.
#[test]
fn the_judgement_from_the_last_one_holds_across_its_shortcuts_and_a_grown_window() {
    let trunk = Trunk::new();
    let git = bay::git::Git::new(&trunk.repo);
    let policy = std::fs::read_to_string(trunk.repo.join(".bay/policy.toml")).unwrap();
    trunk.push_file(
        ".bay/policy.toml",
        &format!("{policy}{}", POST.replace("\"suite\"", "\"silent\"")),
    );
    trunk.push_file(".bay/config.toml", "[red]\nwindow = 4\n");
    let names: Vec<String> = (0..10).map(|n| format!("w{n}")).collect();
    let history = trunk.push(&names);
    let mut before: Option<bay::red::Judged> = None;
    let step = |label: &str, before: &mut Option<bay::red::Judged>| {
        let tip = trunk.fetched_tip();
        let registry = bay::registry::Registry::current(&git).unwrap();
        let full = bay::red::state(&git, &tip, registry.as_ref()).unwrap();
        let (from_last, judged) =
            bay::red::judge(&git, &tip, registry.as_ref(), before.as_ref()).unwrap();
        assert_eq!(
            from_last, full,
            "{label}: the judgement from the last one differs"
        );
        *before = judged;
        full
    };
    // `suite` red on the newest landing; `silent` answered only on a landing the window of 4 does
    // not reach.
    trunk.answer("suite", &history[9], "product_failure");
    trunk.answer("silent", &history[2], "product_failure");
    let small = step("a red in a window of 4", &mut before);
    assert!(
        small.red.iter().any(|red| red.check == "suite"),
        "{small:?}"
    );
    assert!(
        !small.red.iter().any(|red| red.check == "silent"),
        "{small:?}"
    );
    assert!(
        small
            .pending
            .iter()
            .any(|pending| pending.check == "silent"),
        "{small:?}"
    );
    // A new commit with no record while red: the carried decision answers.
    trunk.push(&["quiet".to_owned()]);
    let quiet = step("a new commit with no record while red", &mut before);
    assert!(
        quiet.red.iter().any(|red| red.check == "suite"),
        "{quiet:?}"
    );
    // The window grows: it now reaches `silent`'s old failure.
    trunk.push_file(".bay/config.toml", "[red]\nwindow = 50\n");
    let grown = step("a grown window", &mut before);
    assert!(
        grown.red.iter().any(|red| red.check == "silent"),
        "the grown window reaches the old failure: {grown:?}"
    );
}
