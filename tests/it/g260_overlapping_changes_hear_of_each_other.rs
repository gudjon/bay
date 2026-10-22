//! G260 (NEW-8). Overlapping changes hear of each other, and a queued change of its early
//! conflict.
//!
//! `bay start --paths` lists in `detail.overlaps` the live changes this repository holds whose
//! declared paths overlap the new change's own, each with its change id, intent, actor and the
//! overlapping paths, from the chains already here (no fetch of their own); it never refuses.
//! A change on other paths lists none. `bay hook session.start` in the view names the same
//! overlaps. When the executor's probe notes an early merge conflict on the view's queued
//! operation (G177, `early_conflict` in its chain), the session's own agent is told once, with
//! the paths, at its next `step.end`, through the notice path of the refusals and failed
//! checks, and never again in that session.

// Each case runs the hook with a wrapper on PATH, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files};

fn setup() -> (TestBox, PathBuf, String) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let trunk = test_box.seed_with_check(
        &repo,
        &[
            ("src/a.rs", "fn a() {}\n"),
            ("docs/keep.md", "k\n"),
            ("lib/x.rs", "fn x() {}\n"),
        ],
        &unchecked_files(),
    );
    (test_box, repo, trunk)
}

/// `bay start <intent> --paths <paths>` as `actor`: the result.
fn start(test_box: &TestBox, repo: &Path, actor: &str, intent: &str, paths: &[&str]) -> Value {
    let mut args = vec!["start", intent, "--paths"];
    args.extend(paths);
    let (code, started) = result_json(test_box.bay(repo).env("BAY_ACTOR", actor).args(&args));
    assert_eq!(code, 0, "{started}");
    assert_eq!(started["decision"], "ok", "{started}");
    started
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

/// `bay hook <event>` with fixture `name` for `session` in `cwd`: its stdout and stderr.
fn hook(
    test_box: &TestBox,
    cwd: &Path,
    event: &str,
    name: &str,
    session: &str,
) -> (String, String) {
    let payload = fixture(name, session, cwd);
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
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    (
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

fn last_line(repo: &Path, session: &str) -> Value {
    let text =
        std::fs::read_to_string(repo.join(format!(".git/bay/sessions/{session}.jsonl"))).unwrap();
    serde_json::from_str(text.lines().last().unwrap()).unwrap()
}

/// `git <args>` in `dir` with `input` on stdin: its trimmed stdout.
fn git_in(test_box: &TestBox, dir: &Path, args: &[&str], input: &[u8]) -> String {
    let mut child = test_box
        .git(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// What the executor's early probe writes (G177), written here as one more state commit of the
/// chain of `id`: its latest operation waits in the queue (`admitted`) and conflicts with
/// `trunk` on `paths`. The repository has no key registry, so its chain is unsigned. The
/// operation id.
fn note_early_conflict(
    test_box: &TestBox,
    repo: &Path,
    id: &str,
    trunk: &str,
    paths: &[&str],
) -> String {
    let name = format!("refs/bay/changes/{id}");
    let head = test_box.git_ok(repo, &["rev-parse", &name]);
    let draft = test_box.git_ok(repo, &["rev-parse", &format!("{head}^2")]);
    let text = test_box.git_ok(repo, &["cat-file", "blob", &format!("{head}:change.json")]);
    let mut record: Value = serde_json::from_str(&text).unwrap();
    let op = record["operations"]
        .as_array_mut()
        .and_then(|ops| ops.last_mut())
        .expect("the change has an operation");
    op["state"] = Value::from("admitted");
    op["early_conflict"] = serde_json::json!({"trunk": trunk, "paths": paths});
    let op_id = op["op_id"].as_str().unwrap().to_owned();
    let blob = git_in(
        test_box,
        repo,
        &["hash-object", "-w", "--stdin"],
        record.to_string().as_bytes(),
    );
    let tree = git_in(
        test_box,
        repo,
        &["mktree"],
        format!("100644 blob {blob}\tchange.json\n").as_bytes(),
    );
    let commit = test_box.git_ok(
        repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit-tree",
            &tree,
            "-p",
            &head,
            "-p",
            &draft,
            "--no-gpg-sign",
            "-m",
            &format!("{op_id} conflicts with trunk {trunk}"),
        ],
    );
    test_box.git_ok(repo, &["update-ref", &name, &commit, &head]);
    op_id
}

#[test]
fn the_second_start_lists_the_first_and_one_on_other_paths_lists_none() {
    let (test_box, repo, _) = setup();
    let first = start(&test_box, &repo, "agent-a", "OVL-1", &["src/"]);
    let first_id = first["change"].as_str().unwrap();
    assert_eq!(
        first["detail"]["overlaps"],
        serde_json::json!([]),
        "{first}"
    );

    let second = start(&test_box, &repo, "agent-b", "OVL-2", &["src/a.rs", "docs/"]);
    let overlaps = second["detail"]["overlaps"].as_array().unwrap();
    assert_eq!(overlaps.len(), 1, "{second}");
    assert_eq!(overlaps[0]["change"], first_id, "{second}");
    assert_eq!(overlaps[0]["intent"], "OVL-1", "{second}");
    assert_eq!(overlaps[0]["actor"]["name"], "agent-a", "{second}");
    assert_eq!(
        overlaps[0]["paths"],
        serde_json::json!(["src/a.rs"]),
        "{second}"
    );

    // Other paths: no overlap, and the start is not refused either way.
    let third = start(&test_box, &repo, "agent-c", "OVL-3", &["lib/"]);
    assert_eq!(
        third["detail"]["overlaps"],
        serde_json::json!([]),
        "{third}"
    );

    // The second's session hears of the first at its start.
    let view = PathBuf::from(second["detail"]["view"].as_str().unwrap());
    let second_id = second["change"].as_str().unwrap();
    let (out, _) = hook(
        &test_box,
        &view,
        "session.start",
        "01-SessionStart.json",
        "s-ovl",
    );
    assert!(
        out.contains(&format!(
            "bay: change {second_id} overlaps live change {first_id} (intent OVL-1, actor agent-a) on src/a.rs\n"
        )),
        "{out}"
    );
}

#[test]
fn a_queued_change_hears_of_its_early_conflict_once() {
    let (test_box, repo, trunk) = setup();
    let started = start(&test_box, &repo, "agent-a", "CONFLICT-1", &["src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/b.rs"), "fn b() {}\n").unwrap();
    let (code, submitted) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");

    hook(
        &test_box,
        &view,
        "session.start",
        "01-SessionStart.json",
        "s-q",
    );
    // Nothing noted yet: no notice.
    let (_, told) = hook(&test_box, &view, "step.end", "08-PostToolBatch.json", "s-q");
    assert_eq!(told, "");

    let op_id = note_early_conflict(&test_box, &repo, &id, &trunk, &["src/b.rs"]);
    let (_, told) = hook(&test_box, &view, "step.end", "08-PostToolBatch.json", "s-q");
    let short = &trunk[..12];
    assert!(
        told.starts_with(&format!(
            "bay: change {id}: MERGE_CONFLICT with trunk {short} on {op_id} while it waits in \
             the queue, on src/b.rs: allowed "
        )),
        "{told}"
    );
    assert!(told.contains("refresh"), "{told}");
    assert_eq!(told.lines().count(), 1, "{told}");
    assert_eq!(
        last_line(&repo, "s-q")["conflict_notices"],
        serde_json::json!([op_id])
    );

    // Once: not at the next step, not at the next prompt.
    let (_, again) = hook(&test_box, &view, "step.end", "08-PostToolBatch.json", "s-q");
    assert_eq!(again, "");
    let (prompt, _) = hook(
        &test_box,
        &view,
        "turn.start",
        "02-UserPromptSubmit.json",
        "s-q",
    );
    assert_eq!(prompt, "");
    assert!(last_line(&repo, "s-q").get("conflict_notices").is_none());

    // A new session reads the conflict in its compact state at its start, and is not told it
    // again after.
    let (out, _) = hook(
        &test_box,
        &view,
        "session.start",
        "01-SessionStart.json",
        "s-r",
    );
    assert!(
        out.contains("bay: MERGE_CONFLICT with trunk while it waits in the queue, on src/b.rs: "),
        "{out}"
    );
    let (_, later) = hook(&test_box, &view, "step.end", "08-PostToolBatch.json", "s-r");
    assert_eq!(later, "");
}
