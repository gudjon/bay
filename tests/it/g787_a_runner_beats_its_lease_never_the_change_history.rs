//! G787 (owner ruling 2026-11-18, "own lease ref"; a fleet measurement the same day: each recent
//! change history on one fleet held about 4,300 heartbeat commits, a push of a second or two
//! each, and an agent's own write to its change during a long check lost "fetch first" to a
//! beat). A runner beats `refs/bay/lease/<change>` while its check runs, replaced in place by
//! compare-and-swap, never the change's history: the history moves only on real state, and the
//! claims stay on it. Every reader of a holder's liveness reads the lease beside the history.
//!
//! - A check that beats many times leaves no beat on its change's history, and releases its
//!   lease when it ends.
//! - While a holder beats, its lease names it, the history stands still, and another runner
//!   never takes the operation over; once the holder dies, another runner takes it over after
//!   one lease of quiet and lands it, and the dead holder's lease goes.
//! - An agent's write to its change during a long check is taken at once.
//! - A host with a key registry takes a lease only from the runner the history names as its
//!   holder, at that epoch.
//!
//! Guards to mutate, in `src/publish.rs`: `heartbeat` beating the change's history again (a
//! chain commit, as before G787), which fails the first and third tests; and `lease_expired`
//! watching the history alone (`crate::lease::watched` dropped), which lets the second test's
//! watcher take a live holder's operation. In `src/hook.rs`: `Judge::lease` taking any signed
//! lease, which fails the fourth.

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use support::{EXECUTOR, TestBox, check_files, passing_result, result_json};

/// A runner clone of `remote` named `name`, with a producer of its own.
fn runner(test_box: &TestBox, remote: &Path, name: &str) -> PathBuf {
    let clone = test_box.clone_of(remote, name);
    test_box.git_ok(&clone, &["config", "bay.runnerName", name]);
    test_box.git_ok(
        &clone,
        &["config", "bay.producerName", &format!("{name}-checks")],
    );
    clone
}

/// A repository whose one check first runs the snippet `before` makes of the box's root, under
/// `config`, and a change of it submitted for a runner. Returns the box, the remote, the agent's
/// clone and the change.
fn submitted(
    before: impl FnOnce(&Path) -> String,
    config: &str,
) -> (TestBox, PathBuf, PathBuf, String) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let before = test_box.relay(&before(test_box.path()));
    let mut files = check_files(&before, &passing_result(1), 1, 120);
    files.push((".bay/config.toml".to_owned(), config.to_owned()));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    (test_box, remote, repo, id)
}

