//! G33. Executors publish without agent keys, under the scoped hook.
//!
//! On a bay-hosted trunk with ref-prefix scoping on: an agent submits and exits; E1 claims and
//! dies; E2 takes over after the lease and publishes once, executor-signed, naming the
//! agent-signed request. An agent key cannot push trunk; an executor key cannot sign a
//! request; a producer key updates only its own notes ref; only the registry key advances the
//! registry, and a registry with one key in two files or an option line is refused; an executor
//! appends only to its own log; no change chain is deleted; a request signed by a key already
//! revoked is refused at its push; a later revocation does not strand a queued request. A
//! newer revision or a cancel that races the publishing push wins at the hook and the losing
//! operation is told it was superseded; a cancel or newer revision after the publish gets
//! `CHANGE_ALREADY_LANDED`.
//! `REGISTRY_CHANGED` re-signing needs a revocation between signing and pushing, and adoption
//! and live heartbeats need item 19e; those cases leave `#[ignore]`.

mod support;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use support::{EXECUTOR, PRODUCER, TestBox, TestKeys, check_files, passing_result, result_json};

/// A bay host with a registry, an agent clone using the keys, and a change in a view.
fn hosted() -> (TestBox, PathBuf, PathBuf, TestKeys) {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.git_ok(&repo, &["remote", "add", "origin", host.to_str().unwrap()]);
    // Trunk is seeded before the registry exists: from then on only executors publish.
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    (test_box, host, repo, keys)
}

fn signed_commit(
    test_box: &TestBox,
    repo: &Path,
    key: &Path,
    name: &str,
    tree: &str,
    parents: &[&str],
    message: &str,
) -> String {
    let signing = format!("user.signingkey={}", key.display());
    let user = format!("user.name={name}");
    let mut args = vec![
        "-c",
        "gpg.format=ssh",
        "-c",
        signing.as_str(),
        "-c",
        user.as_str(),
        "-c",
        "user.email=x@bay.invalid",
        "commit-tree",
        "-S",
        tree,
    ];
    for parent in parents {
        args.extend(["-p", parent]);
    }
    args.extend(["-m", message]);
    test_box.git_ok(repo, &args)
}

fn refused(test_box: &TestBox, repo: &Path, refspecs: &[&str]) -> String {
    let out = test_box
        .git(repo)
        .args(["push", "--atomic", "origin"])
        .args(refspecs)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "push {refspecs:?} was accepted: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn blob_tree(test_box: &TestBox, repo: &Path, name: &str, text: &str) -> String {
    files_tree(test_box, repo, &[(name, text)])
}

fn files_tree(test_box: &TestBox, repo: &Path, files: &[(&str, &str)]) -> String {
    let mut entries = String::new();
    for (name, text) in files {
        let mut child = test_box
            .git(repo)
            .args(["hash-object", "-w", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), text.as_bytes()).unwrap();
        let blob = String::from_utf8(child.wait_with_output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_owned();
        {
            use std::fmt::Write;
            let _ = writeln!(entries, "100644 blob {blob}\t{name}");
        }
    }
    let mut child = test_box
        .git(repo)
        .arg("mktree")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), entries.as_bytes()).unwrap();
    String::from_utf8(child.wait_with_output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_owned()
}

#[test]
fn a_dead_executor_is_replaced_and_the_publication_is_executor_signed() {
    let (test_box, host, repo, keys) = hosted();
    let started = test_box.path().join("check-started");
    let marker = test_box.path().join("checked-once");
    // A second trunk commit whose first check is slow (pushed through bay is not needed: the
    // policy is already on trunk; this edit only changes the check script's behaviour).
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0);
    std::fs::remove_dir_all(&view).ok();

    let e1 = test_box.clone_of(&host, "e1");
    let e2 = test_box.clone_of(&host, "e2");
    for clone in [&e1, &e2] {
        test_box.use_keys(clone, &keys);
    }
    // E1 dies mid-run: kill it at its claim's first heartbeat window.
    let mut first = test_box
        .bay_process(&e1)
        .env("BAY_TEST_PAUSE_AT", "after-commit")
        .env("BAY_TEST_PAUSE_FILE", &started)
        .args(["executor", "run", "--once"])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !test_box.path().join("check-started.paused").exists() {
        assert!(Instant::now() < deadline, "E1 never claimed");
        std::thread::sleep(Duration::from_millis(50));
    }
    first.kill().unwrap();
    let _ = first.wait();
    let _ = marker;

    let ran = test_box.bay_ok(&e2, &["executor", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["decision"], "landed", "{ran}");
    assert_eq!(test_box.trunk_change_ids(&host), vec![id.clone()]);
    let tip = test_box.git_bare(&host, &["rev-parse", "main"]);
    assert!(test_box.executor_signed(&host, &keys, &tip));
    assert_eq!(
        test_box.git_bare(&host, &["log", "-1", "--format=%cn", "main"]),
        EXECUTOR
    );
    let request = test_box.git_bare(
        &host,
        &[
            "log",
            "-1",
            "--format=%(trailers:key=Request-Id,valueonly)",
            "main",
        ],
    );
    let request_json = test_box.git_bare(
        &host,
        &["show", &format!("{}:request.json", request.trim())],
    );
    assert!(request_json.contains(&id), "{request_json}");
}

