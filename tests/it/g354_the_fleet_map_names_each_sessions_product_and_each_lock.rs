//! G354 (owner ruling 2026-10-24, BACKLOG 61): the fleet map names each session's product and
//! each lock's holder.
//!
//! Two facts bay held but did not show sent people to a second place:
//! - (a) which agent product a session is. Its hook knows the product that called, a guest
//!   (Grok running a repository's Claude Code hooks) as itself; each session's row in
//!   `bay status --all` names it.
//! - (b) who holds a lock. The lock ref is the state; `bay status` fetches the lock refs with
//!   the presence it already fetches, and `bay status --all` lists each lock held now: its path,
//!   actor, session, change and age.
//!
//! Guards to mutate:
//! - `src/agent_hook.rs` `dispatch_as`: drop the `product` field (no row names its product).
//! - `src/locks.rs` `held`: keep no lock (`.filter(|_| false)`; the map lists none).

use crate::support;

use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;

use serde_json::{Value, json};
use support::{TestBox, UNCHECKED_POLICY, result_json};

/// `bay hook <event>` of `payload` from `cwd`; asserts it exits 0.
fn hook(test_box: &TestBox, cwd: &Path, event: &str, payload: &Value) {
    let mut child = test_box
        .bay_process(cwd)
        .args(["hook", event])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}

/// A recorded payload `name` of the product's `dir` fixtures, for `session` in `cwd`.
fn fixture(dir: &str, name: &str, session: &str, cwd: &Path) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(dir)
        .join(name);
    let mut payload: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    payload["session_id"] = json!(session);
    for key in ["cwd", "workspaceRoot"] {
        if payload.get(key).is_some() {
            payload[key] = json!(cwd);
        }
    }
    payload
}

/// `bay status --all` in `repo`, after its sessions' presence is pushed and fetched.
fn fleet(test_box: &TestBox, repo: &Path) -> Value {
    test_box.bay_ok(repo, &["live", "push"]);
    test_box.bay_ok(repo, &["status"]);
    let (code, map) = result_json(test_box.bay(repo).args(["status", "--all"]));
    assert_eq!(code, 0, "{map}");
    map
}

/// The product the map's row of `session` names.
fn product_of<'a>(map: &'a Value, session: &str) -> &'a Value {
    map["detail"]["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("no sessions: {map}"))
        .iter()
        .find(|row| row["session"] == session)
        .map_or(&Value::Null, |row| &row["product"])
}

#[test]
fn each_sessions_row_names_the_product_that_called() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[(".bay/policy.toml".to_owned(), UNCHECKED_POLICY.to_owned())],
    );
    hook(
        &test_box,
        &repo,
        "session.start",
        &fixture(
            "claude-code-2.1.302",
            "01-SessionStart.json",
            "s-claude",
            &repo,
        ),
    );
    hook(
        &test_box,
        &repo,
        "session.start",
        &fixture("grok-1.0.52", "SessionStart-0.json", "s-grok", &repo),
    );
    let map = fleet(&test_box, &repo);
    assert_eq!(product_of(&map, "s-claude"), "claude", "{map}");
    assert_eq!(product_of(&map, "s-grok"), "grok", "{map}");
}

#[test]
fn the_map_lists_each_lock_held_now_and_its_holder() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("art/logo.bin", "logo\n")],
        &[(
            ".bay/policy.toml".to_owned(),
            format!("{UNCHECKED_POLICY}\n[locks]\npaths = [\"art/\"]\n"),
        )],
    );
    let other = test_box.clone_of(&test_box.path().join("repo-remote.git"), "other");
    let edit = json!({
        "session_id": "s-a",
        "hook_event_name": "PreToolUse",
        "tool_name": "Write",
        "tool_input": {"file_path": repo.join("art/logo.bin"), "content": "x"},
        "cwd": repo,
    });
    hook(&test_box, &repo, "tool.before", &edit);
    // Another clone sees the holder once its `bay status` fetched.
    let map = fleet(&test_box, &other);
    let locks = map["detail"]["locks"]
        .as_array()
        .unwrap_or_else(|| panic!("{map}"));
    assert_eq!(locks.len(), 1, "{map}");
    assert_eq!(locks[0]["path"], "art/logo.bin", "{map}");
    assert_eq!(locks[0]["session"], "s-a", "{map}");
    assert!(locks[0]["held_s"].is_u64(), "{map}");
}
