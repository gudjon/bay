//! G284: an approval lands its change (owner ruling 2026-10-21).
//!
//! A change held `APPROVAL_PENDING` waits; it has not ended. Every executor pass asks it
//! again. Once an enrolled approver records `bay evidence record --kind approval` for its
//! exact candidate, one pass lands that same candidate: the author lands nothing a second
//! time, and the check whose evidence still holds does not run again. When trunk moved after
//! the approval, the pass rebuilds the change on the new trunk; under `carry = false` it waits
//! `APPROVAL_PENDING` again, naming the new candidate the approval must name (without it, an
//! approval holds while the files it covers are unchanged: G292). A pass that finds
//! nothing changed leaves it waiting. A rejection ends it `CHECK_FAILED`, and no later pass
//! runs it. The refusal states the plain next step: an executor lands it once approved, and
//! without one the author's `bay land --wait` does.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/executor.rs` `runs`: `(op.state != OpState::Blocked || publish::awaits_approval(op))`
//!   to `op.state != OpState::Blocked` (no pass asks the change again: nothing lands, no
//!   rebuild, no `CHECK_FAILED`).
//! - `src/publish.rs` `awaits_approval`: `== Some(Reason::ApprovalPending)` to `== None` (the
//!   same: a waiting change is skipped).
//! - `src/publish.rs` `retry_blocked`: drop `ctx.approved = Some(op_id.to_owned());` (the pass
//!   builds and lands a new candidate, not the approved one).
//! - `src/publish.rs` `persist_attempt`: drop `op.attempts.pop();` (the one candidate is
//!   recorded as two attempts).
//! - `src/publish.rs` `approval_detail`: drop `detail["once_approved"] = ...` (the refusal no
//!   longer says what lands it).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::talk::fresh_key;
use support::{
    AGENT, EXECUTOR, PRODUCER, REGISTRAR, TestBox, TestKeys, check_files, key_line, passing_result,
    result_json,
};

const APPROVER: &str = "approver-a";
const CHECK: &str = "billing-approval";
const APPROVAL: &str = "\n[[check]]\nid = \"billing-approval\"\nenvironment = \"people\"\nkind = \"approval\"\nwhen_paths = [\"billing/\"]\n";

struct Trunk {
    test_box: TestBox,
    repo: PathBuf,
    /// The approver's own clone, with `bay.approverName` and `bay.approverKey`.
    approver_box: PathBuf,
    /// One line per run of the `tests` check, naming the tree it ran on.
    runs: PathBuf,
}

/// The registry files: the test registry, with `approvers` as the whole approver file.
fn files(keys: &TestKeys, approvers: &str) -> Vec<(&'static str, String)> {
    vec![
        ("agent", key_line(AGENT, &keys.agent)),
        ("executor", key_line(EXECUTOR, &keys.executor)),
        ("producer", key_line(PRODUCER, &keys.producer)),
        ("approver", approvers.to_owned()),
        ("registry", key_line(REGISTRAR, &keys.registry)),
    ]
}

/// A trunk on a bay host whose policy runs `tests` on every change, counting each run, and
/// asks an approval for `billing/`; the registry enrols approver `approver-a`. The work
/// repository holds the agent, executor and producer keys, and runs the executor.
fn trunk() -> Trunk {
    trunk_with("")
}

/// [`trunk`], with `extra` keys on the approval check (G292: `carry = false` asks an approval
/// of every candidate).
fn trunk_with(extra: &str) -> Trunk {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    let runs = test_box.path().join("runs");
    let relayed = test_box.relay(&format!("echo \"$tree\" >> '{}'", runs.display()));
    let mut check = check_files(&format!("n=$({relayed})\n"), &passing_result(1), 1, 60);
    for (path, text) in &mut check {
        if path == ".bay/policy.toml" {
            text.push_str(APPROVAL);
            text.push_str(extra);
        }
    }
    test_box.seed_with_check(
        &repo,
        &[
            ("src/a.rs", "fn a() {}\n"),
            ("billing/rates.txt", "rate 1\n"),
        ],
        &check,
    );
    let keys = test_box.keys();
    let approver_key = fresh_key(&test_box, APPROVER);
    test_box.enroll_files(
        &repo,
        &keys,
        &files(&keys, &key_line(APPROVER, &approver_key)),
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
        runs,
    }
}

