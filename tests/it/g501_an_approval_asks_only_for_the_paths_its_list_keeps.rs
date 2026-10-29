//! G501 (B1, G500 for approvals). An approval check asks a person only for the paths its
//! `when_paths` list keeps.
//!
//! With `when_paths = ["billing/", "!billing/notes/"]` a change to `billing/notes/` needs no
//! approval; a change to both lists only the gated path as what the person approves (G280,
//! `detail.approval.checks[].paths`); and an approval holds over a landing on the excluded
//! paths, since its footprint stamp reads the list with its exclusion (G292), and an approval
//! set aside names only the gated paths that changed.
//!
//! The guards to mutate: `CheckEntry::woken_paths` in `src/policy.rs` (back to any glob: the
//! approved paths list the excluded file), and in `src/throughput.rs` `read_digest` (back to
//! any glob: the stamp covers the excluded file, so the landing on it sets the approval aside)
//! and `gated_changes` (back to any glob: the set-aside approval names the excluded file).

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::talk::fresh_key;
use support::{
    AGENT, EXECUTOR, PRODUCER, REGISTRAR, TestBox, check_files, key_line, passing_result,
    result_json,
};

const APPROVER: &str = "approver-a";
const CHECK: &str = "billing-approval";
const APPROVAL: &str = "\n[[check]]\nid = \"billing-approval\"\nenvironment = \"people\"\nkind = \"approval\"\nwhen_paths = [\"billing/\", \"!billing/notes/\"]\n";

struct Trunk {
    test_box: TestBox,
    repo: PathBuf,
    /// The approver's own clone, with `bay.approverName` and `bay.approverKey`.
    approver_box: PathBuf,
}

/// A trunk on a bay host whose policy runs `tests` on every change and asks an approval for
/// `billing/` but `billing/notes/`; the registry enrols approver `approver-a`. The work
/// repository holds the agent, executor and producer keys, and runs the executor.
fn trunk() -> Trunk {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    let mut check = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut check {
        if path == ".bay/policy.toml" {
            text.push_str(APPROVAL);
        }
    }
    test_box.seed_with_check(
        &repo,
        &[
            ("src/a.rs", "fn a() {}\n"),
            ("billing/rates.txt", "rate 1\n"),
            ("billing/notes/memo.txt", "memo 1\n"),
        ],
        &check,
    );
    let keys = test_box.keys();
    let approver_key = fresh_key(&test_box, APPROVER);
    test_box.enroll_files(
        &repo,
        &keys,
        &[
            ("agent", key_line(AGENT, &keys.agent)),
            ("runner", key_line(EXECUTOR, &keys.executor)),
            ("producer", key_line(PRODUCER, &keys.producer)),
            ("approver", key_line(APPROVER, &approver_key)),
            ("registry", key_line(REGISTRAR, &keys.registry)),
        ],
    );
    test_box.use_keys(&repo, &keys);
    let approver_box = test_box.clone_of(&host, "approver-box");
    test_box.git_ok(&approver_box, &["config", "bay.approverName", APPROVER]);
    test_box.git_ok(
        &approver_box,
        &["config", "bay.approverKey", approver_key.to_str().unwrap()],
    );
    Trunk {
        test_box,
        repo,
        approver_box,
    }
}

impl Trunk {
    /// Starts a change under `intent` on `billing/` writing each of `paths`, and lands it.
    /// Returns the change id, its view and the land result.
    fn land(&self, intent: &str, paths: &[&str]) -> (String, PathBuf, i32, Value) {
        let started = self
            .test_box
            .bay_ok(&self.repo, &["start", intent, "--paths", "billing/"]);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        for path in paths {
            std::fs::write(view.join(path), format!("{path} two\n")).unwrap();
        }
        let (code, result) = self.merge(&view);
        (id, view, code, result)
    }

    fn merge(&self, view: &Path) -> (i32, Value) {
        result_json(self.test_box.bay(view).args(["merge", "--wait"]))
    }

    /// The approver reads change `id` from its own clone and says yes to candidate `commit`.
    fn approve(&self, id: &str, commit: &str) {
        self.test_box.bay_ok(&self.approver_box, &["status", id]);
        let (code, recorded) = result_json(self.test_box.bay(&self.approver_box).args([
            "evidence",
            "record",
            "--kind",
            "approval",
            "--check",
            CHECK,
            "--verdict",
            "pass",
            "--commit",
            commit,
        ]));
        assert_eq!(code, 0, "{recorded}");
    }

