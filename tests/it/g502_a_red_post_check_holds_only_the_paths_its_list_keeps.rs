//! G502 (B1, G500 for a red trunk). A red post check holds only the paths its `when_paths`
//! list keeps.
//!
//! Trunk is red on a failed post check's paths (G244), read as its `when_paths` are: with
//! `["src/**", "!src/docs/**"]` a landing on `src/docs/` lands while the check is red, and a
//! landing on the rest of `src/` waits `MAIN_RED`.
//!
//! The guard to mutate: `Red::covers` in `src/red.rs` (back to any glob: the landing on the
//! excluded path waits `MAIN_RED`; to `false`: the landing on `src/` lands on the red).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::talk::fresh_key;
use support::{TestBox, check_files, passing_result, result_json};

const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\", \"!src/docs/**\"]\n";

struct Trunk {
    test_box: TestBox,
    repo: PathBuf,
    suite_box: PathBuf,
}

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
        &[("src/a.rs", "fn a() {}\n"), ("src/docs/a.md", "a\n")],
        &files,
    );
    // The suite runs elsewhere and writes its records as producer `suite-1`.
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
    /// Starts a change writing `path` and lands it.
    fn land(&self, path: &str) -> (i32, serde_json::Value) {
        let dir = format!("{}/", Path::new(path).parent().unwrap().display());
        let started = self
            .test_box
            .bay_ok(&self.repo, &["start", path, "--paths", &dir]);
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(path), format!("{path}\n")).unwrap();
        result_json(self.test_box.bay(&view).args(["merge", "--wait"]))
    }

    fn fail_suite(&self, landing: &str) {
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
            "product_failure",
            "--commit",
            landing,
        ]));
        assert_eq!(code, 0, "{written}");
    }
}

#[test]
fn a_landing_on_an_excluded_path_lands_while_the_check_is_red() {
    let trunk = trunk();
    let (code, landed) = trunk.land("src/broke.rs");
    assert_eq!(code, 0, "{landed}");
    trunk.fail_suite(landed["detail"]["commit"].as_str().unwrap());
    let red = trunk.test_box.bay_ok(&trunk.repo, &["status"])["detail"]["red"].clone();
    assert_eq!(red[0]["check"], "suite", "{red}");

    // The exclusion keeps `src/docs/` off the red.
    let (code, docs) = trunk.land("src/docs/b.md");
    assert_eq!(code, 0, "{docs}");
    assert_eq!(docs["decision"], "merged", "{docs}");

    // The rest of `src/` waits for main.
    let (code, waiting) = trunk.land("src/next.rs");
    assert_eq!(code, 0, "{waiting}");
    assert_eq!(waiting["reason"], "MAIN_RED", "{waiting}");
}
