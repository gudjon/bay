//! G244b: a red trunk shows in the fleet map and in `bay status --since` (the pilot-gaps analysis of
//! 2026-10-20, plan item A1).
//!
//! `bay status --fleet` lists the post checks trunk is red on, as `bay status` does, and marks
//! each entry whose change works on red paths (`on_red`). `bay status --since` lists a `trunk_red` event
//! when a check turns red after the cursor, and a `trunk_red_cleared` event when a fix clears it.

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

/// G244's trunk: a post check `suite` on `src/**`, whose records a runner elsewhere writes.
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
    /// Starts a change on `path`'s directory (`--fixes <fixes>` when given) and writes `path`.
    fn start(&self, path: &str, fixes: Option<&str>) -> (String, PathBuf) {
        let dir = Path::new(path)
            .parent()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let mut args = vec!["start", path, "--paths", dir.as_str()];
        if let Some(fixed) = fixes {
            args.extend(["--fixes", fixed]);
        }
        let started = self.test_box.bay_ok(&self.repo, &args);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(path), format!("{path}\n")).unwrap();
        (id, view)
    }

    fn land(&self, path: &str, fixes: Option<&str>) -> (String, String) {
        let (id, view) = self.start(path, fixes);
        let (code, landed) = result_json(self.test_box.bay(&view).args(["merge", "--wait"]));
        assert_eq!(code, 0, "{landed}");
        let commit = landed["detail"]["commit"].as_str().unwrap().to_owned();
        (id, commit)
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

    /// The fleet map after `bay status`, its fetch.
    fn fleet(&self) -> Value {
        self.test_box.bay_ok(&self.repo, &["status"]);
        self.test_box
            .bay_ok(&self.repo, &["status", "--fleet"])
            .get("detail")
            .cloned()
            .unwrap()
    }

    /// `bay status --since` outside any view, from `since`: its events and the cursor it returned.
    fn watch(&self, since: Option<&str>) -> (Vec<Value>, String) {
        let mut args = vec!["status", "--since"];
        if let Some(cursor) = since {
            args.push(cursor);
        }
        let watched = self.test_box.bay_ok(&self.repo, &args);
        let events = watched["detail"]["events"]
            .as_array()
            .unwrap_or_else(|| panic!("{watched}"))
            .clone();
        let cursor = watched["detail"]["cursor"].as_str().unwrap().to_owned();
        (events, cursor)
    }
}

/// The fleet entry of change `id`.
fn entry<'a>(fleet: &'a Value, id: &str) -> &'a Value {
    fleet["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["change"] == id)
        .unwrap_or_else(|| panic!("no entry for {id}: {fleet}"))
}

fn of_kind<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["kind"] == kind)
        .collect()
}

#[test]
fn the_fleet_map_marks_who_works_on_red_paths_and_watch_sees_red_turn_and_clear() {
    let trunk = trunk();
    let (broke, landing) = trunk.land("src/broke.rs", None);
    // Two changes held on this machine, with no session: one on the red paths, one elsewhere.
    let (on_src, _) = trunk.start("src/held.rs", None);
    let (on_docs, _) = trunk.start("docs/held.md", None);
    let (events, green) = trunk.watch(None);
    assert!(of_kind(&events, "trunk_red").is_empty(), "{events:?}");

    trunk.suite(&landing, "product_failure");

    // The fleet map lists the red check, and marks the entry working on its paths only.
    let fleet = trunk.fleet();
    let red = &fleet["red"];
    assert_eq!(red.as_array().map(Vec::len), Some(1), "{fleet}");
    assert_eq!(red[0]["check"], "suite", "{fleet}");
    assert_eq!(red[0]["change"], broke.as_str(), "{fleet}");
    assert_eq!(red[0]["landing"], landing.as_str(), "{fleet}");
    let suite = json!([{"check": "suite", "environment": "local"}]);
    assert_eq!(entry(&fleet, &on_src)["on_red"], suite, "{fleet}");
    assert_eq!(entry(&fleet, &on_docs)["on_red"], json!([]), "{fleet}");

    // Watch from before the failure sees it turn red, once.
    let (events, seen) = trunk.watch(Some(&green));
    let turned = of_kind(&events, "trunk_red");
    assert_eq!(turned.len(), 1, "{events:?}");
    assert_eq!(turned[0]["check"], "suite", "{events:?}");
    assert_eq!(turned[0]["change"], broke.as_str(), "{events:?}");
    assert_eq!(turned[0]["landing"], landing.as_str(), "{events:?}");
    let (again, _) = trunk.watch(Some(&seen));
    assert!(of_kind(&again, "trunk_red").is_empty(), "{again:?}");
    assert!(of_kind(&again, "trunk_red_cleared").is_empty(), "{again:?}");

    // A fix that names the owner lands; watch sees the red clear, and the map no longer lists it.
    trunk.land("src/fix.rs", Some(&broke));
    let (events, _) = trunk.watch(Some(&seen));
    let cleared = of_kind(&events, "trunk_red_cleared");
    assert_eq!(cleared.len(), 1, "{events:?}");
    assert_eq!(cleared[0]["check"], "suite", "{events:?}");
    assert_eq!(cleared[0]["landing"], landing.as_str(), "{events:?}");
    assert!(of_kind(&events, "trunk_red").is_empty(), "{events:?}");
    let fleet = trunk.fleet();
    assert_eq!(fleet["red"], json!([]), "{fleet}");
    assert_eq!(entry(&fleet, &on_src)["on_red"], json!([]), "{fleet}");
}

