//! G244c: a red trunk is judged at scale (NEW-6).
//!
//! `[red] window = N` in `.bay/config.toml` sets how many trunk commits a red judgement reads
//! back. A post check that no landing in the window answers is `pending_beyond_window` in
//! `detail.red_pending` of `bay status`, the fleet map and `bay watch`: never green, though it
//! refuses no landing. A red check names its suspect range: the last landing it passed on, the
//! first it failed on, and the landings in between, so an operator can bisect.

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

/// G244's trunk, a post check `suite` on `src/**` whose records a runner elsewhere writes,
/// with `config` as its `.bay/config.toml`.
fn trunk(config: &str) -> Trunk {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
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
    /// Lands a change that writes `path`: its change id and its commit on trunk.
    fn land(&self, path: &str) -> (String, String) {
        let dir = Path::new(path).parent().unwrap().to_str().unwrap();
        let started = self
            .test_box
            .bay_ok(&self.repo, &["start", path, "--paths", dir]);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(path), format!("{path}\n")).unwrap();
        let (code, landed) = result_json(self.test_box.bay(&view).args(["land", "--wait"]));
        assert_eq!(code, 0, "{landed}");
        (id, landed["detail"]["commit"].as_str().unwrap().to_owned())
    }

    /// Five landings on `src/`, oldest first.
    fn five(&self) -> Vec<(String, String)> {
        (1..=5)
            .map(|n| self.land(&format!("src/l{n}.rs")))
            .collect()
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

    fn status(&self) -> Value {
        self.test_box.bay_ok(&self.repo, &["status"])["detail"].clone()
    }
}

#[test]
fn a_post_check_answered_only_behind_the_window_is_pending_never_green() {
    let trunk = trunk("[red]\nwindow = 3\n");
    let landings = trunk.five();
    trunk.suite(&landings[0].1, "pass");

    // No landing of the three judged answers the check: not red, and never clear.
    let status = trunk.status();
    assert_eq!(status["red"], json!([]), "{status}");
    let pending = &status["red_pending"];
    assert_eq!(pending.as_array().map(Vec::len), Some(1), "{status}");
    assert_eq!(pending[0]["check"], "suite", "{status}");
    assert_eq!(pending[0]["state"], "pending_beyond_window", "{status}");
    assert_eq!(pending[0]["window"], 3, "{status}");
    assert_eq!(pending[0]["oldest"], landings[2].1.as_str(), "{status}");
    // The unattributed scan reads the same window.
    assert_eq!(status["unattributed"]["window"], 3, "{status}");

    // The fleet map and watch say the same.
    let fleet = trunk.test_box.bay_ok(&trunk.repo, &["status", "--fleet"]);
    assert_eq!(fleet["detail"]["red"], json!([]), "{fleet}");
    assert_eq!(fleet["detail"]["red_pending"], *pending, "{fleet}");
    let watched = trunk.test_box.bay_ok(&trunk.repo, &["watch"]);
    assert_eq!(watched["detail"]["red_pending"], *pending, "{watched}");

    // Pending is unknown, not a failure: a landing on its paths still lands.
    let (_, sixth) = trunk.land("src/l6.rs");

    // An answer inside the window decides it, and nothing is pending.
    trunk.suite(&sixth, "pass");
    let status = trunk.status();
    assert_eq!(status["red"], json!([]), "{status}");
    assert_eq!(status["red_pending"], json!([]), "{status}");
}

#[test]
fn a_red_post_check_names_the_landings_between_its_last_pass_and_first_failure() {
    let trunk = trunk("");
    let landings = trunk.five();
    // The window holds all of trunk, and nothing has answered what its landings require.
    let status = trunk.status();
    assert_eq!(status["red"], json!([]), "{status}");
    assert_eq!(status["red_pending"][0]["state"], "pending", "{status}");

    trunk.suite(&landings[1].1, "pass");
    trunk.suite(&landings[3].1, "product_failure");

    let status = trunk.status();
    assert_eq!(status["red_pending"], json!([]), "{status}");
    let red = &status["red"];
    assert_eq!(red.as_array().map(Vec::len), Some(1), "{status}");
    assert_eq!(red[0]["landing"], landings[3].1.as_str(), "{status}");
    let suspect = &red[0]["suspect"];
    let mark = |(change, commit): &(String, String)| json!({"landing": commit, "change": change});
    assert_eq!(suspect["last_passed"], mark(&landings[1]), "{status}");
    assert_eq!(suspect["first_failed"], mark(&landings[3]), "{status}");
    assert_eq!(
        suspect["landings"],
        json!([mark(&landings[2]), mark(&landings[3])]),
        "{status}"
    );
}
