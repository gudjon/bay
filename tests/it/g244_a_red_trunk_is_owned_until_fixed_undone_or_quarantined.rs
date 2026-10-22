//! G244: a red trunk is owned until it is fixed, undone or quarantined (the pilot-gaps analysis
//! of 2026-10-20).
//!
//! A post-publication check that fails on a landing makes trunk red on that check's paths,
//! owned by the landing. `bay status` names it. While red, a landing that touches those paths
//! is refused `MAIN_RED`, naming the owner; a landing elsewhere lands, and a change that names
//! the owner (`--fixes`) lands and clears the red.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::talk::fresh_key;
use support::{TestBox, check_files, passing_result, result_json};

const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

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
        &[("src/a.rs", "fn a() {}\n"), ("docs/a.md", "a\n")],
        &files,
    );
    // The broad suite runs elsewhere and writes its records as producer `suite-1`.
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
    /// Starts a change writing `path` (`--fixes <fixes>` when given) and lands it.
    fn land(&self, path: &str, fixes: Option<&str>) -> (i32, serde_json::Value, String) {
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
        let (code, result) = result_json(self.test_box.bay(&view).args(["merge", "--wait"]));
        (code, result, id)
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

    fn red(&self) -> serde_json::Value {
        self.test_box.bay_ok(&self.repo, &["status"])["detail"]["red"].clone()
    }
}

#[test]
fn a_failed_post_check_makes_its_paths_red_until_a_fix_lands() {
    let trunk = trunk();
    let (code, landed, broke) = trunk.land("src/broke.rs", None);
    assert_eq!(code, 0, "{landed}");
    let landing = landed["detail"]["commit"].as_str().unwrap().to_owned();
    assert_eq!(
        trunk.red(),
        serde_json::json!([]),
        "green before any post record"
    );

    trunk.suite(&landing, "product_failure");
    let red = trunk.red();
    assert_eq!(red[0]["check"], "suite", "{red}");
    assert_eq!(red[0]["change"], broke.as_str(), "{red}");
    assert_eq!(red[0]["landing"], landing.as_str(), "{red}");

    // A landing on the red paths is refused, naming the owner.
    let (code, refused, _) = trunk.land("src/next.rs", None);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "MAIN_RED", "{refused}");
    assert_eq!(
        refused["detail"]["red"][0]["change"],
        broke.as_str(),
        "{refused}"
    );

    // A landing elsewhere lands.
    let (code, elsewhere, _) = trunk.land("docs/other.md", None);
    assert_eq!(
        code, 0,
        "paths the check does not cover stay open: {elsewhere}"
    );

    // A fix that names the owner lands, and clears the red.
    let (code, fixed, _) = trunk.land("src/fix.rs", Some(&broke));
    assert_eq!(code, 0, "{fixed}");
    assert_eq!(trunk.red(), serde_json::json!([]), "the fix answered it");
    let (code, after, _) = trunk.land("src/after.rs", None);
    assert_eq!(code, 0, "{after}");
}

#[test]
fn a_later_pass_of_the_post_check_turns_trunk_green() {
    let trunk = trunk();
    let (_, landed, _) = trunk.land("src/broke.rs", None);
    let broken = landed["detail"]["commit"].as_str().unwrap().to_owned();
    trunk.suite(&broken, "product_failure");
    let (code, elsewhere, _) = trunk.land("docs/later.md", None);
    assert_eq!(code, 0, "{elsewhere}");
    let later = elsewhere["detail"]["commit"].as_str().unwrap().to_owned();
    // The suite runs again on the newer trunk, and passes.
    trunk.suite(&later, "pass");
    assert_eq!(trunk.red(), serde_json::json!([]));
}
