//! G72. With the Claude adapter configured, every view holds the agent product's hook-settings
//! file.
//!
//! A sparse view holds only its declared paths, and its Git top level is the view, so a session
//! started inside it would run without bay's project hooks (probed 2026-10-19). The view's
//! sparse set therefore adds that one file, at the path the adapter names, when the file exists
//! at the view's base. The file is bay's, not the change's: it is never an observed input.

// Each case runs shell scripts (a wrapper on PATH, the installed hook command), so these run
// on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use support::TestBox;

const CLAUDE_CONFIG: &str = "trunk = \"main\"\n\n[agent]\nproduct = \"claude\"\n";
const SETTINGS: &str = ".claude/settings.json";

/// A trunk with the Claude adapter, `src/`, another file under `.claude/`, and the settings
/// file `bay install` writes, committed.
fn seeded(test_box: &TestBox) -> PathBuf {
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(
        &repo,
        &[
            (".bay/config.toml", CLAUDE_CONFIG),
            (".bay/policy.toml", support::UNCHECKED_POLICY),
            ("src/a.rs", "fn a() {}\n"),
            (".claude/agents/reviewer.md", "an agent definition\n"),
        ],
    );
    test_box.bay_ok(&repo, &["install"]);
    test_box.seed_trunk(&repo, &[]);
    repo
}

fn start(test_box: &TestBox, repo: &Path, intent: &str) -> (String, PathBuf) {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    (
        id,
        PathBuf::from(started["detail"]["view"].as_str().unwrap()),
    )
}

fn sparse_set(test_box: &TestBox, view: &Path) -> Vec<String> {
    let mut list: Vec<String> = test_box
        .git_ok(view, &["sparse-checkout", "list"])
        .lines()
        .map(str::to_owned)
        .collect();
    list.sort();
    list
}

/// Runs the view's own `SessionStart` entry the way the product does: its command, from the
/// view, with the event on stdin.
fn session_start_from_view(test_box: &TestBox, view: &Path, session: &str) -> Option<i32> {
    let settings: Value =
        serde_json::from_str(&std::fs::read_to_string(view.join(SETTINGS)).unwrap()).unwrap();
    let command = settings["hooks"]["SessionStart"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["hooks"].as_array().unwrap())
        .filter_map(|hook| hook["command"].as_str())
        .find(|command| command.ends_with("bay hook session.start"))
        .unwrap()
        .to_owned();
    let payload = json!({"session_id": session, "hook_event_name": "SessionStart",
        "source": "startup", "cwd": view});
    let mut child = Command::new("sh")
        .args(["-c", &command])
        .current_dir(view)
        .env_clear()
        .env(
            "PATH",
            format!(
                "{}:{}:/bin:/usr/bin",
                test_box.fakebin.display(),
                test_box.local_bin().display()
            ),
        )
        .env("HOME", &test_box.home)
        .env("GIT_CONFIG_GLOBAL", &test_box.global_config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CEILING_DIRECTORIES", test_box.path())
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
    child.wait_with_output().unwrap().status.code()
}

fn change_record(test_box: &TestBox, repo: &Path, id: &str) -> Value {
    serde_json::from_str(&test_box.git_ok(
        repo,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    ))
    .unwrap()
}

#[test]
fn a_view_holds_the_hook_settings_and_a_hook_from_inside_it_is_recorded() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    let (id, view) = start(&test_box, &repo, "CARD-1");

    // Only that one file is added: not the rest of `.claude/`.
    assert_eq!(
        sparse_set(&test_box, &view),
        ["/.claude/settings.json", "/src/"]
    );
    assert!(!view.join(".claude/agents/reviewer.md").exists());
    let held = std::fs::read_to_string(view.join(SETTINGS)).unwrap();
    assert!(held.contains("bay hook tool.before"), "{held}");
    assert!(held.contains("bay hook session.start"), "{held}");

    assert_eq!(session_start_from_view(&test_box, &view, "s-1"), Some(0));
    let recorded = std::fs::read_to_string(repo.join(".git/bay/sessions/s-1.jsonl")).unwrap();
    let line: Value = serde_json::from_str(recorded.lines().next().unwrap()).unwrap();
    assert_eq!(line["event"], "session_start", "{line}");
    assert_eq!(line["change"], id.as_str(), "{line}");

    // The file is bay's, not an input the change read.
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    test_box.bay_ok(&repo, &["start", &id]);
    let record = change_record(&test_box, &repo, &id);
    assert!(record.get("observed_inputs").is_none(), "{record}");
}

#[test]
fn a_resumed_view_keeps_the_hook_settings() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    let (id, view) = start(&test_box, &repo, "CARD-1");

    // A view made again by resume holds the file.
    test_box.git_ok(
        &repo,
        &["worktree", "remove", "--force", view.to_str().unwrap()],
    );
    let resumed = test_box.bay_ok(&repo, &["start", &id]);
    let view = PathBuf::from(resumed["detail"]["view"].as_str().unwrap());
    assert_eq!(
        sparse_set(&test_box, &view),
        ["/.claude/settings.json", "/src/"]
    );
    assert!(view.join(SETTINGS).is_file());

    // A view that lost the entry gets it back on resume.
    test_box.git_ok(&view, &["sparse-checkout", "set", "--no-cone", "/src/"]);
    assert!(!view.join(SETTINGS).exists());
    test_box.bay_ok(&repo, &["start", &id]);
    assert_eq!(
        sparse_set(&test_box, &view),
        ["/.claude/settings.json", "/src/"]
    );
    assert!(view.join(SETTINGS).is_file());
    let record = change_record(&test_box, &repo, &id);
    assert!(record.get("observed_inputs").is_none(), "{record}");
}

#[test]
fn the_file_is_added_only_when_the_adapter_is_configured_and_the_base_has_it() {
    // The adapter is configured, but the base has no settings file.
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(
        &repo,
        &[
            (".bay/config.toml", CLAUDE_CONFIG),
            (".bay/policy.toml", support::UNCHECKED_POLICY),
            ("src/a.rs", "fn a() {}\n"),
        ],
    );
    let (_, view) = start(&test_box, &repo, "CARD-1");
    assert_eq!(sparse_set(&test_box, &view), ["/src/"]);

    // The base has the file, but no adapter is configured.
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(
        &repo,
        &[
            (".bay/policy.toml", support::UNCHECKED_POLICY),
            ("src/a.rs", "fn a() {}\n"),
            (SETTINGS, "{}\n"),
        ],
    );
    let (_, view) = start(&test_box, &repo, "CARD-1");
    assert_eq!(sparse_set(&test_box, &view), ["/src/"]);
    assert!(!view.join(SETTINGS).exists());
}
