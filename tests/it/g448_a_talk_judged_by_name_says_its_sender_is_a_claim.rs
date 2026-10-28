//! G448 (A3 with G445; a fleet's forged-name incident, 2026-10-27). A talk judged by name, not by
//! key, says so where its recipient reads it.
//!
//! Under `[policy] signing = "approvals"` a talk is sent and acked unsigned, its sender named by
//! `BAY_ACTOR` (G445): anyone who can push may write any name. The arrival notice names the
//! sender, so it says the name is a claim: `from agent (name not verified)`. Under
//! `signing = "all"` the registry judged the sender's key, and the line names it plainly (G147).
//!
//! The guard to mutate: the `claimed` signing-mode read in `notice::arrived` (`src/notice.rs`).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::Value;
use support::talk::from_now;
use support::{TestBox, result_json, unchecked_files};

const APPROVALS_ONLY: &str = "\n[policy]\nsigning = \"approvals\"\n";

/// One plain remote under `signing = "approvals"`, no key registry anywhere: `agent` on `a`,
/// `agent-b` on `b`.
fn approvals() -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let remote = test_box.bare_any("remote.git");
    let a = test_box.repo_without_remote_any("a");
    test_box.add_origin(&a, &remote);
    let mut files = unchecked_files();
    files[0].1.push_str(APPROVALS_ONLY);
    test_box.seed_with_check(&a, &[("src/a.rs", "fn a() {}\n")], &files);
    let b = test_box.clone_of(&remote, "b");
    (test_box, a, b)
}

fn fixture(name: &str, session: &str, cwd: &Path) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code-2.1.302")
        .join(name);
    let mut payload: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    payload["session_id"] = Value::from(session);
    payload["cwd"] = Value::from(cwd.display().to_string());
    payload
}

/// `bay hook turn.start` as `agent-b` on `b`: what it printed.
fn prompt(test_box: &TestBox, b: &Path) -> String {
    let payload = fixture("02-UserPromptSubmit.json", "s-b", b);
    let mut child = test_box
        .bay_process(b)
        .env("BAY_ACTOR", "agent-b")
        .args(["hook", "turn.start"])
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
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    format!(
        "{}{}",
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap()
    )
}

#[test]
fn under_approvals_the_notice_says_the_sender_s_name_is_a_claim() {
    let (test_box, a, b) = approvals();
    let follow_up = from_now(3600);
    let (code, sent) = result_json(
        test_box
            .bay(&a)
            .env("BAY_ACTOR", "agent")
            .env("BAY_SESSION", "s-agent")
            .args([
                "talk",
                "agent-b",
                "Review the parser change",
                "--kind",
                "delivery",
                "--change",
                "abcd2345",
                "--follow-up",
                &follow_up,
            ]),
    );
    assert_eq!(code, 0, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();
    let told = prompt(&test_box, &b);
    assert!(
        told.contains(&format!(
            "bay: talk {id} (delivery) from agent (name not verified) arrived for agent-b, reply owed by {follow_up}"
        )),
        "{told}"
    );
}