#[test]
fn watch_sees_a_red_clear_when_a_later_run_of_the_check_passes() {
    let trunk = trunk();
    let (_, broken) = trunk.land("src/broke.rs", None);
    trunk.suite(&broken, "product_failure");
    // Without a cursor, a red check is new.
    let (events, seen) = trunk.watch(None);
    assert_eq!(of_kind(&events, "trunk_red").len(), 1, "{events:?}");
    let (_, later) = trunk.land("docs/later.md", None);
    trunk.suite(&later, "pass");
    let (events, _) = trunk.watch(Some(&seen));
    let cleared = of_kind(&events, "trunk_red_cleared");
    assert_eq!(cleared.len(), 1, "{events:?}");
    assert_eq!(cleared[0]["landing"], broken.as_str(), "{events:?}");
}

/// A registry that cannot be read never reads as none in watch: evidence would then count
/// unsigned, so a record could clear a red trunk. The red is not judged, the cursor keeps the
/// red it held, and the result names why.
#[test]
fn watch_judges_no_red_from_a_registry_it_cannot_read() {
    let trunk = trunk();
    let (_, landing) = trunk.land("src/b.rs", None);
    trunk.suite(&landing, "product_failure");
    trunk.test_box.bay_ok(&trunk.repo, &["status"]);
    let (events, cursor) = trunk.watch(None);
    assert_eq!(of_kind(&events, "trunk_red").len(), 1, "{events:?}");

    // The registry ref names a commit whose agent file is no key list, and a pass is recorded.
    let bad = trunk.test_box.path().join("bad-registry");
    std::fs::create_dir_all(&bad).unwrap();
    let format = trunk
        .test_box
        .git_ok(&trunk.repo, &["rev-parse", "--show-object-format"]);
    trunk.test_box.git_ok(
        &bad,
        &["init", "-q", &format!("--object-format={}", format.trim())],
    );
    std::fs::write(bad.join("agent"), "not a key line\n").unwrap();
    trunk.test_box.git_ok(&bad, &["add", "agent"]);
    trunk
        .test_box
        .git_ok(&bad, &["commit", "-q", "-m", "registry"]);
    trunk.test_box.git_ok(
        &trunk.repo,
        &[
            "fetch",
            "-q",
            bad.to_str().unwrap(),
            "HEAD:refs/bay/registry",
        ],
    );
    let (code, written) = result_json(trunk.test_box.bay(&trunk.suite_box).args([
        "evidence",
        "record",
        "--kind",
        "check",
        "--check",
        "suite",
        "--environment",
        "local",
        "--verdict",
        "pass",
        "--commit",
        &landing,
        "--version",
        "2",
    ]));
    assert_eq!(code, 0, "{written}");
    let watched = trunk
        .test_box
        .bay_ok(&trunk.repo, &["status", "--since", &cursor]);
    let detail = &watched["detail"];
    assert!(detail["red_unread"].is_string(), "{watched}");
    let events = detail["events"].as_array().unwrap();
    assert_eq!(of_kind(events, "trunk_red_cleared").len(), 0, "{watched}");
}