impl Trunk {
    /// Starts a change under `intent` writing `path`. Returns the change id and its view.
    fn start(&self, intent: &str, path: &str) -> (String, PathBuf) {
        let dir = format!("{}/", Path::new(path).parent().unwrap().display());
        let started = self
            .test_box
            .bay_ok(&self.repo, &["start", intent, "--paths", &dir]);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(path), format!("{path}\n")).unwrap();
        (id, view)
    }

    fn land(&self, view: &Path) -> (i32, serde_json::Value) {
        result_json(self.test_box.bay(view).args(["land", "--wait"]))
    }

    /// The approver reads change `id` from its own clone and records `verdict` about
    /// candidate `commit`.
    fn approve(&self, id: &str, verdict: &str, commit: &str) {
        self.test_box.bay_ok(&self.approver_box, &["status", id]);
        let (code, recorded) = result_json(self.test_box.bay(&self.approver_box).args([
            "evidence",
            "record",
            "--kind",
            "approval",
            "--check",
            CHECK,
            "--verdict",
            verdict,
            "--commit",
            commit,
        ]));
        assert_eq!(code, 0, "{recorded}");
    }

    /// One executor pass from the work repository: its row for change `id`, if it ran one.
    fn pass(&self, id: &str) -> Option<serde_json::Value> {
        let ran = self
            .test_box
            .bay_ok(&self.repo, &["executor", "run", "--once"]);
        ran["detail"]["ran"]
            .as_array()
            .unwrap_or_else(|| panic!("no ran rows: {ran}"))
            .iter()
            .find(|row| row["change"] == id)
            .cloned()
    }

    /// The commit the remote's trunk names.
    fn trunk_tip(&self) -> String {
        let listed = self
            .test_box
            .git_ok(&self.repo, &["ls-remote", "origin", "refs/heads/main"]);
        listed.split_whitespace().next().unwrap().to_owned()
    }

    /// How many times the `tests` check ran.
    fn runs(&self) -> usize {
        std::fs::read_to_string(&self.runs).map_or(0, |text| text.lines().count())
    }
}

/// The candidate commit a refusal held for `billing-approval` names, in approval state `state`.
fn held(result: &serde_json::Value, state: &str) -> String {
    let approval = &result["detail"]["approval"];
    assert_eq!(approval["checks"][0]["check"], CHECK, "{result}");
    assert_eq!(approval["checks"][0]["state"], state, "{result}");
    approval["commit"].as_str().unwrap().to_owned()
}

#[test]
fn an_approval_lands_the_approved_candidate_with_no_second_land() {
    let trunk = trunk();
    let (id, view) = trunk.start("CARD-billing", "billing/discount.txt");
    let (code, waiting) = trunk.land(&view);
    assert_eq!(code, 10, "{waiting}");
    assert_eq!(waiting["reason"], "APPROVAL_PENDING", "{waiting}");
    assert_eq!(waiting["retry_when"], "approved", "{waiting}");
    let next = waiting["detail"]["approval"]["once_approved"]
        .as_str()
        .unwrap_or_default();
    assert!(next.contains("executor"), "{waiting}");
    assert!(next.contains("bay land --wait"), "{waiting}");
    let candidate = held(&waiting, "pending");
    assert_eq!(trunk.runs(), 1);

    // A pass before any approval leaves it waiting, and runs nothing.
    let row = trunk
        .pass(&id)
        .expect("the pass asks the waiting change again");
    assert_eq!(row["reason"], "APPROVAL_PENDING", "{row}");
    assert_ne!(trunk.trunk_tip(), candidate, "it landed with no approval");

    // The approver says yes to that exact candidate; one pass lands it, and the author does
    // nothing more.
    trunk.approve(&id, "pass", &candidate);
    let row = trunk.pass(&id).expect("the pass runs the approved change");
    assert_eq!(row["decision"], "landed", "{row}");
    assert_eq!(
        trunk.trunk_tip(),
        candidate,
        "the approved candidate landed"
    );
    assert_eq!(trunk.runs(), 1, "the tests check ran again");

    let status = trunk.test_box.bay_ok(&trunk.repo, &["status", &id]);
    let operation = &status["detail"]["changes"][0]["operation"];
    assert_eq!(operation["published"], true, "{status}");
    assert_eq!(operation["attempts"], 1, "{status}");
}

