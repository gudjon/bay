//! G531 (bay 0.6.1, B5). The read back for a red post check's last pass is bounded by
//! `[red] reach` main commits (default 250, never less than the window). With no pass within
//! it, the row says so: `last_passed: null` and `searched`, how many main commits it read back,
//! never a guess at a range end. A pass at the reach's oldest commit is found.
//!
//! Guards to mutate, in `src/red.rs`: `Answers::past` reads one commit past the reach (the pass
//! beyond it is found); `Answers::suspect` gives no `searched` depth (the row says nothing of
//! how far it read).

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
fn a_pass_one_commit_past_the_reach_is_not_found_and_the_row_says_how_far_it_read() {
    // Window 1, reach 4: main's tip and the three merges below it.
    let trunk = trunk("[red]\nwindow = 1\nreach = 4\n");
    let landings: Vec<(String, String)> = (0..5)
        .map(|n| trunk.land(&format!("src/l{n}.rs")))
        .collect();
    // The pass is on the fifth main commit from the tip, one past the reach.
    trunk.suite(&landings[0].1, "pass");
    trunk.suite(&landings[4].1, "product_failure");

    let red = trunk.red();
    assert_eq!(red.as_array().map(Vec::len), Some(1), "{red}");
    let suspect = &red[0]["suspect"];
    assert_eq!(suspect["last_passed"], Value::Null, "{red}");
    assert_eq!(suspect["searched"], 4, "it read back the reach: {red}");
    assert_eq!(red[0]["culprit"], Value::Null, "{red}");
    let say = red[0]["say"].as_str().unwrap_or_default();
    assert!(
        say.contains("newest 4"),
        "the row says how far it read: {red}"
    );

    // A pass at the reach's oldest commit is the range's good end.
    trunk.suite(&landings[1].1, "pass");
    let red = trunk.red();
    let suspect = &red[0]["suspect"];
    assert_eq!(suspect["last_passed"], mark(&landings[1]), "{red}");
    assert_eq!(suspect["searched"], Value::Null, "{red}");
    assert_eq!(
        suspect["merges"],
        json!([mark(&landings[2]), mark(&landings[3]), mark(&landings[4])]),
        "{red}"
    );
}

#[test]
fn a_window_that_holds_all_of_main_reads_nothing_past_it() {
    // The window holds all of main, so there is nothing past it to read.
    let trunk = trunk("[red]\nwindow = 40\n");
    let landings: Vec<(String, String)> = (0..3)
        .map(|n| trunk.land(&format!("src/l{n}.rs")))
        .collect();
    trunk.suite(&landings[2].1, "product_failure");
    let red = trunk.red();
    let suspect = &red[0]["suspect"];
    assert_eq!(suspect["last_passed"], Value::Null, "{red}");
    trunk
        .test_box
        .git_ok(&trunk.repo, &["fetch", "-q", "origin", "main"]);
    let depth: u64 = trunk
        .test_box
        .git_ok(
            &trunk.repo,
            &[
                "rev-list",
                "--first-parent",
                "--count",
                "refs/remotes/origin/main",
            ],
        )
        .trim()
        .parse()
        .unwrap();
    assert_eq!(suspect["searched"], json!(depth), "{red}");
}