/// The tip of `name` on the bare `remote`, if it holds the ref.
fn tip(test_box: &TestBox, remote: &Path, name: &str) -> Option<String> {
    let out = test_box
        .git(remote)
        .args(["rev-parse", "--verify", "-q", name])
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Waits until the remote holds a lease of change `id`, and returns its tip.
fn lease_appears(test_box: &TestBox, remote: &Path, id: &str) -> String {
    let name = format!("refs/bay/lease/{id}");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(lease) = tip(test_box, remote, &name) {
            return lease;
        }
        assert!(Instant::now() < deadline, "no lease of {id} appeared");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The subjects of change `id`'s history on the remote, newest first.
fn history(test_box: &TestBox, remote: &Path, id: &str) -> String {
    test_box.git_bare(
        remote,
        &[
            "log",
            "--first-parent",
            "--format=%s",
            &format!("refs/bay/changes/{id}"),
        ],
    )
}

fn spawned(test_box: &TestBox, clone: &Path, trace: Option<&Path>) -> Child {
    let mut command = test_box.bay_process(clone);
    command
        .args(["runner", "run", "--once", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(trace) = trace {
        command.env("BAY_TEST_GIT_TRACE", trace);
    }
    command.spawn().unwrap()
}

fn finished(child: Child) -> Value {
    let out = child.wait_with_output().unwrap();
    serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
}

/// The pushes in the trace at `trace` that name a ref under `prefix`.
fn pushes(trace: &Path, prefix: &str) -> usize {
    std::fs::read_to_string(trace)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|line| {
            line["sub"] == "push"
                && line["args"].as_array().is_some_and(|args| {
                    args.iter()
                        .any(|arg| arg.as_str().is_some_and(|arg| arg.contains(prefix)))
                })
        })
        .count()
}

#[test]
fn a_check_that_beats_leaves_its_change_history_alone() {
    // GIVEN a change whose check runs for eight seconds under a one-second heartbeat
    let check_s = 8;
    let (test_box, remote, _, id) = submitted(
        |_| format!("sleep {check_s}"),
        "[runner]\nlease_s = 30\nheartbeat_s = 1\n",
    );
    let holder = runner(&test_box, &remote, "exec-a");
    let trace = test_box.path().join("trace.jsonl");

    // WHEN a runner checks and lands it
    let began = Instant::now();
    let ran = finished(spawned(&test_box, &holder, Some(&trace)));
    let took = began.elapsed().as_secs_f64();
    assert_eq!(
        test_box.trunk_change_ids(&remote),
        vec![id.clone()],
        "{ran}"
    );

    // THEN its beats went to the lease, none to the change's history
    let subjects = history(&test_box, &remote, &id);
    let beats_on_history = subjects
        .lines()
        .filter(|subject| subject.starts_with("heartbeat "))
        .count();
    let lease_pushes = pushes(&trace, "refs/bay/lease/");
    let history_pushes = pushes(&trace, "refs/bay/changes/");
    let per_minute = |count: usize| f64::from(u32::try_from(count).unwrap()) * 60.0 / took;
    eprintln!(
        "G787 measure: a {check_s} s check in a {took:.1} s pass: {history_pushes} history pushes \
         ({:.0}/min), {lease_pushes} lease pushes ({:.0}/min), {beats_on_history} beats on the \
         history ({:.0}/min), {} states on the history",
        per_minute(history_pushes),
        per_minute(lease_pushes),
        per_minute(beats_on_history),
        subjects.lines().count(),
    );
    assert_eq!(beats_on_history, 0, "the history holds beats:\n{subjects}");
    assert!(
        lease_pushes >= 4,
        "{lease_pushes} lease pushes in an {check_s} s check"
    );
    assert!(
        history_pushes < lease_pushes,
        "{history_pushes} history pushes beside {lease_pushes} lease pushes"
    );
    // AND the lease went when the holder was done
    assert_eq!(
        tip(&test_box, &remote, &format!("refs/bay/lease/{id}")),
        None,
        "the lease outlived its check"
    );
}

#[test]
fn a_live_holder_keeps_its_operation_and_a_dead_one_is_taken_over() {
    // GIVEN a change whose first check runs for a minute and later ones at once, under a
    // three-second lease
    let (test_box, remote, _, id) = submitted(
        |root| {
            let marker = root.join("first-check");
            format!(
                "if [ -e '{marker}' ]; then :; else : > '{marker}'; sleep 60; fi",
                marker = marker.display()
            )
        },
        "[runner]\nlease_s = 3\nheartbeat_s = 1\n",
    );
    let a = runner(&test_box, &remote, "exec-a");
    let b = runner(&test_box, &remote, "exec-b");
    let lease_name = format!("refs/bay/lease/{id}");

    // WHEN runner A claims it and beats its lease
    let mut holder = spawned(&test_box, &a, None);
    let first = lease_appears(&test_box, &remote, &id);
    let lease = test_box.git_bare(&remote, &["show", &format!("{first}:lease.json")]);
    let lease: Value = serde_json::from_str(&lease).unwrap();
    let stood = tip(&test_box, &remote, &format!("refs/bay/changes/{id}"));

    // THEN the lease names its holder, its operation and its claim
    assert_eq!(lease["schema"], "bay.lease/1", "{lease}");
    assert_eq!(lease["change"], id.as_str(), "{lease}");
    assert_eq!(lease["op_id"], format!("{id}.1"), "{lease}");
    assert_eq!(lease["holder"], "exec-a", "{lease}");
    assert!(
        lease["epoch"].as_u64().is_some_and(|epoch| epoch >= 1),
        "{lease}"
    );
    assert_eq!(
        test_box.git_bare(&remote, &["rev-list", "--count", &first]),
        "1",
        "a lease is one parentless commit"
    );

    // AND another runner's visit meanwhile watches it move and takes nothing
    let watched = test_box.bay_ok(&b, &["runner", "run", "--once"]);
    let moved = tip(&test_box, &remote, &lease_name);
    assert!(
        moved.as_ref().is_some_and(|moved| *moved != first),
        "the lease never moved in place"
    );
    assert!(
        !history(&test_box, &remote, &id).contains("by exec-b"),
        "B took a live holder's operation: {watched}"
    );
    assert_eq!(
        tip(&test_box, &remote, &format!("refs/bay/changes/{id}")),
        stood,
        "the history moved while the holder only beat:\n{}",
        history(&test_box, &remote, &id)
    );

    // WHEN A dies mid-check
    holder.kill().unwrap();
    let _ = holder.wait();

    // THEN B's next visit takes it over after a lease of quiet, lands it, and A's lease goes
    let (code, taken) = result_json(test_box.bay(&b).args(["runner", "run", "--once"]));
    assert_eq!(code, 0, "{taken}");
    assert_eq!(
        test_box.trunk_change_ids(&remote),
        vec![id.clone()],
        "{taken}"
    );
    assert!(
        history(&test_box, &remote, &id).contains("by exec-b"),
        "{taken}"
    );
    assert_eq!(
        tip(&test_box, &remote, &lease_name),
        None,
        "A's lease stayed"
    );
}

#[test]
fn an_agents_write_during_a_long_check_is_taken_at_once() {
    // GIVEN a runner in a long check of a change, beating each second
    let (test_box, remote, repo, id) = submitted(
        |_| "sleep 12".to_owned(),
        "[runner]\nlease_s = 30\nheartbeat_s = 1\n",
    );
    let holder = runner(&test_box, &remote, "exec-a");
    let child = spawned(&test_box, &holder, None);
    let first = lease_appears(&test_box, &remote, &id);
    let deadline = Instant::now() + Duration::from_secs(30);
    while tip(&test_box, &remote, &format!("refs/bay/lease/{id}")).as_deref()
        == Some(first.as_str())
    {
        assert!(Instant::now() < deadline, "the lease never beat");
        std::thread::sleep(Duration::from_millis(100));
    }

    // WHEN its author gives the change up meanwhile, each write timed to land between beats
    // THEN each is taken the first time: no beat moved the history under it
    let (code, abandoned) =
        result_json(
            test_box
                .bay(&repo)
                .args(["abandon", id.as_str(), "--why", "done elsewhere"]),
        );
    assert_eq!(code, 0, "{abandoned}");
    assert_eq!(abandoned["decision"], "ok", "{abandoned}");

    // AND the runner lands nothing and leaves no lease
    let ran = finished(child);
    assert!(test_box.trunk_change_ids(&remote).is_empty(), "{ran}");
    assert_eq!(
        tip(&test_box, &remote, &format!("refs/bay/lease/{id}")),
        None,
        "{ran}"
    );
}

/// A commit of `tree` signed by `key` as `name`, with no parent.
fn signed(test_box: &TestBox, repo: &Path, key: &Path, name: &str, tree: &str) -> String {
    let signing = format!("user.signingkey={}", key.display());
    let out = test_box
        .git(repo)
        .args([
            "-c",
            "gpg.format=ssh",
            "-c",
            &signing,
            "commit-tree",
            "-S",
            tree,
            "-m",
            "lease",
        ])
        .env("GIT_AUTHOR_NAME", name)
        .env("GIT_COMMITTER_NAME", name)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// The tree of one `lease.json` holding `lease`.
fn lease_tree(test_box: &TestBox, repo: &Path, lease: &Value) -> String {
    let mut child = test_box
        .git(repo)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(
        &mut child.stdin.take().unwrap(),
        lease.to_string().as_bytes(),
    )
    .unwrap();
    let blob = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
    let mut child = test_box
        .git(repo)
        .arg("mktree")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let entry = format!("100644 blob {}\tlease.json\n", blob.trim());
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), entry.as_bytes()).unwrap();
    String::from_utf8(child.wait_with_output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_owned()
}

#[test]
fn a_host_takes_a_lease_only_from_its_holder() {
    // GIVEN a bay host with a key registry, and a runner in a long check of a change
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    let before = test_box.relay("sleep 15");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files(&before, &passing_result(1), 1, 120)
            .into_iter()
            .chain([(
                ".bay/config.toml".to_owned(),
                "[runner]\nlease_s = 30\nheartbeat_s = 1\n".to_owned(),
            )])
            .collect::<Vec<_>>(),
    );
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    let holder = test_box.clone_of(&host, "holder");
    test_box.use_keys(&holder, &keys);
    let child = spawned(&test_box, &holder, None);

    // WHEN its beats reach the host THEN the host takes them in place
    let first = lease_appears(&test_box, &host, &id);
    let deadline = Instant::now() + Duration::from_secs(30);
    let name = format!("refs/bay/lease/{id}");
    while tip(&test_box, &host, &name).as_deref() == Some(first.as_str()) {
        assert!(
            Instant::now() < deadline,
            "the host took one beat and no more"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let lease: Value =
        serde_json::from_str(&test_box.git_bare(&host, &["show", &format!("{first}:lease.json")]))
            .unwrap();
    assert_eq!(lease["holder"], EXECUTOR, "{lease}");

    // AND a lease another key signed, or one the history names no holder of, is refused
    let forged = signed(
        &test_box,
        &repo,
        &keys.agent,
        EXECUTOR,
        &lease_tree(&test_box, &repo, &lease),
    );
    let refused = |commit: &str| {
        let out = test_box
            .git(&repo)
            .args(["push", "origin", &format!("+{commit}:{name}")])
            .output()
            .unwrap();
        assert!(!out.status.success(), "the host took lease {commit}");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let why = refused(&forged);
    assert!(
        why.contains(&format!("not signed by runner {EXECUTOR}")),
        "{why}"
    );
    let mut later = lease.clone();
    later["epoch"] = serde_json::json!(lease["epoch"].as_u64().unwrap() + 7);
    let unheld = signed(
        &test_box,
        &repo,
        &keys.executor,
        EXECUTOR,
        &lease_tree(&test_box, &repo, &later),
    );
    let why = refused(&unheld);
    assert!(why.contains("holder the history names"), "{why}");

    // AND the holder's own release is taken when its check ends
    let ran = finished(child);
    assert_eq!(test_box.trunk_change_ids(&host), vec![id], "{ran}");
    assert_eq!(tip(&test_box, &host, &name), None, "{ran}");
}
