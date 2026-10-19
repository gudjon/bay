//! G23. The fleet map is true (`DESIGN` § 11a, `INTERFACES` § 5a, `ROADMAP` § 8).
//!
//! Built so far: the per-session work record's writes. A shell tool reports no changed files
//! (`tests/fixtures/claude-code-2.1.302`), so the files a step wrote are the difference between
//! the view's Git status at this post-tool or stop event and the snapshot the record kept at the
//! previous one, kept per agent, at a bounded cost. The live summary ref, `status --fleet`'s
//! sessions, heartbeat reaping, untracked sessions, the notice budget, the `Stop` refusal and
//! `COVERAGE_UNKNOWN` are later parts of G23: they are the ignored cases at the end, so a pass
//! of this file is not G23 done.

mod support;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use serde_json::Value;
use support::TestBox;

/// A probed Claude Code payload, its redacted session id and cwd rewritten to the test's.
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

fn hook(test_box: &TestBox, cwd: &Path, event: &str, payload: &Value) -> Option<i32> {
    hook_env(test_box, cwd, event, payload, &[])
}

fn hook_env(
    test_box: &TestBox,
    cwd: &Path,
    event: &str,
    payload: &Value,
    envs: &[(&str, &Path)],
) -> Option<i32> {
    let mut command = test_box.bay_process(cwd);
    for (key, value) in envs {
        command.env(key, value);
    }
    let mut child = command
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
    child.wait_with_output().unwrap().status.code()
}

fn last_line(path: &Path) -> Value {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(text.lines().last().unwrap()).unwrap()
}

fn wrote(line: &Value) -> Vec<&str> {
    line["wrote"]
        .as_array()
        .unwrap_or_else(|| panic!("no wrote list: {line}"))
        .iter()
        .map(|path| path.as_str().unwrap())
        .collect()
}

#[test]
fn a_shell_write_is_recorded_from_the_view() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    // `start_and_edit` leaves `src/new.rs` written before the session starts: it is not this
    // session's write.
    let (id, view) = test_box.start_and_edit(&repo);
    let record = repo.join(".git/bay/sessions/s-g23.jsonl");

    let start = fixture("01-SessionStart.json", "s-g23", &view);
    assert_eq!(hook(&test_box, &view, "session-start", &start), Some(0));

    // The step's shell call (`echo b > b.txt` in the fixture) wrote a file the payload never
    // names; the write record comes from the view.
    std::fs::write(view.join("src/shell.rs"), "fn shell() {}\n").unwrap();
    std::fs::write(view.join("src/a.rs"), "fn a() { 1; }\n").unwrap();
    let batch = fixture("08-PostToolBatch.json", "s-g23", &view);
    assert_eq!(hook(&test_box, &view, "post-tool", &batch), Some(0));
    let line = last_line(&record);
    assert_eq!(line["event"], "post_tool_batch", "{line}");
    assert_eq!(line["change"], id.as_str(), "{line}");
    assert_eq!(wrote(&line), ["src/a.rs", "src/shell.rs"], "{line}");
    // No CLAUDE_ENV_FILE in the hook's environment (ruling R2): the session is linked to the
    // view's change only, never to a guessed actor session.
    assert_eq!(line["actor_link"], "unbound", "{line}");
    // The kept status ids are the hook's own state, not record fields: a line does not grow
    // with the view.
    assert!(line.get("snapshot").is_none(), "{line}");

    // A second edit of the same file is a write again, though status alone shows it unchanged.
    std::fs::write(view.join("src/shell.rs"), "fn shell() { 2; }\n").unwrap();
    assert_eq!(hook(&test_box, &view, "post-tool", &batch), Some(0));
    assert_eq!(wrote(&last_line(&record)), ["src/shell.rs"]);

    // A step that writes nothing lists nothing.
    assert_eq!(hook(&test_box, &view, "post-tool", &batch), Some(0));
    assert_eq!(wrote(&last_line(&record)), [] as [&str; 0]);

    // A file put back to trunk's content is a write; a deleted file is a write, also one the
    // view held before the session.
    std::fs::write(view.join("src/a.rs"), "fn a() {}\n").unwrap();
    std::fs::remove_file(view.join("src/new.rs")).unwrap();
    let stop = fixture("09-Stop.json", "s-g23", &view);
    assert_eq!(hook(&test_box, &view, "stop", &stop), Some(0));
    let line = last_line(&record);
    assert_eq!(line["state"], "idle", "{line}");
    assert_eq!(wrote(&line), ["src/a.rs", "src/new.rs"], "{line}");
}

