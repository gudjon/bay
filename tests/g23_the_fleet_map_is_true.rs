//! G23. The fleet map is true (`DESIGN` § 11a, `INTERFACES` § 5a, `ROADMAP` § 8).
//!
//! Built so far: the per-session work record's writes. A shell tool reports no changed files
//! (`tests/fixtures/claude-code-2.1.302`), so the files a step wrote are the difference between
//! the view's Git status at this post-tool or stop event and the snapshot the record kept at the
//! previous one, kept per agent, at a bounded cost. `bay status --fleet` lists every session the
//! fetched live summaries name, with its machine, actor, change, state, last write, age and
//! whether trunk moved past its change's base; a row a takeover window old drops out, judged
//! without comparing two machines' clocks; this machine's sessions come from its own records; a
//! change whose view is here and that no record names is `tracked: false`, never idle, and one
//! whose view is elsewhere is `unknown`; a summary that is too large, not JSON, or (under a
//! registry) not signed by the agent it names lists no sessions. And the `Stop` refusal (ruling
//! R7): a stop over uncheckpointed edits is refused once. The notice budget is the later part of
//! G23: it is the ignored case at the end, so a pass of this file is not G23 done. The
//! `pre-tool` cache clauses (`COVERAGE_UNKNOWN` past `cache_max_age_s`, land refusing the same
//! stale write with the hooks removed) are tested with G8's inputs in
//! `g08_invalidation_stops_stale_work_and_permits_repair.rs`.

mod support;

use std::io::Write;
use std::path::{Path, PathBuf};
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

