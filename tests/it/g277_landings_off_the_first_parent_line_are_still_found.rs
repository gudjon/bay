//! G277 (N-20): landings an outside merge pushed off the first-parent line are still found.
//!
//! On a forge, someone outside bay can push a merge whose first parent is an old trunk commit
//! and whose second parent is trunk's tip: bay's landings then sit only behind the merge's
//! second parent. bay finds a landing by its marker and checks it by ancestry, and its readers
//! that need a linear order read the landing line (`crate::trunkline`): the first-parent line
//! with each outside merge followed by the bay landings it brought in. Here A and B land
//! through bay, and a merge M (first parent: the commit before A; second parent: B) is pushed
//! with plain Git to a plain bare remote. Then:
//!
//! - `bay status` warns `trunk_not_linear`, naming M, its parents and the 2 landings it pushed
//!   off, with the hint to require linear history; `bay status --since` names M once the same way;
//! - a `[[target]]` at B reads B as on its source's line, with no failure;
//! - `bay undo A` lands its revert on the tip;
//! - a post check that passed at A and failed at B is red, owned by B, B the culprit.
//!
//! Guards to mutate, each of which fails this test:
//!
//! - the merge expansion of the shared helper, `src/trunkline.rs` `walk`:
//!   `if !expansion.landings.is_empty() {` to `if false {` (no warning, no event, and the red
//!   is owned by M, not B);
//! - the ancestry check in `src/undo.rs` `landed_commit`: `Ok(true)` to `Ok(false)` in the
//!   `is_ancestor` match (the undo of A is refused `CHANGE_NOT_MERGED`);
//! - `src/target.rs`: `crate::trunkline::on_line(git, from, tip)?` back to
//!   `on_first_parent(git, from, tip)?` in `Read::row` (B reads as off its source).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::talk::fresh_key;
use support::{TestBox, check_files, passing_result, result_json};

const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

const CONFIG: &str = "main = \"main\"\n\n[[target]]\nname = \"staging\"\n";

struct Trunk {
    test_box: TestBox,
    repo: PathBuf,
    remote: PathBuf,
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
    files.push((".bay/config.toml".to_owned(), CONFIG.to_owned()));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
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
        remote,
        suite_box,
    }
}

impl Trunk {
    /// Starts a change writing `path` and lands it: its change id and landed commit.
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

    fn remote_git(&self, args: &[&str]) -> String {
        self.test_box.git_bare(&self.remote, args)
    }

    /// Pushes `refspec` to the remote with plain Git: the forge takes any push.
    fn push(&self, refspec: &str) {
        self.test_box
            .git_ok(&self.repo, &["push", "-q", "origin", refspec]);
    }
}