#[test]
fn a_teammate_idle_is_recorded_as_idle() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    let (_, view) = test_box.start_and_edit(&repo);
    let idle = fixture("round-2/10-TeammateIdle-inter.json", "s-mate", &view);
    assert_eq!(hook(&test_box, &view, "teammate-idle", &idle), Some(0));
    let line = last_line(&repo.join(".git/bay/sessions/s-mate.jsonl"));
    assert_eq!(line["event"], "teammate_idle", "{line}");
    assert_eq!(line["state"], "idle", "{line}");
}

#[test]
fn an_unprobed_event_has_no_behaviour() {
    // Ruling R16: no recorded payload, no behaviour. `WorktreeCreate` is not yet probed, so bay
    // installs no entry for it, and a `bay hook` an older install left is silent and records
    // nothing.
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    let (_, view) = test_box.start_and_edit(&repo);
    let payload = serde_json::json!({"session_id": "s-wt", "hook_event_name": "WorktreeCreate",
        "cwd": view});
    assert_eq!(hook(&test_box, &view, "worktree-create", &payload), Some(0));
    assert!(!repo.join(".git/bay/sessions/s-wt.jsonl").exists());
}

#[test]
fn a_subagent_keeps_its_own_writes_while_the_parent_works() {
    // A background subagent shares its parent's session id (08-PostToolBatch launches one "in the
    // background"). Each agent diffs against its own last status, so the parent's steps never
    // take the subagent's writes away from it, and a compacted session keeps its baseline.
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    let (_, view) = test_box.start_and_edit(&repo);
    let record = repo.join(".git/bay/sessions/s-sub.jsonl");
    let start = fixture("01-SessionStart.json", "s-sub", &view);
    assert_eq!(hook(&test_box, &view, "session-start", &start), Some(0));
    let mut sub_start = fixture("21-SubagentStart-fg-subagent.json", "s-sub", &view);
    sub_start["agent_id"] = Value::from("af9b166e249d0c0a9");
    assert_eq!(
        hook(&test_box, &view, "subagent-start", &sub_start),
        Some(0)
    );

    // The subagent writes while the parent's step ends. The parent's line lists the write it
    // saw in the view (status cannot tell who wrote it); the subagent's stop lists it too.
    std::fs::write(view.join("src/c.rs"), "fn c() {}\n").unwrap();
    let batch = fixture("08-PostToolBatch.json", "s-sub", &view);
    assert_eq!(hook(&test_box, &view, "post-tool", &batch), Some(0));
    assert_eq!(wrote(&last_line(&record)), ["src/c.rs"]);
    let stop = fixture("13-SubagentStop.json", "s-sub", &view);
    assert_eq!(hook(&test_box, &view, "subagent-stop", &stop), Some(0));
    let line = last_line(&record);
    assert_eq!(line["agent"], "af9b166e249d0c0a9", "{line}");
    assert_eq!(wrote(&line), ["src/c.rs"], "{line}");

    // A compaction restarts the session but not its baseline: a write made before it is still
    // the next step's write.
    std::fs::write(view.join("src/d.rs"), "fn d() {}\n").unwrap();
    let compact = fixture("round-3/07-SessionStart-interactive.json", "s-sub", &view);
    assert_eq!(compact["source"], "compact");
    assert_eq!(hook(&test_box, &view, "session-start", &compact), Some(0));
    assert_eq!(hook(&test_box, &view, "post-tool", &batch), Some(0));
    assert_eq!(wrote(&last_line(&record)), ["src/d.rs"]);
}