#[test]
fn a_moved_trunk_rebuilds_and_waits_for_an_approval_of_the_new_candidate() {
    // G292: under `carry = false` an approval answers its exact candidate only.
    let trunk = trunk_with("carry = false\n");
    let (id, view) = trunk.start("CARD-billing", "billing/discount.txt");
    let (code, waiting) = trunk.land(&view);
    assert_eq!(code, 10, "{waiting}");
    let first = held(&waiting, "pending");
    trunk.approve(&id, "pass", &first);

    // Trunk moves before any pass: the approval named a candidate on the old trunk.
    let (_, other) = trunk.start("CARD-src", "src/other.rs");
    let (code, landed) = trunk.land(&other);
    assert_eq!(code, 0, "{landed}");
    let moved = trunk.trunk_tip();
    assert_eq!(trunk.runs(), 2);

    let row = trunk
        .pass(&id)
        .expect("the pass asks the waiting change again");
    assert_eq!(row["decision"], "blocked", "{row}");
    assert_eq!(row["reason"], "APPROVAL_PENDING", "{row}");
    let second = held(&row, "pending");
    assert_ne!(second, first, "the refusal names the new candidate: {row}");
    assert_eq!(
        trunk.trunk_tip(),
        moved,
        "nothing landed on the old approval"
    );
    assert_eq!(trunk.runs(), 3, "the rebuilt candidate is checked");

    // Nothing changed since: the next pass leaves it waiting and runs nothing.
    let row = trunk
        .pass(&id)
        .expect("the pass asks the waiting change again");
    assert_eq!(row["reason"], "APPROVAL_PENDING", "{row}");
    assert_eq!(trunk.trunk_tip(), moved);
    assert_eq!(trunk.runs(), 3);

    // The approval of the new candidate lands it.
    trunk.approve(&id, "pass", &second);
    let row = trunk.pass(&id).expect("the pass runs the approved change");
    assert_eq!(row["decision"], "landed", "{row}");
    assert_eq!(trunk.trunk_tip(), second);
    assert_eq!(trunk.runs(), 3);
}

#[test]
fn a_rejection_ends_the_change_check_failed() {
    let trunk = trunk();
    let (id, view) = trunk.start("CARD-billing", "billing/discount.txt");
    let (code, waiting) = trunk.land(&view);
    assert_eq!(code, 10, "{waiting}");
    let candidate = held(&waiting, "pending");
    let before = trunk.trunk_tip();

    trunk.approve(&id, "product_failure", &candidate);
    let row = trunk
        .pass(&id)
        .expect("the pass asks the waiting change again");
    assert_eq!(row["decision"], "blocked", "{row}");
    assert_eq!(row["reason"], "CHECK_FAILED", "{row}");
    let named = row["detail"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| check["check"] == CHECK && check["approver"] == APPROVER);
    assert!(named, "the rejection names its approver: {row}");
    assert_eq!(trunk.trunk_tip(), before);

    // The end is terminal: no later pass runs it.
    assert!(trunk.pass(&id).is_none(), "a rejected change ran again");
    assert_eq!(trunk.runs(), 1);
    let status = trunk.test_box.bay_ok(&trunk.repo, &["status", &id]);
    let operation = &status["detail"]["changes"][0]["operation"];
    assert_eq!(operation["outcome"], "CHECK_FAILED", "{status}");
}