#[test]
#[allow(clippy::too_many_lines)]
fn the_hook_refuses_what_a_key_class_may_not_write() {
    let (test_box, _host, repo, keys) = hosted();
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    test_box.git_ok(&repo, &["fetch", "-q", "origin"]);
    let trunk = test_box.git_ok(&repo, &["rev-parse", "origin/main"]);
    let tree = test_box.git_ok(&repo, &["rev-parse", "origin/main^{tree}"]);
    let registry = test_box.git_ok(&repo, &["rev-parse", "refs/bay/registry"]);

    // An agent key signs a publication-shaped commit and pushes it as trunk.
    let forged = signed_commit(
        &test_box,
        &repo,
        &keys.agent,
        EXECUTOR,
        &tree,
        &[&trunk],
        &format!("forged\n\nChange-Id: aaaaaaaa\nRevision-Id: {trunk}\nRegistry-Id: {registry}\n"),
    );
    let why = refused(
        &test_box,
        &repo,
        &[
            &format!("{forged}:refs/heads/main"),
            &format!("{forged}:refs/bay/landed/aaaaaaaa"),
        ],
    );
    assert!(
        why.contains("trunk takes only commits signed by an executor key"),
        "{why}"
    );

    // An executor key signs a request.
    let request_tree = blob_tree(
        &test_box,
        &repo,
        "request.json",
        &format!(
            "{{\"change\":\"{id}\",\"revision\":\"{trunk}\",\"actor\":{{\"name\":\"agent\"}}}}\n"
        ),
    );
    let request = signed_commit(
        &test_box,
        &repo,
        &keys.executor,
        "agent",
        &request_tree,
        &[&trunk],
        "request",
    );
    let why = refused(
        &test_box,
        &repo,
        &[&format!("+{request}:refs/bay/requests/{id}")],
    );
    assert!(
        why.contains("not signed by its actor agent's agent key"),
        "{why}"
    );

    // A producer key writes a record for another producer's ref, and an agent key writes one.
    for (key, producer) in [(&keys.producer, "other-checks"), (&keys.agent, PRODUCER)] {
        let mut record: serde_json::Value = serde_json::json!({
            "schema": "bay.evidence/1", "id": format!("{producer}/forged"), "kind": "check",
            "subject": {"tree": tree, "commit": trunk, "base_tree": tree},
            "check": {"id": "tests", "environment": "local"}, "policy_revision": 1,
            "manifest_digest": "sha256:x", "producer": {"name": producer, "version": "1"},
            "verdict": "pass", "at": "now"});
        let mut parsed: bay::evidence::EvidenceRecord =
            serde_json::from_value(record.take()).unwrap();
        parsed.sign(key).unwrap();
        let notes = format!("refs/notes/bay/evidence/{producer}");
        let text = serde_json::to_string(&vec![parsed]).unwrap();
        test_box.git_ok(
            &repo,
            &[
                "notes",
                &format!("--ref={notes}"),
                "add",
                "-f",
                "-m",
                &text,
                &tree,
            ],
        );
        let why = refused(&test_box, &repo, &[&format!("+{notes}:{notes}")]);
        assert!(
            why.contains("not signed by producer") || why.contains("is not producer"),
            "{why}"
        );
    }

    // Only the registry key advances the registry; a registry with one key in two files is refused.
    let agent_line = std::fs::read_to_string(keys.agent.with_extension("pub")).unwrap();
    let agent_line: Vec<&str> = agent_line.split_whitespace().collect();
    let key_line = format!("{} {}", agent_line[0], agent_line[1]);
    let registrar = std::fs::read_to_string(keys.registry.with_extension("pub")).unwrap();
    let registrar: Vec<&str> = registrar.split_whitespace().collect();
    let registry_line = format!("admin {} {}\n", registrar[0], registrar[1]);
    // A valid next version, signed by the agent key: refused for its signer.
    let valid = files_tree(
        &test_box,
        &repo,
        &[
            ("agent", &format!("agent {key_line}\n")),
            ("registry", &registry_line),
        ],
    );
    let next = signed_commit(
        &test_box,
        &repo,
        &keys.agent,
        "admin",
        &valid,
        &[&registry],
        "rotate",
    );
    let why = refused(&test_box, &repo, &[&format!("{next}:refs/bay/registry")]);
    assert!(why.contains("not signed by a registry key"), "{why}");
    // Signed by the registry key, but one key in two class files, or an option line: refused.
    for bad in [
        files_tree(
            &test_box,
            &repo,
            &[
                ("agent", &format!("agent {key_line}\n")),
                ("executor", &format!("exec-1 {key_line}\n")),
                ("registry", &registry_line),
            ],
        ),
        files_tree(
            &test_box,
            &repo,
            &[
                ("agent", &format!("agent cert-authority {key_line}\n")),
                ("registry", &registry_line),
            ],
        ),
    ] {
        let next = signed_commit(
            &test_box,
            &repo,
            &keys.registry,
            "admin",
            &bad,
            &[&registry],
            "rotate",
        );
        let why = refused(&test_box, &repo, &[&format!("{next}:refs/bay/registry")]);
        assert!(why.contains("refs/bay/registry: registry"), "{why}");
    }

    // An executor appends only to its own log, and no change chain is deleted.
    let log_tree = blob_tree(&test_box, &repo, "record.json", "{}\n");
    let entry = signed_commit(
        &test_box,
        &repo,
        &keys.executor,
        EXECUTOR,
        &log_tree,
        &[],
        "terminal record",
    );
    let why = refused(
        &test_box,
        &repo,
        &[&format!("{entry}:refs/bay/log/someone-else")],
    );
    assert!(
        why.contains("is not signed by executor someone-else"),
        "{why}"
    );
    let why = refused(&test_box, &repo, &[&format!(":refs/bay/changes/{id}")]);
    assert!(why.contains("never deleted"), "{why}");
    let _ = landed;
}