#[test]
fn a_hook_reads_only_what_changed_and_its_record_stays_small() {
    // The hook is on the synchronous path before the next model call: a file whose size,
    // mtime and inode did not move is not hashed again, a line does not carry the view's whole
    // status, and a view with more changed paths than the bound is recorded as unknown, not
    // hashed.
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    let (_, view) = test_box.start_and_edit(&repo);
    let record = repo.join(".git/bay/sessions/s-cost.jsonl");
    // Old files: a stat match is trusted only for a file older than the racy window.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    for index in 0..200 {
        let path = view.join(format!("src/gen{index}.rs"));
        std::fs::write(&path, "// gen\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
    let start = fixture("01-SessionStart.json", "s-cost", &view);
    assert_eq!(hook(&test_box, &view, "session-start", &start), Some(0));

    // From here on, log every path the hook hashes.
    let log = test_box.path().join("hashed.log");
    let real = String::from_utf8(
        std::process::Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let real = real.trim();
    support::write_executable(
        &test_box.fakebin.join("git"),
        &format!(
            "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = --stdin-paths ]; then\n    tee -a \"{}\" | \"{real}\" \"$@\"\n    exit $?\n  fi\ndone\nexec \"{real}\" \"$@\"\n",
            log.display(),
        ),
    );
    std::fs::write(view.join("src/a.rs"), "fn a() { 1; }\n").unwrap();
    let batch = fixture("08-PostToolBatch.json", "s-cost", &view);
    assert_eq!(hook(&test_box, &view, "post-tool", &batch), Some(0));
    let line = last_line(&record);
    assert_eq!(wrote(&line), ["src/a.rs"], "{line}");
    let hashed = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !hashed.contains("src/gen"),
        "unchanged files hashed again:\n{hashed}"
    );
    let text = std::fs::read_to_string(&record).unwrap();
    assert!(
        text.lines().all(|line| line.len() < 2048),
        "a record line grows with the view:\n{text}"
    );

    // Past the bound the write list is unknown, never a guess, and nothing is hashed.
    for index in 0..2100 {
        std::fs::write(view.join(format!("src/many{index}.rs")), "// many\n").unwrap();
    }
    std::fs::write(&log, "").unwrap();
    assert_eq!(hook(&test_box, &view, "post-tool", &batch), Some(0));
    let line = last_line(&record);
    assert_eq!(line["wrote_unknown"], "too_many_paths", "{line}");
    assert!(line.get("wrote").is_none(), "{line}");
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "",
        "hashed past the bound"
    );
}

#[test]
fn deletions_ignored_paths_and_nested_repositories_are_named_truly() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(
        &repo,
        &[
            ("src/a.rs", "fn a() {}\n"),
            ("src/b.rs", "fn b() {}\n"),
            (".gitignore", "*.log\n"),
        ],
    );
    let (_, view) = test_box.start_and_edit(&repo);
    let record = repo.join(".git/bay/sessions/s-kinds.jsonl");
    let start = fixture("01-SessionStart.json", "s-kinds", &view);
    assert_eq!(hook(&test_box, &view, "session-start", &start), Some(0));

    // A staged deletion (`D.`), an ignored file, an untracked nested repository (`? dir/`) and
    // one ordinary write in the same step.
    test_box.git_ok(&view, &["rm", "-q", "src/a.rs"]);
    std::fs::write(view.join("src/build.log"), "noise\n").unwrap();
    let nested = view.join("src/nested");
    std::fs::create_dir_all(&nested).unwrap();
    test_box.git_ok(&nested, &["init", "-q"]);
    std::fs::write(nested.join("inner.rs"), "fn inner() {}\n").unwrap();
    std::fs::write(view.join("src/b.rs"), "fn b() { 2; }\n").unwrap();
    let batch = fixture("08-PostToolBatch.json", "s-kinds", &view);
    assert_eq!(hook(&test_box, &view, "post-tool", &batch), Some(0));
    let line = last_line(&record);
    assert_eq!(wrote(&line), ["src/a.rs", "src/b.rs"], "{line}");

    // The staged deletion stays deleted: the next step does not list it again.
    std::fs::write(view.join("src/b.rs"), "fn b() { 3; }\n").unwrap();
    assert_eq!(hook(&test_box, &view, "post-tool", &batch), Some(0));
    assert_eq!(wrote(&last_line(&record)), ["src/b.rs"]);
}

