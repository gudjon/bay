//! G571 (a fleet finding, 2026-10-29: a red post check held every change on its paths, and
//! nothing told the session whose merge first failed it). A runner pass that finds a red post
//! check of trunk writes one `red_owner` event to its log, addressed to the change that owns the
//! red: the culprit when the evidence pins one, else the first failure. The event names the
//! check and the exact fix to run, the wake adapter is asked once to wake the change's session
//! (`cause: "red"`, `urgent`), and the owner's session hears it as one notice line. A second
//! pass over the same red writes nothing and wakes no one.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/red_owner.rs` `told`: drop the `held.contains(&id)` skip, and the second pass writes a
//!   second event and wakes again.
//! - `src/talk/wake.rs` `red_request`: make `"cause": "red"` `"outcome"`, and the wake case fails.
//! - `src/notice.rs` `KINDS`: drop `"red_owner"`, and the session hears nothing.
//! - `src/executor.rs` `pass`: drop `notes.extend(red_owners(repo))`, and no event is written.

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::Value;
use support::talk::fresh_key;
use support::{TestBox, check_files, passing_result, result_json, write_executable};

/// The post check `suite` on `src/**`, on the shared adapter with `suite` as its argument.
const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check suite\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

struct Trunk {
    test_box: TestBox,
    repo: PathBuf,
    suite_box: PathBuf,
    woken: PathBuf,
}