/// `bay hook <event>` with `payload` on stdin: its exit code and its stderr.
fn hook_output(test_box: &TestBox, cwd: &Path, event: &str, payload: &Value) -> (i32, String) {
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
    (
        output.status.code().unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
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
    // The view still holds uncheckpointed edits, so this is the stop the product sends after
    // bay refused the first one (ruling R7).
    let mut stop = fixture("09-Stop.json", "s-g23", &view);
    stop["stop_hook_active"] = Value::from(true);
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

/// The reaping case's takeover window (a session row's time to live). The map case uses a
/// minute, so a loaded box never ages its rows out before it reads them.
const TAKEOVER_S: u64 = 8;
const MAP_TAKEOVER_S: u64 = 60;

/// The fleet case's config: a 2 s lease, the takeover window and a 1 s heartbeat for the live
/// pusher.
fn fleet_config(takeover_s: u64) -> String {
    format!("[executor]\nlease_s = 2\ntakeover_s = {takeover_s}\nheartbeat_s = 1\n")
}

/// The current UTC time as bay's records write it; such times compare as text.
fn now_rfc3339() -> String {
    let out = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// The `live.json` the remote holds for `machine`, if any.
fn remote_summary(test_box: &TestBox, bare: &Path, machine: &str) -> Option<Value> {
    let out = test_box
        .git(test_box.path())
        .args([
            &format!("--git-dir={}", bare.display()),
            "cat-file",
            "blob",
            &format!("refs/bay/live/{machine}:live.json"),
        ])
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| serde_json::from_slice(&out.stdout).unwrap())
}

/// Fires a post-tool event of each session in its view (the hook starts the live pusher) until
/// the remote's summary of `machine` holds a row of each written from now on.
fn heartbeat(test_box: &TestBox, bare: &Path, machine: &str, sessions: &[(&str, &Path)]) {
    let since = now_rfc3339();
    let asked = std::time::Instant::now();
    loop {
        for (session, view) in sessions {
            let batch = fixture("08-PostToolBatch.json", session, view);
            assert_eq!(hook(test_box, view, "post-tool", &batch), Some(0));
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        let fresh = remote_summary(test_box, bare, machine).is_some_and(|summary| {
            sessions.iter().all(|(session, _)| {
                summary["sessions"].as_array().is_some_and(|rows| {
                    rows.iter().any(|row| {
                        row["session"] == *session
                            && row["at"].as_str().is_some_and(|at| at >= since.as_str())
                    })
                })
            })
        });
        if fresh {
            return;
        }
        assert!(
            asked.elapsed() < std::time::Duration::from_secs(30),
            "the live summary of {machine} never reached the remote"
        );
    }
}

fn fleet(test_box: &TestBox, repo: &Path) -> Value {
    let (code, result) = support::result_json(test_box.bay(repo).args(["status", "--fleet"]));
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["decision"], "ok", "{result}");
    result
}

fn entry<'a>(result: &'a Value, key: &str, value: &str) -> Option<&'a Value> {
    result["detail"]["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("no sessions: {result}"))
        .iter()
        .find(|row| row[key] == value)
}

/// Two clones of one remote, named box-a and box-b, whose trunk sets the fleet case's windows.
fn two_machines(test_box: &TestBox, takeover_s: u64) -> (PathBuf, PathBuf, PathBuf) {
    let a = test_box.repo_any("a");
    let config = fleet_config(takeover_s);
    test_box.seed_trunk(
        &a,
        &[("src/a.rs", "fn a() {}\n"), (".bay/config.toml", &config)],
    );
    let bare = PathBuf::from(test_box.git_ok(&a, &["remote", "get-url", "origin"]));
    test_box.git_ok(&a, &["config", "bay.machine", "box-a"]);
    let b = test_box.clone_of(&bare, "b");
    test_box.git_ok(&b, &["config", "bay.machine", "box-b"]);
    (bare, a, b)
}

fn session_start(test_box: &TestBox, session: &str, view: &Path) {
    let start = fixture("01-SessionStart.json", session, view);
    assert_eq!(hook(test_box, view, "session-start", &start), Some(0));
}

#[test]
fn three_sessions_on_two_machines_are_in_the_fleet_map() {
    let test_box = TestBox::new();
    let (bare, a, b) = two_machines(&test_box, MAP_TAKEOVER_S);
    let (a1, a1_view) = test_box.start_and_edit(&a);
    let (a2, a2_view) = test_box.start_and_edit(&a);
    let (b1, b1_view) = test_box.start_and_edit(&b);
    for (session, view) in [("s-a1", &a1_view), ("s-a2", &a2_view), ("s-b1", &b1_view)] {
        session_start(&test_box, session, view);
    }
    // s-a1's first step writes a file; its later steps write nothing.
    std::fs::write(a1_view.join("src/w.rs"), "fn w() {}\n").unwrap();
    heartbeat(
        &test_box,
        &bare,
        "box-a",
        &[("s-a1", &a1_view), ("s-a2", &a2_view)],
    );
    heartbeat(&test_box, &bare, "box-b", &[("s-b1", &b1_view)]);

    // `bay status` is the fetch: it brings refs/bay/live/* and records it, so the fleet map no
    // longer fails closed.
    test_box.bay_ok(&a, &["status"]);
    let result = fleet(&test_box, &a);
    let detail = &result["detail"];
    let machines: Vec<&str> = detail["machines"]
        .as_array()
        .unwrap_or_else(|| panic!("{result}"))
        .iter()
        .map(|machine| machine["machine"].as_str().unwrap())
        .collect();
    assert_eq!(machines, ["box-a", "box-b"], "{result}");
    assert!(detail["machines"][0]["age_s"].is_u64(), "{result}");
    assert!(detail["actors"][0]["actor"].is_string(), "{result}");
    assert!(detail["actors"][0]["age_s"].is_u64(), "{result}");
    assert_eq!(detail["talks"], serde_json::json!([]), "{result}");
    // Whether the remote moved since the fetch is unknown to a read that never asks it: never a
    // known `false`.
    assert!(detail["behind"].is_null(), "{result}");
    for (session, machine, change) in [
        ("s-a1", "box-a", &a1),
        ("s-a2", "box-a", &a2),
        ("s-b1", "box-b", &b1),
    ] {
        let row =
            entry(&result, "session", session).unwrap_or_else(|| panic!("{session}: {result}"));
        assert_eq!(row["machine"], machine, "{row}");
        assert_eq!(row["change"], change.as_str(), "{row}");
        assert_eq!(row["tracked"], true, "{row}");
        assert_eq!(row["state"], "working", "{row}");
        assert!(row["actor"]["name"].is_string(), "{row}");
        assert!(
            row["age_s"]
                .as_u64()
                .is_some_and(|age| age < MAP_TAKEOVER_S),
            "{row}"
        );
    }
    // This machine's rows carry their last files written from the local record; another
    // machine's summary carries no file lists (ruling R-A2-1), so its list is unknown, never
    // empty.
    let a1_row = entry(&result, "session", "s-a1").unwrap();
    assert_eq!(a1_row["wrote"], serde_json::json!(["src/w.rs"]), "{a1_row}");
    assert!(a1_row["wrote_at"].is_string(), "{a1_row}");
    let a2_row = entry(&result, "session", "s-a2").unwrap();
    assert_eq!(a2_row["wrote"], serde_json::json!([]), "{a2_row}");
    let b1_row = entry(&result, "session", "s-b1").unwrap();
    assert!(b1_row["wrote"].is_null(), "{b1_row}");
    // Every change is on trunk's tip; the other machine's change is not in this cache.
    assert_eq!(a1_row["behind"], false, "{a1_row}");
    assert!(b1_row["behind"].is_null(), "{b1_row}");

    // Trunk moves: box-a's changes are behind once a fetch brings the new trunk.
    std::fs::write(b.join("README"), "moved\n").unwrap();
    test_box.git_ok(&b, &["add", "README"]);
    test_box.git_ok(&b, &["commit", "-q", "-m", "trunk moves"]);
    test_box.git_ok(&b, &["push", "-q", "origin", "main"]);
    test_box.bay_ok(&a, &["status"]);
    let result = fleet(&test_box, &a);
    for session in ["s-a1", "s-a2"] {
        let row = entry(&result, "session", session).unwrap_or_else(|| panic!("{result}"));
        assert_eq!(row["behind"], true, "{row}");
    }
}

/// Waits until the live pusher of `repo` has exited, so its machine's summary stops moving.
fn pusher_idle(repo: &Path) {
    let lock = repo.join(".git/bay/live-push.lock");
    let asked = std::time::Instant::now();
    while lock.exists() {
        assert!(
            asked.elapsed() < std::time::Duration::from_secs(30),
            "the live pusher of {} never exited",
            repo.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// One post-tool event of `session`: its local record line is fresh again.
fn step(test_box: &TestBox, session: &str, view: &Path) {
    let batch = fixture("08-PostToolBatch.json", session, view);
    assert_eq!(hook(test_box, view, "post-tool", &batch), Some(0));
}

#[test]
fn a_stale_session_is_reaped_and_a_hookless_one_is_untracked() {
    let test_box = TestBox::new();
    let (bare, a, b) = two_machines(&test_box, TAKEOVER_S);
    let (a1, a1_view) = test_box.start_and_edit(&a);
    let (b1, b1_view) = test_box.start_and_edit(&b);
    // A change held on box-a with no session row: its session runs with hooks disabled.
    let (hookless, _) = test_box.start_and_edit(&a);
    // box-a's cache also holds box-b's change, whose view is not on box-a.
    let b1_ref = format!("refs/bay/changes/{b1}");
    test_box.git_ok(
        &a,
        &["fetch", "-q", "origin", &format!("{b1_ref}:{b1_ref}")],
    );
    session_start(&test_box, "s-a1", &a1_view);
    session_start(&test_box, "s-b1", &b1_view);
    heartbeat(&test_box, &bare, "box-b", &[("s-b1", &b1_view)]);
    pusher_idle(&b);
    // box-a's own session is read from its record, so one step keeps it fresh.
    step(&test_box, "s-a1", &a1_view);
    test_box.bay_ok(&a, &["status"]);
    let result = fleet(&test_box, &a);
    assert!(entry(&result, "session", "s-b1").is_some(), "{result}");
    // A change with a fresh row of its actor gets no other entry.
    assert_eq!(
        result["detail"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["change"] == b1.as_str())
            .count(),
        1,
        "{result}"
    );
    let untracked = entry(&result, "change", &hookless).unwrap_or_else(|| panic!("{result}"));
    assert_eq!(untracked["tracked"], false, "{untracked}");
    assert_eq!(untracked["state"], "untracked", "{untracked}");
    assert!(untracked["session"].is_null(), "{untracked}");
    assert!(untracked["age_s"].is_null(), "{untracked}");
    assert_eq!(
        untracked["actor"]["name"],
        entry(&result, "session", "s-a1").unwrap()["actor"]["name"],
        "{result}"
    );
    // A change with a session row is listed once, by that session.
    let a1_rows = result["detail"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["change"] == a1.as_str())
        .count();
    assert_eq!(a1_rows, 1, "{result}");

    // box-b's session stops firing. Once its row is a takeover window old it drops out, though
    // box-b's summary still lists it; box-a's session keeps beating and stays.
    std::thread::sleep(std::time::Duration::from_secs(TAKEOVER_S + 1));
    step(&test_box, "s-a1", &a1_view);
    test_box.bay_ok(&a, &["status"]);
    let stale = remote_summary(&test_box, &bare, "box-b").unwrap();
    assert_eq!(stale["sessions"][0]["session"], "s-b1", "{stale}");
    let result = fleet(&test_box, &a);
    assert!(entry(&result, "session", "s-b1").is_none(), "{result}");
    let a1_row = entry(&result, "session", "s-a1").unwrap_or_else(|| panic!("{result}"));
    assert_eq!(a1_row["tracked"], true, "{a1_row}");
    // box-b's change, its view elsewhere and its row reaped: whether a session works on it is
    // unknown here, never untracked and never idle.
    let unknown = entry(&result, "change", &b1).unwrap_or_else(|| panic!("{result}"));
    assert_eq!(unknown["state"], "unknown", "{unknown}");
    assert!(unknown["tracked"].is_null(), "{unknown}");
    // Its actor is still one the cache knows, from box-b's summary.
    assert!(
        result["detail"]["actors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|actor| actor["machines"]
                .as_array()
                .unwrap()
                .contains(&Value::from("box-b"))),
        "{result}"
    );
}

/// The Unix time now.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Unix time `secs` as bay's records write it (`YYYY-MM-DDTHH:MM:SSZ`).
fn rfc3339(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap();
    let rest = secs % 86_400;
    // Civil from days (H. Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Runs git in `dir` with `input` on stdin; its trimmed stdout.
fn git_input(test_box: &TestBox, dir: &Path, args: &[&str], input: &[u8]) -> String {
    let mut child = test_box
        .git(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Pushes a live commit whose `live.json` is `body` to the remote's `refs/bay/live/<machine>`,
/// signed with `key` when one is given, as another machine would.
fn push_live(test_box: &TestBox, repo: &Path, machine: &str, body: &[u8], key: Option<&Path>) {
    let blob = git_input(test_box, repo, &["hash-object", "-w", "--stdin"], body);
    let tree = git_input(
        test_box,
        repo,
        &["mktree"],
        format!("100644 blob {blob}\tlive.json\n").as_bytes(),
    );
    let signing = key.map(|key| format!("user.signingkey={}", key.display()));
    let mut args = vec!["-c", "gpg.format=ssh"];
    if let Some(signing) = &signing {
        args.extend(["-c", signing, "commit-tree", &tree, "-S"]);
    } else {
        args.extend(["commit-tree", &tree, "--no-gpg-sign"]);
    }
    args.extend(["-m", "live"]);
    let commit = test_box.git_ok(repo, &args);
    test_box.git_ok(
        repo,
        &[
            "push",
            "-q",
            "origin",
            &format!("+{commit}:refs/bay/live/{machine}"),
        ],
    );
}

/// A summary of `machine` written at `at` with one working row per `(session, actor, at)`.
fn summary(machine: &str, at: u64, rows: &[(&str, &str, u64)]) -> Value {
    let sessions: Vec<Value> = rows
        .iter()
        .map(|(session, actor, row_at)| {
            serde_json::json!({"session": session, "actor": {"name": actor, "session": session},
                "state": "working", "event": "post_tool_batch", "at": rfc3339(*row_at)})
        })
        .collect();
    serde_json::json!({"schema": "bay.live/1", "machine": machine, "at": rfc3339(at),
        "sessions": sessions})
}

#[test]
fn this_machines_sessions_come_from_its_records() {
    let test_box = TestBox::new();
    let (_bare, a, _b) = two_machines(&test_box, MAP_TAKEOVER_S);
    let (live, live_view) = test_box.start_and_edit(&a);
    let (ended, ended_view) = test_box.start_and_edit(&a);
    let (abandoned, abandoned_view) = test_box.start_and_edit(&a);
    let (code, result) =
        support::result_json(test_box.bay(&abandoned_view).args(["land", "--abandon"]));
    assert_eq!(code, 0, "{result}");
    // box-a's pushes fail from here on: its sessions never reach the remote's summary.
    test_box.git_ok(
        &a,
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            "/nonexistent/remote.git",
        ],
    );
    session_start(&test_box, "s-live", &live_view);
    step(&test_box, "s-live", &live_view);
    session_start(&test_box, "s-end", &ended_view);
    let end = fixture("16-SessionEnd.json", "s-end", &ended_view);
    assert_eq!(hook(&test_box, &ended_view, "session-end", &end), Some(0));
    test_box.bay_ok(&a, &["status"]);
    let result = fleet(&test_box, &a);
    let row = entry(&result, "session", "s-live").unwrap_or_else(|| panic!("{result}"));
    assert_eq!(row["machine"], "box-a", "{row}");
    assert_eq!(row["change"], live.as_str(), "{row}");
    assert_eq!(row["tracked"], true, "{row}");
    // A session that ended is no session, and its change had a hooked one: not untracked. An
    // abandoned change is not held.
    assert!(entry(&result, "session", "s-end").is_none(), "{result}");
    assert!(entry(&result, "change", &ended).is_none(), "{result}");
    assert!(entry(&result, "change", &abandoned).is_none(), "{result}");
    let machine = &result["detail"]["machines"][0];
    assert_eq!(machine["machine"], "box-a", "{result}");
    assert_eq!(machine["source"], "local records", "{result}");
}

#[test]
fn bay_status_leaves_the_live_refs_alone_and_the_pusher_pushes_what_it_wrote() {
    let test_box = TestBox::new();
    let (bare, a, _b) = two_machines(&test_box, MAP_TAKEOVER_S);
    let (_, view) = test_box.start_and_edit(&a);
    session_start(&test_box, "s-p", &view);
    heartbeat(&test_box, &bare, "box-a", &[("s-p", &view)]);
    pusher_idle(&a);
    // This machine's live ref names a summary the remote does not hold: `bay status` fetches
    // the remote's live refs without moving it.
    let other = test_box.git_ok(&a, &["rev-parse", "HEAD"]);
    test_box.git_ok(&a, &["update-ref", "refs/bay/live/box-a", &other]);
    test_box.bay_ok(&a, &["status"]);
    assert_eq!(
        test_box.git_ok(&a, &["rev-parse", "refs/bay/live/box-a"]),
        other
    );
    assert!(fleet(&test_box, &a)["detail"]["machines"][0]["commit"].is_string());

    // A fetch that moves or prunes the local live ref between the pusher's write and its push
    // (here a hook deletes it) does not change what the pusher pushes.
    support::write_executable(
        &a.join(".git/hooks/reference-transaction"),
        "#!/bin/sh\n[ \"$1\" = committed ] || exit 0\n[ -z \"$BAY_TEST_IN_HOOK\" ] || exit 0\nwhile read -r old new name; do\n  case \"$name\" in\n    refs/bay/live/*) BAY_TEST_IN_HOOK=1 git update-ref -d \"$name\" ;;\n  esac\ndone\nexit 0\n",
    );
    // No hook event here: one would start a detached pusher that holds the lock.
    let pushed = test_box.bay_ok(&a, &["live", "push"]);
    assert_eq!(pushed["detail"]["pushed"], true, "{pushed}");
    let commit = pushed["detail"]["commit"].as_str().unwrap();
    assert_eq!(
        test_box.git_bare(&bare, &["rev-parse", "refs/bay/live/box-a"]),
        commit
    );
}

#[test]
fn a_rows_age_never_compares_two_machines_clocks() {
    let test_box = TestBox::new();
    let (_bare, a, _b) = two_machines(&test_box, MAP_TAKEOVER_S);
    // box-slow's clock runs an hour behind this one: by its own clock its row is 2 s old, and
    // the row of s-gone is two windows old.
    let slow = unix_now() - 3600;
    let body = summary(
        "box-slow",
        slow,
        &[
            ("s-slow", "slow-agent", slow - 2),
            ("s-gone", "ghost", slow - 2 * MAP_TAKEOVER_S),
        ],
    );
    push_live(&test_box, &a, "box-slow", body.to_string().as_bytes(), None);
    test_box.bay_ok(&a, &["status"]);
    let result = fleet(&test_box, &a);
    let row = entry(&result, "session", "s-slow").unwrap_or_else(|| panic!("{result}"));
    assert!(
        row["age_s"]
            .as_u64()
            .is_some_and(|age| age < MAP_TAKEOVER_S),
        "{row}"
    );
    assert!(entry(&result, "session", "s-gone").is_none(), "{result}");
    // Every actor the summary names is one the cache knows, a stale row's too.
    let actors: Vec<&str> = result["detail"]["actors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|actor| actor["actor"].as_str().unwrap())
        .collect();
    assert!(
        actors.contains(&"ghost") && actors.contains(&"slow-agent"),
        "{result}"
    );
}

#[test]
fn an_unreadable_summary_spoils_only_its_own_machine() {
    let test_box = TestBox::new();
    let (_bare, a, _b) = two_machines(&test_box, MAP_TAKEOVER_S);
    let now = unix_now();
    let mut big = summary("box-big", now, &[("s-big", "agent-big", now)]);
    big["pad"] = Value::from("x".repeat(40 * 1024));
    push_live(&test_box, &a, "box-big", big.to_string().as_bytes(), None);
    push_live(&test_box, &a, "box-odd", b"{\"at\": \"\xff\"}\n", None);
    let ok = summary("box-ok", now, &[("s-ok", "agent-ok", now)]);
    push_live(&test_box, &a, "box-ok", ok.to_string().as_bytes(), None);
    test_box.bay_ok(&a, &["status"]);
    let result = fleet(&test_box, &a);
    let machine = |name: &str| {
        result["detail"]["machines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|machine| machine["machine"] == name)
            .unwrap_or_else(|| panic!("{name}: {result}"))
            .clone()
    };
    assert_eq!(machine("box-big")["readable"], false, "{result}");
    assert_eq!(machine("box-odd")["readable"], false, "{result}");
    assert_eq!(machine("box-ok")["readable"], true, "{result}");
    assert!(entry(&result, "session", "s-ok").is_some(), "{result}");
    assert!(entry(&result, "session", "s-big").is_none(), "{result}");
}

/// The `SHA256:…` fingerprint of `key`.
fn fingerprint(key: &Path) -> String {
    let out = std::process::Command::new("ssh-keygen")
        .arg("-lf")
        .arg(key.with_extension("pub"))
        .output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned()
}

#[test]
fn under_a_registry_only_a_summary_its_agent_signed_lists_sessions() {
    let test_box = TestBox::new();
    let (_bare, a, _b) = two_machines(&test_box, MAP_TAKEOVER_S);
    let keys = test_box.keys();
    test_box.enroll(&a, &keys);
    test_box.use_keys(&a, &keys);
    let now = unix_now();
    let signed = |machine: &str, session: &str| {
        let mut body = summary(machine, now, &[(session, support::AGENT, now)]);
        body["signer"] = Value::from(support::AGENT);
        body["key"] = Value::from(fingerprint(&keys.agent));
        body.to_string()
    };
    let body = signed("box-signed", "s-signed");
    push_live(
        &test_box,
        &a,
        "box-signed",
        body.as_bytes(),
        Some(&keys.agent),
    );
    // Another key signs a summary naming the agent and its key.
    let body = signed("box-rogue", "s-rogue");
    push_live(
        &test_box,
        &a,
        "box-rogue",
        body.as_bytes(),
        Some(&keys.rogue),
    );
    let body = signed("box-unsigned", "s-unsigned");
    push_live(&test_box, &a, "box-unsigned", body.as_bytes(), None);
    test_box.bay_ok(&a, &["status"]);
    let result = fleet(&test_box, &a);
    assert!(entry(&result, "session", "s-signed").is_some(), "{result}");
    assert!(entry(&result, "session", "s-rogue").is_none(), "{result}");
    assert!(
        entry(&result, "session", "s-unsigned").is_none(),
        "{result}"
    );
    let readable: Vec<(&str, bool)> = result["detail"]["machines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|machine| {
            (
                machine["machine"].as_str().unwrap(),
                machine["readable"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        readable,
        [
            ("box-rogue", false),
            ("box-signed", true),
            ("box-unsigned", false)
        ],
        "{result}"
    );
}

#[test]
fn a_stop_over_uncheckpointed_edits_is_refused_once() {
    // Ruling R7: the session's first stop over edits no checkpoint records is refused, with the
    // two commands that record them; the product's second stop (`stop_hook_active`) is allowed,
    // and the session is idle with its edits still uncheckpointed.
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    let (id, view) = test_box.start_and_edit(&repo);
    let record = repo.join(".git/bay/sessions/s-stop.jsonl");
    let start = fixture("01-SessionStart.json", "s-stop", &view);
    assert_eq!(hook(&test_box, &view, "session-start", &start), Some(0));
    std::fs::write(view.join("src/a.rs"), "fn a() { 1; }\n").unwrap();

    let stop = fixture("09-Stop.json", "s-stop", &view);
    assert_eq!(stop["stop_hook_active"], false);
    let (code, stderr) = hook_output(&test_box, &view, "stop", &stop);
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains(&format!("bay start --resume {id}")) && stderr.contains("bay land"),
        "{stderr}"
    );
    let line = last_line(&record);
    assert_eq!(line["event"], "stop", "{line}");
    assert_eq!(line["state"], "working", "{line}");
    assert_eq!(line["stop_refused"], true, "{line}");
    assert_eq!(line["uncheckpointed"], true, "{line}");
    assert_eq!(wrote(&line), ["src/a.rs"], "{line}");

    let mut again = stop.clone();
    again["stop_hook_active"] = Value::from(true);
    let (code, stderr) = hook_output(&test_box, &view, "stop", &again);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stderr, "");
    let line = last_line(&record);
    assert_eq!(line["state"], "idle", "{line}");
    assert_eq!(line["uncheckpointed"], true, "{line}");
    assert!(line.get("stop_refused").is_none(), "{line}");
}

#[test]
fn a_clean_view_never_blocks_a_stop() {
    // A view whose edits a checkpoint records, and whose sparse paths are absent, is clean: its
    // stop is allowed, also the first one. The checkpoint leaves the view's HEAD behind its
    // latest draft, so the draft, not HEAD, is what the view is held against.
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n"), ("docs/b.md", "b\n")]);
    let (id, view) = test_box.start_and_edit(&repo);
    std::fs::write(view.join("src/a.rs"), "fn a() { 1; }\n").unwrap();
    std::fs::write(view.join("src/gone.rs"), "fn gone() {}\n").unwrap();
    test_box.bay_ok(&repo, &["start", "--resume", &id]);
    let record = repo.join(".git/bay/sessions/s-clean.jsonl");
    let start = fixture("01-SessionStart.json", "s-clean", &view);
    assert_eq!(hook(&test_box, &view, "session-start", &start), Some(0));

    for name in ["09-Stop.json", "20-Stop-denied.json"] {
        let stop = fixture(name, "s-clean", &view);
        assert_eq!(stop["stop_hook_active"], false);
        let (code, stderr) = hook_output(&test_box, &view, "stop", &stop);
        assert_eq!(code, 0, "{name}: {stderr}");
        let line = last_line(&record);
        assert_eq!(line["state"], "idle", "{line}");
        assert_eq!(line["uncheckpointed"], false, "{line}");
    }

    // Deleting a checkpointed file is an edit; putting it back makes the view clean again.
    std::fs::remove_file(view.join("src/gone.rs")).unwrap();
    let stop = fixture("20-Stop-denied.json", "s-clean", &view);
    assert_eq!(hook_output(&test_box, &view, "stop", &stop).0, 2);
    std::fs::write(view.join("src/gone.rs"), "fn gone() {}\n").unwrap();
    assert_eq!(hook_output(&test_box, &view, "stop", &stop).0, 0);
    assert_eq!(last_line(&record)["uncheckpointed"], false);
}

/// A new change on `src/` over trunk's `src/a.rs`, one file written in its view, and the
/// session `session` started there.
struct StopView {
    test_box: TestBox,
    repo: PathBuf,
    id: String,
    view: PathBuf,
    record: PathBuf,
}

impl StopView {
    fn new(session: &str) -> Self {
        let test_box = TestBox::new();
        let repo = test_box.repo_any("repo");
        test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
        let (id, view) = test_box.start_and_edit(&repo);
        let record = repo.join(format!(".git/bay/sessions/{session}.jsonl"));
        let start = fixture("01-SessionStart.json", session, &view);
        assert_eq!(hook(&test_box, &view, "session-start", &start), Some(0));
        Self {
            test_box,
            repo,
            id,
            view,
            record,
        }
    }

    /// The exit code of `bay hook <event>` with `payload`.
    fn exit(&self, event: &str, payload: &Value) -> i32 {
        hook_output(&self.test_box, &self.view, event, payload).0
    }

    fn checkpoint(&self) {
        self.test_box
            .bay_ok(&self.repo, &["start", "--resume", &self.id]);
    }
}

#[test]
fn the_same_edits_are_refused_once_and_new_edits_again() {
    // Ruling R7's "once" is bay's own guard as well: the recordings show `stop_hook_active` only
    // as false, so a product that sends false again must not loop the session. The same edits
    // are refused once; an edit made after the refusal is new and is refused once more.
    let at = StopView::new("s-once");
    let stop = fixture("09-Stop.json", "s-once", &at.view);
    assert_eq!(at.exit("stop", &stop), 2);
    // The same edits, now past the racy window: their stat moved, their content did not.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(at.view.join("src/new.rs"))
        .unwrap()
        .set_modified(old)
        .unwrap();
    assert_eq!(at.exit("stop", &stop), 0);
    let line = last_line(&at.record);
    assert_eq!(line["state"], "idle", "{line}");
    assert_eq!(line["uncheckpointed"], true, "{line}");
    assert!(line.get("stop_refused").is_none(), "{line}");

    std::fs::write(at.view.join("src/a.rs"), "fn a() { 2; }\n").unwrap();
    assert_eq!(at.exit("stop", &stop), 2);
    assert_eq!(at.exit("stop", &stop), 0);
}

#[test]
fn a_stop_that_cannot_be_judged_is_never_refused() {
    // Unknown is never a refusal: a stop with no `stop_hook_active`, a subagent's stop, and a
    // view past the path bound are allowed, and the line says what is known.
    let at = StopView::new("s-unknown");
    let mut stop = fixture("09-Stop.json", "s-unknown", &at.view);
    stop.as_object_mut().unwrap().remove("stop_hook_active");
    assert_eq!(at.exit("stop", &stop), 0);
    let line = last_line(&at.record);
    assert_eq!(line["uncheckpointed"], true, "{line}");
    assert!(line.get("stop_refused").is_none(), "{line}");

    let sub = fixture("13-SubagentStop.json", "s-unknown", &at.view);
    assert_eq!(at.exit("subagent-stop", &sub), 0);

    for index in 0..2100 {
        std::fs::write(at.view.join(format!("src/many{index}.rs")), "// many\n").unwrap();
    }
    let stop = fixture("09-Stop.json", "s-unknown", &at.view);
    assert_eq!(at.exit("stop", &stop), 0);
    let line = last_line(&at.record);
    assert!(line["uncheckpointed"].is_null(), "{line}");
    assert_eq!(line["uncheckpointed_unknown"], "too_many_paths", "{line}");
}

#[test]
fn a_checkpointed_link_or_filtered_file_never_blocks_a_stop() {
    // A checkpoint records what `bay start --resume` stages: a link's target, and a file as its
    // clean filter (here end-of-line conversion) stores it. The refusal names that command, so
    // after it the view is clean.
    let at = StopView::new("s-kinds");
    at.test_box
        .git_ok(&at.view, &["config", "core.autocrlf", "true"]);
    std::fs::write(at.view.join("src/crlf.txt"), "a\r\nb\r\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("a.rs", at.view.join("src/link.rs")).unwrap();
    at.checkpoint();
    let stop = fixture("09-Stop.json", "s-kinds", &at.view);
    assert_eq!(at.exit("stop", &stop), 0);
    assert_eq!(last_line(&at.record)["uncheckpointed"], false);
}

#[test]
fn a_nested_repository_or_a_mode_change_is_an_edit() {
    // An untracked nested repository is in no checkpoint, and a file the draft records as
    // executable is an edit once its mode on disk changes, also a file new in the change.
    let at = StopView::new("s-edits");
    let stop = fixture("09-Stop.json", "s-edits", &at.view);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = at.view.join("src/run.sh");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        at.checkpoint();
        assert_eq!(at.exit("stop", &stop), 0);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(at.exit("stop", &stop), 2);
        assert_eq!(last_line(&at.record)["uncheckpointed"], true);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    at.checkpoint();
    assert_eq!(at.exit("stop", &stop), 0);
    let nested = at.view.join("src/nested");
    std::fs::create_dir_all(&nested).unwrap();
    at.test_box.git_ok(&nested, &["init", "-q"]);
    std::fs::write(nested.join("inner.rs"), "fn inner() {}\n").unwrap();
    assert_eq!(at.exit("stop", &stop), 2);
    assert_eq!(last_line(&at.record)["uncheckpointed"], true);
}

#[test]
#[ignore = "G23: the notice budget (C1)"]
fn notices_follow_the_rulings() {
    panic!("G23: the notice budget is not implemented");
}