#[cfg(unix)]
#[test]
fn a_file_name_that_is_not_utf8_does_not_stop_the_record() {
    use std::os::unix::ffi::OsStrExt;
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    let (_, view) = test_box.start_and_edit(&repo);
    let odd = view
        .join("src")
        .join(std::ffi::OsStr::from_bytes(b"odd-\xff.rs"));
    if std::fs::write(&odd, "odd\n").is_err() {
        // A file system that refuses the name cannot hold the case.
        return;
    }
    let record = repo.join(".git/bay/sessions/s-odd.jsonl");
    let start = fixture("01-SessionStart.json", "s-odd", &view);
    assert_eq!(hook(&test_box, &view, "session-start", &start), Some(0));
    std::fs::write(view.join("src/a.rs"), "fn a() { 1; }\n").unwrap();
    std::fs::write(&odd, "odd 2\n").unwrap();
    let batch = fixture("08-PostToolBatch.json", "s-odd", &view);
    assert_eq!(hook(&test_box, &view, "post-tool", &batch), Some(0));
    let line = last_line(&record);
    assert_eq!(wrote(&line), ["src/a.rs", "src/odd-\u{fffd}.rs"], "{line}");
}

#[test]
fn a_session_start_with_an_env_file_binds_the_session() {
    // Ruling R2: where the product hands session-start a `CLAUDE_ENV_FILE`, bay writes
    // `BAY_SESSION` and `BAY_ACTOR` into it, so the session's own bay commands act as this
    // session; its record says the link is bound.
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    let (_, view) = test_box.start_and_edit(&repo);
    let env_file = test_box.path().join("claude-env.sh");
    let start = fixture("01-SessionStart.json", "s-env", &view);
    let envs: [(&str, &Path); 1] = [("CLAUDE_ENV_FILE", &env_file)];
    assert_eq!(
        hook_env(&test_box, &view, "session-start", &start, &envs),
        Some(0)
    );
    let text = std::fs::read_to_string(&env_file).unwrap();
    assert!(text.contains("export BAY_SESSION='s-env'\n"), "{text}");
    assert!(text.contains("export BAY_ACTOR='"), "{text}");
    let record = repo.join(".git/bay/sessions/s-env.jsonl");
    assert_eq!(last_line(&record)["actor_link"], "bound");
    let batch = fixture("08-PostToolBatch.json", "s-env", &view);
    assert_eq!(hook(&test_box, &view, "post-tool", &batch), Some(0));
    assert_eq!(last_line(&record)["actor_link"], "bound");
}

#[test]
#[ignore = "G23: refs/bay/live/<machine> and status --fleet sessions (A2, B1)"]
fn three_sessions_on_two_machines_are_in_the_fleet_map() {
    panic!("G23: the live summary ref and status --fleet sessions are not implemented");
}

#[test]
#[ignore = "G23: heartbeat reaping and untracked sessions (B1)"]
fn a_stale_session_is_reaped_and_a_hookless_one_is_untracked() {
    panic!("G23: heartbeat reaping and untracked sessions are not implemented");
}

#[test]
#[ignore = "G23: the notice budget, the Stop refusal (R7) and COVERAGE_UNKNOWN (B3, B4, C1)"]
fn notices_and_the_stop_refusal_follow_the_rulings() {
    panic!("G23: the notice budget, the Stop refusal and COVERAGE_UNKNOWN are not implemented");
}