fn not_linear(warnings: &Value) -> Vec<&Value> {
    warnings
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter(|row| {
                    row["code"] == "trunk_not_linear" || row["kind"] == "trunk_not_linear"
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
#[allow(clippy::too_many_lines)]
fn landings_an_outside_merge_pushed_off_the_first_parent_line_are_still_found() {
    let trunk = trunk();
    let test_box = &trunk.test_box;
    let (a, landed_a) = trunk.land("src/one.rs");
    let (b, landed_b) = trunk.land("src/two.rs");
    let watched = test_box.bay_ok(&trunk.repo, &["status", "--since"]);
    let cursor = watched["detail"]["cursor"].as_str().unwrap().to_owned();

    // Outside bay, a merge whose first parent is the commit before A and whose second parent
    // is B, with B's tree, pushed straight to trunk.
    test_box.git_ok(&trunk.repo, &["fetch", "-q", "origin"]);
    let old = test_box.git_ok(&trunk.repo, &["rev-parse", &format!("{landed_a}^")]);
    let merge = test_box.git_ok(
        &trunk.repo,
        &[
            "commit-tree",
            &format!("{landed_b}^{{tree}}"),
            "-p",
            &old,
            "-p",
            &landed_b,
            "-m",
            "Merge the trunk into an old base",
        ],
    );
    trunk.push(&format!("+{merge}:refs/heads/main"));
    let line = trunk.remote_git(&["rev-list", "--first-parent", "main"]);
    assert!(
        !line.contains(&landed_a) && !line.contains(&landed_b),
        "A and B are off the first-parent line: {line}"
    );

    // `bay status` names the merge, its parents and the two landings it pushed off.
    let status = test_box.bay_ok(&trunk.repo, &["status"]);
    let warned = not_linear(&status["detail"]["warnings"]);
    assert_eq!(warned.len(), 1, "{status}");
    let warning = warned[0];
    assert_eq!(warning["commit"], merge.as_str(), "{warning}");
    assert_eq!(warning["parents"], json!([&old, &landed_b]), "{warning}");
    assert_eq!(warning["landings"], 2, "{warning}");
    assert_eq!(
        warning["landing_commits"],
        json!([&landed_b, &landed_a]),
        "{warning}"
    );
    assert_eq!(warning["capped"], false, "{warning}");
    assert!(
        warning["note"]
            .as_str()
            .is_some_and(|note| note.contains("linear history")),
        "{warning}"
    );
    // The merge is still an unattributed landing, and names the landings it pushed off; the
    // landings themselves are not unattributed.
    let unattributed = &status["detail"]["unattributed"];
    assert_eq!(unattributed["count"], 1, "{unattributed}");
    assert_eq!(
        unattributed["landings"][0]["commit"],
        merge.as_str(),
        "{unattributed}"
    );
    assert_eq!(
        unattributed["landings"][0]["landings_off_first_parent"], 2,
        "{unattributed}"
    );

    // `bay status --since` names the merge once.
    let watched = test_box.bay_ok(&trunk.repo, &["status", "--since", &cursor]);
    let events = not_linear(&watched["detail"]["events"]);
    assert_eq!(events.len(), 1, "{watched}");
    assert_eq!(events[0]["commit"], merge.as_str(), "{watched}");
    assert_eq!(events[0]["landings"], 2, "{watched}");
    let next = watched["detail"]["cursor"].as_str().unwrap().to_owned();
    let again = test_box.bay_ok(&trunk.repo, &["status", "--since", &next]);
    assert!(
        not_linear(&again["detail"]["events"]).is_empty(),
        "named once: {again}"
    );

    // A target at B reads B as on its source's line.
    trunk.push(&format!("{landed_b}:refs/heads/staging"));
    let status = test_box.bay_ok(&trunk.repo, &["status"]);
    let staging = status["detail"]["targets"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["name"] == "staging"))
        .unwrap_or_else(|| panic!("{status}"));
    assert_eq!(staging["tip"], landed_b.as_str(), "{staging}");
    assert_eq!(staging["on_from"], true, "{staging}");
    assert_eq!(staging["tip_failures"], json!([]), "{staging}");

    // `bay undo A` reverts A on the tip, above the merge.
    let (code, undone) = result_json(test_box.bay(&trunk.repo).args(["undo", &a, "--wait"]));
    assert_eq!(code, 0, "{undone}");
    assert_eq!(undone["decision"], "merged", "{undone}");
    assert_eq!(trunk.remote_git(&["rev-parse", "main^"]), merge, "{undone}");
    let reverts = trunk.remote_git(&[
        "log",
        "-1",
        "--format=%(trailers:key=Reverts,valueonly)",
        "main",
    ]);
    assert_eq!(reverts.trim(), a, "{undone}");
    let files = trunk.remote_git(&["ls-tree", "-r", "--name-only", "main", "--", "src"]);
    assert!(!files.contains("src/one.rs"), "{files}");
    assert!(files.contains("src/two.rs"), "{files}");

    // The suite passed at A and failed at B: the red is B's, through the landing line.
    trunk.suite(&landed_a, "pass");
    trunk.suite(&landed_b, "product_failure");
    let status = test_box.bay_ok(&trunk.repo, &["status"]);
    let red = &status["detail"]["red"];
    assert_eq!(red[0]["check"], "suite", "{red}");
    assert_eq!(red[0]["change"], b.as_str(), "{red}");
    assert_eq!(red[0]["landing"], landed_b.as_str(), "{red}");
    assert_eq!(red[0]["culprit"]["landing"], landed_b.as_str(), "{red}");
    assert_eq!(
        red[0]["suspect"]["last_passed"]["landing"],
        landed_a.as_str(),
        "{red}"
    );
}