/// A trunk whose pre check `tests` always passes and whose post check `suite` a producer
/// elsewhere, `suite-1`, answers by hand. Its wake adapter `scripts/bay-wake` appends each
/// request to `<root>/woken`.
fn trunk() -> Trunk {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let woken = test_box.path().join("woken");
    std::fs::create_dir_all(repo.join("scripts")).unwrap();
    write_executable(
        &repo.join("scripts/bay-wake"),
        &format!(
            "#!/bin/sh\ncat >> '{}'\necho >> '{}'\nprintf '{{\"schema\":\"bay.wake-result/1\",\"result\":\"woken\",\"reason\":null}}'\n",
            woken.display(),
            woken.display()
        ),
    );
    let mut files = check_files("", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(POST);
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[plugin.wake]\ncommand = \"scripts/bay-wake\"\n".to_owned(),
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
        suite_box,
        woken,
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

    /// One runner pass on the trunk's box: the `red_owners` it told.
    fn pass(&self) -> Vec<Value> {
        let ran = self
            .test_box
            .bay_ok(&self.repo, &["runner", "run", "--once"]);
        ran["detail"]["red_owners"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// The `red` wake requests the adapter was given.
    fn red_wakes(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.woken)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|request| request["cause"] == "red")
            .collect()
    }

    /// The `red_owner` events `bay status --since <cursor>` lists.
    fn red_owner_events(&self, cursor: &str) -> Vec<Value> {
        let watched = self
            .test_box
            .bay_ok(&self.repo, &["status", "--since", cursor]);
        watched["detail"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["kind"] == "red_owner")
            .cloned()
            .collect()
    }

    /// `bay hook <event>` for session `session` in the trunk's checkout: its notice text.
    fn hook(&self, event: &str, fixture: &str, session: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/claude-code-2.1.302")
            .join(fixture);
        let mut payload: Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        payload["session_id"] = Value::from(session);
        payload["cwd"] = Value::from(self.repo.display().to_string());
        let mut child = self
            .test_box
            .bay_process(&self.repo)
            .args(["hook", event])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        let out = String::from_utf8(output.stdout).unwrap();
        if out.trim().is_empty() {
            return String::new();
        }
        let parsed: Value = serde_json::from_str(&out).unwrap_or_else(|_| panic!("{out:?}"));
        parsed["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }
}

#[test]
fn a_red_post_check_tells_the_change_that_owns_it_once() {
    let trunk = trunk();
    // The owner's session is live before the red, so its notice cursor is older than the event.
    let _ = trunk.hook("session.start", "01-SessionStart.json", "s-red");
    let cursor = trunk.test_box.bay_ok(&trunk.repo, &["status", "--since"])["detail"]["cursor"]
        .as_str()
        .unwrap()
        .to_owned();

    // GIVEN three landings on `src/`, the suite passing on the first and failing on the third.
    let landings: Vec<(String, String)> = (0..3)
        .map(|n| trunk.land(&format!("src/l{n}.rs")))
        .collect();
    trunk.suite(&landings[0].1, "pass");
    trunk.suite(&landings[2].1, "product_failure");
    let status = trunk.test_box.bay_ok(&trunk.repo, &["status"]);
    let red = &status["detail"]["red"];
    assert_eq!(red.as_array().map(Vec::len), Some(1), "{status}");
    let owner = red[0]["fixes"].as_str().unwrap().to_owned();
    assert!(
        landings.iter().any(|(id, _)| *id == owner),
        "the red's owner is one of the landings: {red}"
    );

    // WHEN a runner passes THEN it tells the owner once: one note, one event, one wake.
    let told = trunk.pass();
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0]["red_owner"], owner.as_str(), "{told:?}");
    assert_eq!(told[0]["check"], "suite", "{told:?}");
    let events = trunk.red_owner_events(&cursor);
    assert_eq!(events.len(), 1, "{events:?}");
    let event = &events[0];
    assert_eq!(event["change"], owner.as_str(), "{event}");
    assert_eq!(event["check"], "suite", "{event}");
    assert_eq!(event["id"], told[0]["event"], "{event}");
    assert_eq!(event["culprit"], red[0]["culprit"].is_object(), "{event}");
    let fix = event["fix"].as_str().unwrap();
    assert!(fix.starts_with("bay start "), "{event}");
    assert!(fix.ends_with(&format!(" --fixes {owner}")), "{event}");
    assert!(
        event["line"]
            .as_str()
            .unwrap()
            .contains("main is red on suite"),
        "{event}"
    );
    assert_eq!(event["wake"]["result"], "woken", "{event}");
    let wakes = trunk.red_wakes();
    assert_eq!(wakes.len(), 1, "{wakes:?}");
    let wake = &wakes[0];
    assert_eq!(wake["schema"], "bay.wake/1", "{wake}");
    assert_eq!(wake["change"], owner.as_str(), "{wake}");
    assert_eq!(wake["check"], "suite", "{wake}");
    assert_eq!(wake["event"], event["id"], "{wake}");
    assert_eq!(wake["urgent"], true, "{wake}");
    assert_eq!(wake["actor"]["name"], event["actor"]["name"], "{wake}");

    // AND the owner's session hears it as one line, once.
    let heard = trunk.hook("turn.start", "02-UserPromptSubmit.json", "s-red");
    let line = heard
        .lines()
        .find(|line| line.contains("main is red"))
        .unwrap_or_else(|| panic!("no red notice: {heard:?}"));
    assert!(
        line.starts_with(&format!(
            "bay: change {owner}: main is red on suite; your change "
        )),
        "{line}"
    );
    assert!(
        line.ends_with(&format!("bay start <intent> --fixes {owner}")),
        "{line}"
    );
    let again = trunk.hook("turn.start", "02-UserPromptSubmit.json", "s-red");
    assert!(!again.contains("main is red"), "{again:?}");

    // WHEN a second runner pass sees the same red THEN it writes nothing and wakes no one.
    assert_eq!(trunk.pass(), Vec::<Value>::new());
    assert_eq!(trunk.red_owner_events(&cursor).len(), 1);
    assert_eq!(trunk.red_wakes().len(), 1);
}

#[test]
fn a_green_trunk_tells_no_one() {
    let trunk = trunk();
    let landing = trunk.land("src/l0.rs");
    trunk.suite(&landing.1, "pass");
    assert_eq!(trunk.pass(), Vec::<Value>::new());
    assert_eq!(trunk.red_wakes(), Vec::<Value>::new());
}
