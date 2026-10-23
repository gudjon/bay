//! G153. The session row records the agent product's permission mode.
//!
//! Claude Code names its permission mode in the hook payload (`permission_mode`, recorded as
//! `bypassPermissions` in `tests/fixtures/claude-code-2.1.302`). The session's work record line
//! carries it, and keeps the last mode the session named on a line whose payload names none
//! (`SessionStart` and `Stop` payloads may lack it), so the latest line always says.
//! `bay status --all` shows it per session, and doctor's `agent-hooks` row says that bay's
//! deny holds under `bypassPermissions` (the G61 probes P5a and P5b).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use serde_json::{Value, json};
use support::{TestBox, result_json, row};

fn hook(test_box: &TestBox, cwd: &Path, event: &str, payload: &Value) {
    let mut child = test_box
        .bay_process(cwd)
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
    assert_eq!(child.wait_with_output().unwrap().status.code(), Some(0));
}

/// A recorded payload, with this session and directory in place of the redacted fields.
fn fixture(name: &str, session: &str, cwd: &Path) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code-2.1.302")
        .join(name);
    let mut payload: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    payload["session_id"] = json!(session);
    payload["cwd"] = json!(cwd);
    payload
}

fn lines(repo: &Path, session: &str) -> Vec<Value> {
    let path = repo.join(format!(".git/bay/sessions/{session}.jsonl"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn the_row_carries_the_mode_the_payload_names_and_the_fleet_shows_it() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    test_box.git_ok(&repo, &["config", "bay.machine", "box-a"]);

    // The recorded SessionStart names no mode: the line has none to give.
    let start = fixture("01-SessionStart.json", "s-bypass", &repo);
    assert!(start.get("permission_mode").is_none(), "{start}");
    hook(&test_box, &repo, "session.start", &start);
    let first = lines(&repo, "s-bypass");
    assert!(first[0].get("permission_mode").is_none(), "{}", first[0]);

    let prompt = fixture("02-UserPromptSubmit.json", "s-bypass", &repo);
    assert_eq!(prompt["permission_mode"], "bypassPermissions");
    hook(&test_box, &repo, "turn.start", &prompt);
    let after = lines(&repo, "s-bypass");
    assert_eq!(
        after.last().unwrap()["permission_mode"],
        "bypassPermissions",
        "{after:?}"
    );

    // A later payload without the field keeps the session's last named mode on its line.
    let mut stop = fixture("09-Stop.json", "s-bypass", &repo);
    stop.as_object_mut().unwrap().remove("permission_mode");
    hook(&test_box, &repo, "turn.end", &stop);
    let last = lines(&repo, "s-bypass").pop().unwrap();
    assert_eq!(last["event"], "stop", "{last}");
    assert_eq!(last["permission_mode"], "bypassPermissions", "{last}");

    // A session in the default mode says so; a value that is not a plain word is not recorded.
    let mut plain = fixture("02-UserPromptSubmit.json", "s-plain", &repo);
    plain["permission_mode"] = json!("default");
    hook(&test_box, &repo, "turn.start", &plain);
    let mut odd = fixture("02-UserPromptSubmit.json", "s-odd", &repo);
    odd["permission_mode"] = json!("by pass\n; rm -rf /");
    hook(&test_box, &repo, "turn.start", &odd);
    let odd_line = lines(&repo, "s-odd").pop().unwrap();
    assert!(odd_line.get("permission_mode").is_none(), "{odd_line}");

    test_box.bay_ok(&repo, &["status"]);
    let (code, result) = result_json(test_box.bay(&repo).args(["status", "--all"]));
    assert_eq!(code, 0, "{result}");
    let sessions = result["detail"]["sessions"].as_array().unwrap();
    let mode = |id: &str| {
        sessions
            .iter()
            .find(|row| row["session"] == id)
            .unwrap_or_else(|| panic!("no {id}: {result}"))["permission_mode"]
            .clone()
    };
    assert_eq!(mode("s-bypass"), "bypassPermissions", "{result}");
    assert_eq!(mode("s-plain"), "default", "{result}");
    assert!(mode("s-odd").is_null(), "{result}");
}

#[test]
fn doctor_says_the_deny_holds_under_bypass_permissions() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, "main = \"main\"\n\n[agent]\nproduct = \"claude\"\n");
    test_box.fake_claude(Some("2.1.302"));
    test_box.bay_ok(&repo, &["install"]);
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    let hooks = row(&result, "agent-hooks");
    let bypass = &hooks["bypass_permissions"];
    assert_eq!(bypass["deny_holds"], true, "{hooks}");
    let why = bypass["why"].as_str().unwrap_or_default();
    assert!(why.contains("bypassPermissions"), "{hooks}");
}
