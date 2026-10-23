//! G331 (hook contract H5, owner ruling 2026-10-22): the hook contract is tested over every
//! payload the agent products were recorded sending.
//!
//! For each recorded payload of a product (`tests/fixtures/<product>-<version>/`) whose event bay
//! declares for that product in `bay help --json` (`detail.adapters[].events`), `bay hook
//! <moment>` runs as the product runs it, in a repository set up for that product:
//! - conformance: it exits 0 or 2, nothing else (H2: allow, or deny with a reason); exit 2 only
//!   at a moment the declaration says `blocks`, and its stdout then names a reason code the
//!   contract lists;
//! - timing: it answers within a tenth of the entry's timeout, so the product's own timeout never
//!   decides for bay;
//! - a deny: each recorded shell call, asked again as a force push of main, is denied (exit 2)
//!   at a moment the declaration says `blocks`, so the rules above are exercised, not vacuous.
//!
//! Guard to mutate: `src/agent_settings.rs` `entry_shape`: the `PreToolUse` arm's
//! `blocks: true` to `false` (a deny at `tool.before`, declared as not blocking).

// Each case runs `bay hook` with a payload on stdin and a fake product on PATH; Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::TestBox;

/// A recorded payload with `<redacted>` paths made real for `repo`.
fn payload(file: &Path, repo: &Path) -> Value {
    let text = std::fs::read_to_string(file).unwrap();
    let mut value: Value = serde_json::from_str(&text).unwrap();
    let root = repo.display().to_string();
    if let Some(object) = value.as_object_mut() {
        if object.contains_key("cwd") {
            object.insert("cwd".to_owned(), Value::String(root.clone()));
        }
        for key in ["transcript_path", "agent_transcript_path"] {
            if object.contains_key(key) {
                object.insert(
                    key.to_owned(),
                    Value::String(repo.join(".no-transcript.jsonl").display().to_string()),
                );
            }
        }
    }
    let text = value.to_string().replace("<repo>", &root);
    serde_json::from_str(&text).unwrap()
}

/// Every recorded payload file of `dir` and its sub-directories.
fn recorded(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            recorded(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "json") {
            out.push(path);
        }
    }
    out.sort();
}

fn run_hook(
    test_box: &TestBox,
    cwd: &Path,
    moment: &str,
    input: &Value,
) -> (i32, String, Duration) {
    let started = Instant::now();
    let mut child = test_box
        .bay_process(cwd)
        .args(["hook", moment])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    // A deny names its reason on stdout (a `bay.result/1`) or stderr (`bay: refused <CODE>`).
    let said = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), said, started.elapsed())
}

#[test]
fn every_recorded_payload_gets_a_declared_answer_in_time() {
    let test_box = TestBox::new();
    // `bay_ok` adds `--json`: this is `bay help --json`.
    let contract = test_box.bay_ok(test_box.path(), &["help"]);
    let codes: Vec<&str> = contract["detail"]["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|reason| reason["code"].as_str())
        .collect();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut ran = 0;
    let mut faults = Vec::new();
    for adapter in contract["detail"]["adapters"].as_array().unwrap() {
        let product = adapter["product"].as_str().unwrap();
        let version = adapter["payload_version"].as_str().unwrap();
        let dir = fixtures.join(format!(
            "{}-{version}",
            if product == "claude" {
                "claude-code"
            } else {
                product
            }
        ));
        assert!(dir.is_dir(), "no recorded payloads for {product} {version}");
        let events = adapter["events"].as_array().unwrap();
        let repo = test_box.repo_any(&format!("repo-{product}"));
        test_box.commit_config(
            &repo,
            &format!("main = \"main\"\n\n[agent]\nproduct = \"{product}\"\n"),
        );
        let mut files = Vec::new();
        recorded(&dir, &mut files);
        for file in files {
            let input = payload(&file, &repo);
            let Some(event) = input["hook_event_name"].as_str() else {
                continue;
            };
            let Some(declared) = events.iter().find(|entry| entry["product_event"] == event) else {
                continue;
            };
            let moment = declared["bay_event"].as_str().unwrap();
            let blocks = declared["blocks"] == true;
            let budget = Duration::from_secs(declared["timeout_s"].as_u64().unwrap_or(30)) / 10;
            let name = file.strip_prefix(&fixtures).unwrap().display().to_string();
            let mut cases = vec![(name.clone(), input.clone(), false)];
            // The recorded shell call, asking for a force push of main: bay must deny it, at a
            // moment declared as blocking.
            if event == "PreToolUse" && input["tool_name"] == "Bash" {
                let mut push = input.clone();
                push["tool_input"]["command"] = Value::from("git push --force origin main");
                cases.push((format!("{name} (force push)"), push, true));
            }
            for (name, input, must_deny) in cases {
                let (code, said, took) = run_hook(&test_box, &repo, moment, &input);
                ran += 1;
                if must_deny && code != 2 {
                    faults.push(format!("{name}: a force push of main got exit {code}"));
                }
                match code {
                    0 => {}
                    2 if blocks => {
                        if !codes.iter().any(|code| said.contains(code)) {
                            faults.push(format!("{name}: exit 2 names no listed reason: {said}"));
                        }
                    }
                    2 => faults.push(format!("{name}: exit 2 at {moment}, declared not blocking")),
                    other => faults.push(format!("{name}: exit {other} at {moment}")),
                }
                if took > budget.max(Duration::from_secs(3)) {
                    faults.push(format!("{name}: {took:?} at {moment}, over {budget:?}"));
                }
            }
        }
    }
    assert!(ran >= 20, "only {ran} recorded payloads ran");
    assert!(faults.is_empty(), "{}", faults.join("\n"));
}