#[test]
fn a_revoked_agent_key_is_refused_at_push_but_a_queued_request_is_not_stranded() {
    let (test_box, host, repo, keys) = hosted();
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0);
    // The agent's key is revoked after its request was queued.
    test_box.enroll_with(&repo, &keys, &keys.executor, &[]);
    let rotated = support::TestKeys {
        agent: keys.rogue.clone(),
        ..test_box.keys()
    };
    test_box.enroll(&repo, &rotated);
    let executor = test_box.clone_of(&host, "executor");
    test_box.use_keys(&executor, &keys);
    let ran = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["decision"], "landed", "{ran}");
    assert_eq!(test_box.trunk_change_ids(&host), vec![id]);

    // A new change and request signed by the revoked key are refused at their push.
    let out = test_box
        .bay(&repo)
        .args(["--json", "start", "LATER", "--paths", "src/"])
        .output()
        .unwrap();
    assert_ne!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("could not be pushed"));
    assert_eq!(test_box.trunk_change_ids(&host).len(), 1);
}

/// An executor clone running `executor run --once`, held at `before-publish` until `release`
/// exists: it has built, checked and confirmed it still owns the operation, and its publishing
/// push has not started.
fn held_executor(
    test_box: &TestBox,
    host: &Path,
    keys: &TestKeys,
    release: &Path,
) -> (std::process::Child, PathBuf) {
    let executor = test_box.clone_of(host, "executor");
    test_box.use_keys(&executor, keys);
    let child = test_box
        .bay_process(&executor)
        .env("BAY_TEST_PAUSE_AT", "before-publish")
        .env("BAY_TEST_PAUSE_FILE", release)
        .args(["--json", "executor", "run", "--once"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let paused = PathBuf::from(format!("{}.paused", release.display()));
    let deadline = Instant::now() + Duration::from_secs(120);
    while !paused.exists() {
        assert!(
            Instant::now() < deadline,
            "the executor never reached its publish"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    (child, executor)
}

/// Lets a held executor push, and returns the one operation result it reports.
fn finish(child: std::process::Child, release: &Path) -> serde_json::Value {
    std::fs::write(release, "").unwrap();
    let out = child.wait_with_output().unwrap();
    let ran: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let results = ran["detail"]["ran"].as_array().cloned().unwrap_or_default();
    assert_eq!(results.len(), 1, "{ran}");
    results[0].clone()
}

/// A newer revision races the publishing push: the hook's own transaction sees the newer
/// request, refuses the publish, and the losing operation is told it was superseded (never
/// `failed`, never pending). The newer revision then lands, once.
#[test]
fn a_newer_revision_that_races_the_publishing_push_wins_with_a_stated_answer() {
    let (test_box, host, repo, keys) = hosted();
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, r1) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{r1}");

    let release = test_box.path().join("release");
    let (held, executor) = held_executor(&test_box, &host, &keys, &release);
    std::fs::write(view.join("src/new.rs"), "fn new() { r2 }\n").unwrap();
    let (code, r2) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{r2}");
    let lost = finish(held, &release);
    assert_eq!(lost["op_id"], format!("{id}.1"), "{lost}");
    assert_eq!(lost["decision"], "ok", "{lost}");
    assert_eq!(lost["detail"]["superseded"], true, "{lost}");
    assert_eq!(test_box.trunk_change_ids(&host), Vec::<String>::new());

    let ran = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["op_id"], format!("{id}.2"), "{ran}");
    assert_eq!(ran["detail"]["ran"][0]["decision"], "landed", "{ran}");
    assert_eq!(test_box.trunk_change_ids(&host), vec![id]);
    assert_eq!(
        test_box.git_bare(&host, &["show", "main:src/new.rs"]),
        "fn new() { r2 }"
    );
}

/// A cancel (the actor's abandon request) races the publishing push: the publish is refused,
/// the operation ends cancelled with a stated answer, and nothing lands.
#[test]
fn a_cancel_that_races_the_publishing_push_wins_with_a_stated_answer() {
    let (test_box, host, repo, keys) = hosted();
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, r1) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{r1}");

    let release = test_box.path().join("release");
    let (executor, _) = held_executor(&test_box, &host, &keys, &release);
    let (code, abandoned) = result_json(test_box.bay(&view).args(["land", "--abandon"]));
    assert_eq!(code, 0, "{abandoned}");
    let lost = finish(executor, &release);
    assert_eq!(lost["op_id"], format!("{id}.1"), "{lost}");
    assert_eq!(lost["decision"], "ok", "{lost}");
    assert_eq!(lost["detail"]["superseded"], true, "{lost}");
    assert_eq!(test_box.trunk_change_ids(&host), Vec::<String>::new());
    let record = test_box.git_bare(
        &host,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    let record: serde_json::Value = serde_json::from_str(&record).unwrap();
    assert_eq!(record["state"], "abandoned", "{record}");
    assert_eq!(record["operations"][0]["outcome"], "abandoned", "{record}");
}

/// The publish wins the race: a cancel or a newer revision that arrives after it is answered
/// with the landing, `CHANGE_ALREADY_LANDED`, never left pending.
#[test]
fn a_cancel_or_newer_revision_after_the_publish_is_answered_with_the_landing() {
    let (test_box, host, repo, keys) = hosted();
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, r1) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{r1}");
    let executor = test_box.clone_of(&host, "executor");
    test_box.use_keys(&executor, &keys);
    let ran = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["decision"], "landed", "{ran}");

    let (code, cancel) = result_json(test_box.bay(&view).args(["land", "--abandon"]));
    assert_eq!(code, 10, "{cancel}");
    assert_eq!(cancel["reason"], "CHANGE_ALREADY_LANDED", "{cancel}");
    std::fs::write(view.join("src/new.rs"), "fn new() { late }\n").unwrap();
    let (code, newer) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{newer}");
    assert_eq!(newer["reason"], "CHANGE_ALREADY_LANDED", "{newer}");
    assert_eq!(test_box.trunk_change_ids(&host), vec![id]);
}

#[test]
#[ignore = "REGISTRY_CHANGED re-signing is not built"]
fn revoking_the_publishing_executor_key_between_signing_and_pushing_resigns_once() {
    panic!("G33: REGISTRY_CHANGED re-signing is not implemented");
}

#[test]
#[ignore = "item 19e"]
fn adoption_and_live_heartbeats_are_scoped_to_their_actor() {
    panic!("G33: adoption and live heartbeats are not implemented");
}