    /// One executor pass from the work repository: its row for change `id`, if it ran one.
    fn pass(&self, id: &str) -> Option<Value> {
        let ran = self
            .test_box
            .bay_ok(&self.repo, &["runner", "run", "--once"]);
        ran["detail"]["ran"]
            .as_array()
            .unwrap_or_else(|| panic!("no ran rows: {ran}"))
            .iter()
            .find(|row| row["change"] == id)
            .cloned()
    }
}

/// The candidate an `APPROVAL_PENDING` refusal names, and the paths its approval row lists.
fn pending(result: &Value) -> (String, Vec<String>) {
    assert_eq!(result["reason"], "APPROVAL_PENDING", "{result}");
    let approval = &result["detail"]["approval"];
    assert_eq!(approval["checks"][0]["check"], CHECK, "{result}");
    let paths = approval["checks"][0]["paths"]
        .as_array()
        .unwrap_or_else(|| panic!("no approved paths: {result}"))
        .iter()
        .map(|path| path.as_str().unwrap().to_owned())
        .collect();
    (approval["commit"].as_str().unwrap().to_owned(), paths)
}

#[test]
fn an_approval_lists_only_the_paths_its_list_keeps() {
    let trunk = trunk();
    let (_, _, code, waiting) = trunk.land(
        "CARD-both",
        &["billing/rates.txt", "billing/notes/memo.txt"],
    );
    assert_eq!(code, 10, "{waiting}");
    let (_, paths) = pending(&waiting);
    assert_eq!(paths, vec!["billing/rates.txt".to_owned()], "{waiting}");
}

#[test]
fn a_change_only_to_excluded_paths_needs_no_approval() {
    let trunk = trunk();
    let (_, _, code, landed) = trunk.land("CARD-notes", &["billing/notes/memo.txt"]);
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
}

#[test]
fn an_approval_carries_over_a_landing_on_excluded_paths() {
    let trunk = trunk();
    let (id, _, code, waiting) = trunk.land("CARD-rates", &["billing/rates.txt"]);
    assert_eq!(code, 10, "{waiting}");
    let (first, _) = pending(&waiting);
    trunk.approve(&id, &first);

    // Trunk moves on an excluded file, before any pass: the approval's stamp does not read it.
    let (_, _, code, landed) = trunk.land("CARD-notes", &["billing/notes/memo.txt"]);
    assert_eq!(code, 0, "{landed}");

    let row = trunk
        .pass(&id)
        .expect("the pass asks the waiting change again");
    assert_eq!(row["decision"], "merged", "{row}");
    assert_eq!(
        row["detail"]["carried_approvals"][0]["commit"],
        first.as_str(),
        "it landed on the approval it had: {row}"
    );
}

#[test]
fn a_set_aside_approval_names_only_the_gated_paths_that_changed() {
    let trunk = trunk();
    let (id, _, code, waiting) = trunk.land("CARD-discount", &["billing/discount.txt"]);
    assert_eq!(code, 10, "{waiting}");
    let (first, _) = pending(&waiting);
    trunk.approve(&id, &first);

    // Another change edits a gated file and an excluded one, and lands approved.
    let (rates, view, code, waiting) = trunk.land(
        "CARD-rates",
        &["billing/rates.txt", "billing/notes/memo.txt"],
    );
    assert_eq!(code, 10, "{waiting}");
    let (candidate, _) = pending(&waiting);
    trunk.approve(&rates, &candidate);
    let (code, landed) = trunk.merge(&view);
    assert_eq!(code, 0, "{landed}");

    // The first approval covered the old `billing/rates.txt`, not the excluded memo.
    let row = trunk
        .pass(&id)
        .expect("the pass asks the waiting change again");
    assert_eq!(row["decision"], "blocked", "{row}");
    let set_aside: Vec<&str> = row["detail"]["approval"]["checks"][0]["set_aside"][0]["paths"]
        .as_array()
        .unwrap_or_else(|| panic!("no set-aside approval: {row}"))
        .iter()
        .map(|path| path.as_str().unwrap())
        .collect();
    assert_eq!(set_aside, ["billing/rates.txt"], "{row}");
}
