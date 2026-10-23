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
//! `CHANGE_ALREADY_MERGED`. A publication whose executor key was revoked after signing is
//! refused with `REGISTRY_CHANGED`, even when it names the older registry; an executor with a
//! valid key re-signs it on the same tree and parent (taking it over at once, without waiting
//! out a lease, when the revoked key was the publisher's own), reusing tree-keyed evidence and rerunning commit-keyed checks, and
//! rechecking first when the same change revoked the producer key behind the pass. Revoking only
//! that producer key is `REGISTRY_CHANGED` too: the executor rechecks and re-signs. The hook
//! answers `REGISTRY_CHANGED` only when the revocation is the one reason for its refusal, and it
//! refuses a publication or a request that names a registry commit off the registry chain. An
//! executor that cannot read the newer registry yet leaves the operation waiting, never failed.
//! A live summary ref is bound to its first signer, so another actor's update of it is refused;
//! a cross-actor adopt is accepted only past the takeover window, judged by the host's reflog,
//! and only the change actor's own heartbeat delays it (ruling R1).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use support::{EXECUTOR, PRODUCER, TestBox, TestKeys, check_files, passing_result, result_json};

/// A bay host with a registry, an agent clone using the keys, and a change in a view.
fn hosted() -> (TestBox, PathBuf, PathBuf, TestKeys) {
    hosted_with(|_| check_files("true", &passing_result(1), 1, 60))
}

/// Like [`hosted`], with `files(<test box root>)` as trunk's policy and check adapter.
fn hosted_with(
    files: impl FnOnce(&TestBox) -> Vec<(String, String)>,
) -> (TestBox, PathBuf, PathBuf, TestKeys) {
    let test_box = TestBox::new();
    let files = files(&test_box);
    let files = files.as_slice();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    // Trunk is seeded before the registry exists: from then on only executors publish.
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], files);
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
    // The box's environment names a test identity; the commit is `name`'s.
    let out = test_box
        .git(repo)
        .args(&args)
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
    let (code, _) = result_json(test_box.bay(&view).arg("merge"));
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
        .args(["runner", "run", "--once"])
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

    let ran = test_box.bay_ok(&e2, &["runner", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["decision"], "merged", "{ran}");
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
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
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
        why.contains("main takes only commits signed by a runner key"),
        "{why}"
    );

    // An executor key signs a request.
    let request_tree = blob_tree(
        &test_box,
        &repo,
        "request.json",
        &format!(
            "{{\"change\":\"{id}\",\"revision\":\"{trunk}\",\"actor\":{{\"name\":\"agent\"}},\"registry\":\"{registry}\"}}\n"
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
    // The producer's own signed record about one tree, copied onto another tree's note: a
    // pass for one tree never stands for another (G7).
    let mut copied: bay::evidence::EvidenceRecord = serde_json::from_value(serde_json::json!({
        "schema": "bay.evidence/1", "id": format!("{PRODUCER}/copied"), "kind": "check",
        "subject": {"tree": tree, "commit": trunk, "base_tree": tree},
        "check": {"id": "tests", "environment": "local"}, "policy_revision": 1,
        "manifest_digest": "sha256:x", "producer": {"name": PRODUCER, "version": "1"},
        "verdict": "pass", "at": "now"}))
    .unwrap();
    copied.sign(&keys.producer).unwrap();
    let notes = format!("refs/notes/bay/evidence/{PRODUCER}");
    // From the remote's notes, without the forgeries above.
    test_box.git_ok(
        &repo,
        &["fetch", "-q", "origin", &format!("+{notes}:{notes}")],
    );
    test_box.git_ok(
        &repo,
        &[
            "notes",
            &format!("--ref={notes}"),
            "add",
            "-f",
            "-m",
            &serde_json::to_string(&vec![copied]).unwrap(),
            &request_tree,
        ],
    );
    let why = refused(&test_box, &repo, &[&format!("+{notes}:{notes}")]);
    assert!(why.contains("not the"), "{why}");

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
                ("runner", &format!("exec-1 {key_line}\n")),
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
        why.contains("is not signed by runner someone-else"),
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
    let (code, _) = result_json(test_box.bay(&view).arg("merge"));
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
    let ran = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["decision"], "merged", "{ran}");
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

/// A registry-shaped commit that is not on `refs/bay/registry`: anyone can write one. Its agent
/// file enrols `agent` under the agent principal.
fn forged_registry(test_box: &TestBox, repo: &Path, agent: &Path, keys: &TestKeys) -> String {
    let line = |principal: &str, key: &Path| {
        let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
        let words: Vec<&str> = public.split_whitespace().collect();
        format!("{principal} {} {}\n", words[0], words[1])
    };
    let tree = files_tree(
        test_box,
        repo,
        &[
            ("agent", &line(support::AGENT, agent)),
            ("runner", &line(EXECUTOR, &keys.executor)),
            ("registry", &line(support::REGISTRAR, &keys.registry)),
        ],
    );
    signed_commit(
        test_box,
        repo,
        &keys.rogue,
        support::REGISTRAR,
        &tree,
        &[],
        "forged registry",
    )
}

/// A request JSON for `change` at `revision` by the agent principal, naming `registry`.
fn request_text(change: &str, revision: &str, registry: &str) -> String {
    format!(
        "{{\"change\":\"{change}\",\"revision\":\"{revision}\",\"actor\":{{\"name\":\"{}\"}},\"registry\":\"{registry}\"}}\n",
        support::AGENT
    )
}

/// § 10a: a request is judged under the registry version it names, and the hook requires that
/// version to be on the registry chain when the request is pushed. A request signed by a
/// currently valid agent key, naming a registry commit off the chain (even one pushed beside
/// it), is refused at its push.
#[test]
fn a_request_naming_a_registry_off_the_chain_is_refused_at_its_push() {
    let (test_box, _host, repo, keys) = hosted();
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{landed}");
    test_box.git_ok(&repo, &["fetch", "-q", "origin"]);
    let trunk = test_box.git_ok(&repo, &["rev-parse", "origin/main"]);
    let latest = test_box.git_ok(&repo, &["rev-parse", &format!("refs/bay/requests/{id}")]);
    let forged = forged_registry(&test_box, &repo, &keys.agent, &keys);
    for named in [forged.as_str(), trunk.as_str(), "none"] {
        let tree = blob_tree(
            &test_box,
            &repo,
            "request.json",
            &request_text(&id, &trunk, named),
        );
        let request = signed_commit(
            &test_box,
            &repo,
            &keys.agent,
            support::AGENT,
            &tree,
            &[&latest],
            "request",
        );
        let why = refused(
            &test_box,
            &repo,
            &[
                &format!("+{request}:refs/bay/requests/{id}"),
                &format!("{forged}:refs/heads/forged-registry"),
            ],
        );
        assert!(why.contains("not on the registry chain"), "{named}: {why}");
    }
}

/// § 10a: a publication names a request the hook judges under the registry version the request
/// names. A request signed by a key that is enrolled nowhere, naming a forged registry commit
/// that enrols it, never verifies, so an executor-signed C naming it does not land.
#[test]
fn a_publication_naming_a_request_under_a_forged_registry_is_refused() {
    let (test_box, host, repo, keys) = hosted();
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, r1) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{r1}");
    let release = test_box.path().join("release");
    let (mut held, executor) = held_executor(&test_box, &host, &keys, &release);
    // C is signed and its evidence pushed; the executor dies before its publishing push.
    held.kill().unwrap();
    let _ = held.wait();
    let candidate = last_candidate(&test_box, &host, &id);
    let field = |key: &str| {
        test_box
            .git_ok(
                &executor,
                &[
                    "log",
                    "-1",
                    &format!("--format=%(trailers:key={key},valueonly)"),
                    &candidate,
                ],
            )
            .trim()
            .to_owned()
    };
    let (revision, request_id) = (field("Revision-Id"), field("Request-Id"));
    let forged = forged_registry(&test_box, &executor, &keys.rogue, &keys);
    let tree = blob_tree(
        &test_box,
        &executor,
        "request.json",
        &request_text(&id, &revision, &forged),
    );
    let request = signed_commit(
        &test_box,
        &executor,
        &keys.rogue,
        support::AGENT,
        &tree,
        &[&request_id],
        "request",
    );
    let message = test_box
        .git_ok(&executor, &["log", "-1", "--format=%B", &candidate])
        .replace(&request_id, &request);
    let parent = test_box.git_ok(&executor, &["rev-parse", &format!("{candidate}^1")]);
    let tree = test_box.git_ok(&executor, &["rev-parse", &format!("{candidate}^{{tree}}")]);
    let forged_c = signed_commit(
        &test_box,
        &executor,
        &keys.executor,
        EXECUTOR,
        &tree,
        &[&parent],
        &message,
    );
    let why = refused(
        &test_box,
        &executor,
        &[
            &format!("{forged_c}:refs/heads/main"),
            &format!("{forged_c}:refs/bay/landed/{id}"),
            &format!("{request}:refs/heads/forged-request"),
            &format!("{forged}:refs/heads/forged-registry"),
        ],
    );
    assert!(why.contains("not on the registry chain"), "{why}");
    assert_eq!(test_box.trunk_change_ids(&host), Vec::<String>::new());
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
        .args(["--json", "runner", "run", "--once"])
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
    let (code, r1) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{r1}");

    let release = test_box.path().join("release");
    let (held, executor) = held_executor(&test_box, &host, &keys, &release);
    std::fs::write(view.join("src/new.rs"), "fn new() { r2 }\n").unwrap();
    let (code, r2) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{r2}");
    let lost = finish(held, &release);
    assert_eq!(lost["op_id"], format!("{id}.1"), "{lost}");
    assert_eq!(lost["decision"], "ok", "{lost}");
    assert_eq!(lost["detail"]["superseded"], true, "{lost}");
    assert_eq!(test_box.trunk_change_ids(&host), Vec::<String>::new());

    let ran = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["op_id"], format!("{id}.2"), "{ran}");
    assert_eq!(ran["detail"]["ran"][0]["decision"], "merged", "{ran}");
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
    let (code, r1) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{r1}");

    let release = test_box.path().join("release");
    let (executor, _) = held_executor(&test_box, &host, &keys, &release);
    let (code, abandoned) = result_json(test_box.bay(&view).args(["abandon"]));
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
/// with the landing, `CHANGE_ALREADY_MERGED`, never left pending.
#[test]
fn a_cancel_or_newer_revision_after_the_publish_is_answered_with_the_landing() {
    let (test_box, host, repo, keys) = hosted();
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, r1) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{r1}");
    let executor = test_box.clone_of(&host, "executor");
    test_box.use_keys(&executor, &keys);
    let ran = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["decision"], "merged", "{ran}");

    let (code, cancel) = result_json(test_box.bay(&view).args(["abandon"]));
    assert_eq!(code, 10, "{cancel}");
    assert_eq!(cancel["reason"], "CHANGE_ALREADY_MERGED", "{cancel}");
    std::fs::write(view.join("src/new.rs"), "fn new() { late }\n").unwrap();
    let (code, newer) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{newer}");
    assert_eq!(newer["reason"], "CHANGE_ALREADY_MERGED", "{newer}");
    assert_eq!(test_box.trunk_change_ids(&host), vec![id]);
}

/// A new ed25519 key at `<root>/keys/<name>`, enrolled nowhere yet.
fn fresh_key(test_box: &TestBox, name: &str) -> PathBuf {
    let path = test_box.path().join("keys").join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let status = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    path
}

/// The last candidate commit the remote chain of `id` records.
fn last_candidate(test_box: &TestBox, host: &Path, id: &str) -> String {
    let record = test_box.git_bare(
        host,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    let record: serde_json::Value = serde_json::from_str(&record).unwrap();
    record["operations"][0]["attempts"]
        .as_array()
        .and_then(|attempts| attempts.last())
        .and_then(|attempt| attempt["commit"].as_str())
        .unwrap_or_else(|| panic!("no candidate in {record}"))
        .to_owned()
}

/// § 10a: the hook judges a publication's executor key against the current registry, not the
/// registry version its `Registry-Id` names. C signed under the old registry by a key revoked
/// since is refused with `REGISTRY_CHANGED`, and nothing lands.
#[test]
fn a_revoked_executor_key_cannot_publish_by_naming_the_older_registry_id() {
    let (test_box, host, repo, keys) = hosted();
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, r1) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{r1}");
    let release = test_box.path().join("release");
    let (mut held, executor) = held_executor(&test_box, &host, &keys, &release);
    // C is signed and its evidence pushed; the executor dies before its publishing push.
    held.kill().unwrap();
    let _ = held.wait();
    let signed_under = test_box.git_ok(&repo, &["rev-parse", "refs/bay/registry"]);
    let rotated = fresh_key(&test_box, "executor-2");
    test_box.enroll_executor(&repo, &keys, &rotated);

    let candidate = last_candidate(&test_box, &host, &id);
    let named = test_box.git_ok(
        &executor,
        &[
            "log",
            "-1",
            "--format=%(trailers:key=Registry-Id,valueonly)",
            &candidate,
        ],
    );
    assert_eq!(named.trim(), signed_under);
    // § 10a: `REGISTRY_CHANGED` is the answer only when the revocation is the one reason. A push
    // that does not create the landed marker is refused for that.
    let why = refused(
        &test_box,
        &executor,
        &[&format!("{candidate}:refs/heads/main")],
    );
    assert!(why.contains("must create refs/bay/landed/"), "{why}");
    assert!(!why.contains("REGISTRY_CHANGED"), "{why}");
    let why = refused(
        &test_box,
        &executor,
        &[
            &format!("{candidate}:refs/heads/main"),
            &format!("{candidate}:refs/bay/landed/{id}"),
        ],
    );
    assert!(why.contains("REGISTRY_CHANGED"), "{why}");
    assert_eq!(test_box.trunk_change_ids(&host), Vec::<String>::new());
}

/// A copy of `commit` on its own tree and parent, signed by `key` as the executor, with `from`
/// replaced by `to` in its message.
fn copy_of(
    test_box: &TestBox,
    repo: &Path,
    commit: &str,
    key: &Path,
    (from, to): (&str, &str),
) -> String {
    let message = test_box
        .git_ok(repo, &["log", "-1", "--format=%B", commit])
        .replace(from, to);
    let parent = test_box.git_ok(repo, &["rev-parse", &format!("{commit}^1")]);
    let tree = test_box.git_ok(repo, &["rev-parse", &format!("{commit}^{{tree}}")]);
    signed_commit(test_box, repo, key, EXECUTOR, &tree, &[&parent], &message)
}

/// § 10a: the hook checks that a publication's `Registry-Id` is on the registry chain. A C
/// signed by a valid executor key but naming a registry commit off the chain is refused; and
/// a C whose key the current registry revoked, naming an off-chain registry that still enrols
/// that key, is no publication at all, never `REGISTRY_CHANGED`.
#[test]
fn a_publication_naming_a_registry_off_the_chain_is_refused() {
    let (test_box, host, repo, keys) = hosted();
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, r1) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{r1}");
    let release = test_box.path().join("release");
    let (mut held, executor) = held_executor(&test_box, &host, &keys, &release);
    held.kill().unwrap();
    let _ = held.wait();
    let candidate = last_candidate(&test_box, &host, &id);
    let signed_under = test_box.git_ok(&repo, &["rev-parse", "refs/bay/registry"]);
    let forged = forged_registry(&test_box, &executor, &keys.agent, &keys);
    let push = |commit: &str| {
        refused(
            &test_box,
            &executor,
            &[
                &format!("{commit}:refs/heads/main"),
                &format!("{commit}:refs/bay/landed/{id}"),
                &format!("{forged}:refs/heads/forged-registry"),
            ],
        )
    };
    let off_chain = copy_of(
        &test_box,
        &executor,
        &candidate,
        &keys.executor,
        (&signed_under, &forged),
    );
    let why = push(&off_chain);
    assert!(why.contains("not on the registry chain"), "{why}");

    let rotated = fresh_key(&test_box, "executor-2");
    test_box.enroll_executor(&repo, &keys, &rotated);
    let why = push(&off_chain);
    assert!(
        why.contains("main takes only commits signed by a runner key"),
        "{why}"
    );
    assert!(!why.contains("REGISTRY_CHANGED"), "{why}");
    assert_eq!(test_box.trunk_change_ids(&host), Vec::<String>::new());
}

/// Two required checks that log each run to `<root>/runs`: `tests` is keyed by tree, `meta`
/// reads the commit (`inputs = ["commit"]`, ruling 9a), so its result never stands for a
/// re-signed C.
fn two_checks(test_box: &TestBox) -> Vec<(String, String)> {
    let root = test_box.path();
    let base = check_files(
        &test_box.relay(&format!("echo \"$1\" >> '{}'", root.join("runs").display())),
        &passing_result(1),
        1,
        60,
    );
    let script = base
        .iter()
        .find(|(path, _)| path == "scripts/bay-check")
        .unwrap()
        .1
        .clone();
    let policy = "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check tests\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n\n[[check]]\nid = \"meta\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check meta\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\ninputs = [\"commit\"]\n";
    vec![
        ("scripts/bay-check".to_owned(), script),
        (".bay/policy.toml".to_owned(), policy.to_owned()),
        // A lease far longer than a takeover after a revocation may take (§ 10a: a revoked
        // holder's lease is not waited out).
        (
            ".bay/config.toml".to_owned(),
            format!("[runner]\nlease_s = {LONG_LEASE_S}\nheartbeat_s = 1\n"),
        ),
    ]
}

/// The lease of [`two_checks`], in seconds.
const LONG_LEASE_S: u64 = 60;

fn runs_of(test_box: &TestBox) -> Vec<String> {
    std::fs::read_to_string(test_box.path().join("runs"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A second executor clone serving the same principal under `key`, with its own producer.
fn other_executor(
    test_box: &TestBox,
    host: &Path,
    keys: &TestKeys,
    key: &Path,
    producer: (&str, &Path),
) -> PathBuf {
    let clone = test_box.clone_of(host, "executor-2");
    test_box.use_keys(&clone, keys);
    test_box.git_ok(&clone, &["config", "bay.runnerKey", key.to_str().unwrap()]);
    test_box.git_ok(&clone, &["config", "bay.producerName", producer.0]);
    test_box.git_ok(
        &clone,
        &["config", "bay.producerKey", producer.1.to_str().unwrap()],
    );
    clone
}

/// What a re-signed publication keeps from the refused C: tree and parent; what it changes:
/// the commit, its executor key and its `Registry-Id` (now the current registry).
fn assert_resigned(test_box: &TestBox, host: &Path, refused: &str, registry: &str) {
    let tip = test_box.git_bare(host, &["rev-parse", "main"]);
    assert_ne!(tip, refused);
    for part in ["^{tree}", "^1"] {
        assert_eq!(
            test_box.git_bare(host, &["rev-parse", &format!("{tip}{part}")]),
            test_box.git_bare(host, &["rev-parse", &format!("{refused}{part}")]),
            "{part}"
        );
    }
    let named = test_box.git_bare(
        host,
        &[
            "log",
            "-1",
            "--format=%(trailers:key=Registry-Id,valueonly)",
            &tip,
        ],
    );
    assert_eq!(named.trim(), registry);
}

/// `REGISTRY_CHANGED` is not a retry of the same push (`INTERFACES` § 2): no publish retry was
/// counted toward `MERGE_RETRY_EXHAUSTED`.
fn assert_no_publish_retry(test_box: &TestBox, host: &Path, id: &str) {
    let record = test_box.git_bare(
        host,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    let record: serde_json::Value = serde_json::from_str(&record).unwrap();
    assert_eq!(
        record["operations"][0]["publish_retries"]
            .as_u64()
            .unwrap_or(0),
        0,
        "{record}"
    );
}

/// The publishing executor's key is revoked between signing and pushing: the hook refuses C
/// with `REGISTRY_CHANGED`; the executor, whose own key is the revoked one, gives the operation
/// up, and the revocation releases it: another executor under the same principal with a valid
/// key takes it over at once, without waiting out the revoked holder's lease, and re-signs C on
/// the same tree and parent. It lands once; the tree-keyed check is reused and only the
/// commit-keyed check runs again (ruling 9a).
#[test]
fn revoking_the_publishing_executor_key_between_signing_and_pushing_resigns_once() {
    let Held {
        test_box,
        host,
        repo,
        keys,
        id,
        child: held,
        release,
    } = signed_and_held();
    let refused_c = last_candidate(&test_box, &host, &id);
    let rotated = fresh_key(&test_box, "executor-2");
    let producer = fresh_key(&test_box, "producer-2");
    let registry = test_box.enroll_classes(
        &repo,
        &keys,
        &rotated,
        &[],
        &[(PRODUCER, &keys.producer), ("ci-2", &producer)],
    );

    let lost = finish(held, &release);
    assert_eq!(lost["decision"], "queued", "{lost}");
    assert_eq!(lost["reason"], "REGISTRY_CHANGED", "{lost}");
    assert_eq!(lost["detail"]["released"], true, "{lost}");
    assert_eq!(lost["detail"]["registry"], registry.as_str(), "{lost}");
    assert_eq!(test_box.trunk_change_ids(&host), Vec::<String>::new());

    let other = other_executor(&test_box, &host, &keys, &rotated, ("ci-2", &producer));
    let started = Instant::now();
    let ran = test_box.bay_ok(&other, &["runner", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["decision"], "merged", "{ran}");
    assert!(
        started.elapsed() < Duration::from_secs(LONG_LEASE_S / 2),
        "the takeover waited out the revoked holder's lease: {:?}",
        started.elapsed()
    );
    assert_eq!(test_box.trunk_change_ids(&host), vec![id.clone()]);
    assert_eq!(runs_of(&test_box), ["tests", "meta", "meta"]);
    assert_resigned(&test_box, &host, &refused_c, &registry);
    assert_no_publish_retry(&test_box, &host, &id);
}

/// `REGISTRY_CHANGED`, but the executor cannot read the newer registry (its fetch fails): it
/// re-signs nothing and never reports `failed`; the operation waits for the registry. Once the
/// registry can be read, a later pass re-signs C and lands it once.
#[test]
fn a_registry_change_the_executor_cannot_read_yet_leaves_the_operation_waiting() {
    let Held {
        test_box,
        host,
        repo,
        keys,
        id,
        child: held,
        release,
    } = signed_and_held();
    let refused_c = last_candidate(&test_box, &host, &id);
    let rotated = fresh_key(&test_box, "executor-2");
    let signed_under = test_box.git_ok(&repo, &["rev-parse", "refs/bay/registry"]);
    let registry = test_box.enroll_executor(&repo, &keys, &rotated);
    let executor = test_box.path().join("executor");
    test_box.git_ok(
        &executor,
        &["config", "bay.runnerKey", rotated.to_str().unwrap()],
    );
    // The host serves every ref but the registry to fetches; its hook still reads it.
    test_box.git_bare(
        &host,
        &["config", "uploadpack.hideRefs", "refs/bay/registry"],
    );

    let waiting = finish(held, &release);
    assert_eq!(waiting["decision"], "queued", "{waiting}");
    assert_eq!(waiting["reason"], "REGISTRY_CHANGED", "{waiting}");
    assert_eq!(waiting["detail"]["released"], false, "{waiting}");
    assert_eq!(
        waiting["detail"]["registry"],
        signed_under.as_str(),
        "{waiting}"
    );
    assert_eq!(test_box.trunk_change_ids(&host), Vec::<String>::new());

    test_box.git_bare(&host, &["config", "--unset", "uploadpack.hideRefs"]);
    let ran = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["decision"], "merged", "{ran}");
    assert_eq!(test_box.trunk_change_ids(&host), vec![id.clone()]);
    assert_eq!(runs_of(&test_box), ["tests", "meta", "meta"]);
    assert_resigned(&test_box, &host, &refused_c, &registry);
    assert_no_publish_retry(&test_box, &host, &id);
}

/// The executor's box now holds a valid key for the same principal: the executor re-signs C
/// itself, on the same tree and parent, and only the commit-keyed check runs again.
#[test]
fn an_executor_whose_box_holds_a_valid_key_re_signs_c_itself() {
    let Held {
        test_box,
        host,
        repo,
        keys,
        id,
        child: held,
        release,
    } = signed_and_held();
    let refused_c = last_candidate(&test_box, &host, &id);
    let rotated = fresh_key(&test_box, "executor-2");
    let registry = test_box.enroll_executor(&repo, &keys, &rotated);
    let executor = test_box.path().join("executor");
    test_box.git_ok(
        &executor,
        &["config", "bay.runnerKey", rotated.to_str().unwrap()],
    );

    let ran = finish(held, &release);
    assert_eq!(ran["decision"], "merged", "{ran}");
    assert_eq!(test_box.trunk_change_ids(&host), vec![id.clone()]);
    assert_eq!(runs_of(&test_box), ["tests", "meta", "meta"]);
    assert_resigned(&test_box, &host, &refused_c, &registry);
    assert_no_publish_retry(&test_box, &host, &id);
}

/// G33: a registry change that revokes only the producer key behind the pass (the executor key
/// stays valid) makes that evidence stop counting at once. The hook answers `REGISTRY_CHANGED`,
/// and the executor, whose box now holds the producer's new key, rechecks and re-signs C on
/// the same tree and parent under the current registry. It is never `failed`.
#[test]
fn a_registry_change_revoking_only_the_producer_key_rechecks_and_re_signs() {
    let Held {
        test_box,
        host,
        repo,
        keys,
        id,
        child: held,
        release,
    } = signed_and_held();
    let refused_c = last_candidate(&test_box, &host, &id);
    let producer = fresh_key(&test_box, "producer-2");
    let registry =
        test_box.enroll_classes(&repo, &keys, &keys.executor, &[], &[(PRODUCER, &producer)]);
    let executor = test_box.path().join("executor");
    test_box.git_ok(
        &executor,
        &["config", "bay.producerKey", producer.to_str().unwrap()],
    );

    let ran = finish(held, &release);
    assert_eq!(ran["decision"], "merged", "{ran}");
    assert_eq!(test_box.trunk_change_ids(&host), vec![id.clone()]);
    assert_eq!(runs_of(&test_box), ["tests", "meta", "tests", "meta"]);
    assert_resigned(&test_box, &host, &refused_c, &registry);
    assert_no_publish_retry(&test_box, &host, &id);
}

/// The same registry change also revokes the producer key whose signature backs the pass:
/// that evidence stops counting at once, so the operation is rechecked before it is
/// re-signed: both checks run again.
#[test]
fn a_revoked_producer_key_behind_the_pass_is_rechecked_before_the_re_sign() {
    let Held {
        test_box,
        host,
        repo,
        keys,
        id,
        child: held,
        release,
    } = signed_and_held();
    let refused_c = last_candidate(&test_box, &host, &id);
    let rotated = fresh_key(&test_box, "executor-2");
    let producer = fresh_key(&test_box, "producer-2");
    let registry = test_box.enroll_classes(&repo, &keys, &rotated, &[], &[("ci-2", &producer)]);

    let lost = finish(held, &release);
    assert_eq!(lost["reason"], "REGISTRY_CHANGED", "{lost}");
    let other = other_executor(&test_box, &host, &keys, &rotated, ("ci-2", &producer));
    let ran = test_box.bay_ok(&other, &["runner", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["decision"], "merged", "{ran}");
    assert_eq!(test_box.trunk_change_ids(&host), vec![id]);
    assert_eq!(runs_of(&test_box), ["tests", "meta", "tests", "meta"]);
    assert_resigned(&test_box, &host, &refused_c, &registry);
}

/// A hosted trunk with [`two_checks`], one landed request, and an executor held at its
/// publishing push after both checks passed on the signed C.
fn signed_and_held() -> Held {
    let (test_box, host, repo, keys) = hosted_with(two_checks);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, r1) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{r1}");
    let release = test_box.path().join("release");
    let (child, _) = held_executor(&test_box, &host, &keys, &release);
    assert_eq!(runs_of(&test_box), ["tests", "meta"]);
    Held {
        test_box,
        host,
        repo,
        keys,
        id,
        child,
        release,
    }
}

struct Held {
    test_box: TestBox,
    host: PathBuf,
    repo: PathBuf,
    keys: TestKeys,
    id: String,
    child: std::process::Child,
    release: PathBuf,
}

/// The takeover window of the live case; the seed adds a 2 s lease and a 1 s heartbeat.
const TAKEOVER_S: u64 = 3;
const LIVE_INTENT: &str = "CARD-LIVE";

fn live_tip(test_box: &TestBox, host: &Path, machine: &str) -> Option<String> {
    let out = test_box
        .git(test_box.path())
        .args([
            &format!("--git-dir={}", host.display()),
            "rev-parse",
            "-q",
            "--verify",
            &format!("refs/bay/live/{machine}"),
        ])
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8(out.stdout).unwrap().trim().to_owned())
}

/// One agent-hook event of session `session` running in `cwd` (the payload shape of Claude
/// Code 2.1.302).
fn a_hook_event_of(test_box: &TestBox, cwd: &Path, session: &str) {
    let payload = serde_json::json!({"session_id": session, "cwd": cwd.display().to_string(),
        "hook_event_name": "PostToolBatch", "tool_calls": []});
    let out = test_box
        .bay(cwd)
        .args(["hook", "step.end"])
        .write_stdin(payload.to_string())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
}

/// One agent-hook event of A's session in its view.
fn a_hook_event(test_box: &TestBox, view: &Path) {
    a_hook_event_of(test_box, view, "s-a");
}

/// A's session heartbeats through the agent hook (one pusher runs at a time, so it fires again
/// until a new summary lands); waits until the host holds a new summary.
fn a_heartbeat(test_box: &TestBox, host: &Path, view: &Path) -> String {
    let before = live_tip(test_box, host, "box-a");
    let asked = Instant::now();
    loop {
        a_hook_event(test_box, view);
        std::thread::sleep(Duration::from_millis(300));
        if let Some(now) = live_tip(test_box, host, "box-a")
            && before.as_ref() != Some(&now)
        {
            return now;
        }
        assert!(
            asked.elapsed() < Duration::from_secs(90),
            "the live summary of box-a never reached the host"
        );
    }
}

/// The fingerprint of the key at `key`, as `ssh-keygen -l` prints it.
fn fingerprint(key: &Path) -> String {
    let out = std::process::Command::new("ssh-keygen")
        .args(["-l", "-E", "sha256", "-f"])
        .arg(key)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout)
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned()
}

/// The current UTC time as bay's records write it.
fn now_rfc3339() -> String {
    let out = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// A `bay.live/1` summary for `machine` naming `key`, with one fresh row of change `id` by
/// `name`.
fn live_summary(key: &Path, name: &str, machine: &str, id: &str) -> serde_json::Value {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let at = now_rfc3339();
    serde_json::json!({"schema": "bay.live/1", "machine": machine, "at": at,
        "key": fingerprint(key), "signer": name, "sessions": [{"session": format!("s-{n}"),
        "actor": {"name": name, "session": "s"}, "change": id, "state": "working", "at": at}]})
}

/// A [`live_summary`] commit signed by `key` as `name`.
fn live_commit(
    test_box: &TestBox,
    repo: &Path,
    key: &Path,
    name: &str,
    machine: &str,
    id: &str,
) -> String {
    let summary = live_summary(key, name, machine, id);
    let tree = blob_tree(test_box, repo, "live.json", &summary.to_string());
    signed_commit(test_box, repo, key, name, &tree, &[], "live")
}

/// B's own adopt of change `id`, built on the head B fetched and signed with B's agent key.
fn adopt_commit(test_box: &TestBox, b: &Path, key: &Path, id: &str) -> String {
    let name = format!("refs/bay/changes/{id}");
    test_box.git_ok(b, &["fetch", "-q", "origin", &format!("+{name}:{name}")]);
    let head = test_box.git_ok(b, &["rev-parse", &name]);
    let mut record: serde_json::Value =
        serde_json::from_str(&test_box.git_ok(b, &["show", &format!("{head}:change.json")]))
            .unwrap();
    let previous = record["actor"].take();
    record["adopted_from"] = serde_json::json!([previous]);
    record["actor"] = serde_json::json!({"name": "agent-b", "session": "s-b"});
    let tree = blob_tree(
        test_box,
        b,
        "change.json",
        &format!("{}\n", serde_json::to_string_pretty(&record).unwrap()),
    );
    let parents = test_box.git_ok(b, &["rev-list", "--parents", "-n", "1", &head]);
    let draft = parents.split_whitespace().last().unwrap().to_owned();
    signed_commit(
        test_box,
        b,
        key,
        "agent-b",
        &tree,
        &[&head, &draft],
        "adopt",
    )
}

fn b_start(test_box: &TestBox, b: &Path) -> (i32, serde_json::Value) {
    result_json(test_box.bay(b).env("BAY_ACTOR", "agent-b").args([
        "start",
        LIVE_INTENT,
        "--paths",
        "src/",
    ]))
}

/// Runs `beat` about every 300 ms on another thread while `body` runs.
fn while_beating<T>(beat: impl Fn() + Sync, body: impl FnOnce() -> T) -> T {
    let done = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                beat();
                std::thread::sleep(Duration::from_millis(300));
            }
        });
        let out = body();
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        out
    })
}

/// A hosted trunk whose config sets [`TAKEOVER_S`], agent B enrolled, A's clone named box-a,
/// and B's clone (made before any change exists) named box-b.
fn live_case() -> (TestBox, PathBuf, PathBuf, PathBuf, PathBuf) {
    live_case_with(TAKEOVER_S)
}

/// [`live_case`] with its own takeover window. A row older than the window is reaped from the
/// summary, so a case that only needs a row to arrive uses a long window: on a starved box the
/// detached pusher may build the summary seconds after the event.
fn live_case_with(takeover_s: u64) -> (TestBox, PathBuf, PathBuf, PathBuf, PathBuf) {
    let (test_box, host, repo, keys) = hosted_with(|_| {
        let mut files = check_files("true", &passing_result(1), 1, 60);
        files.push((
            ".bay/config.toml".to_owned(),
            format!("[runner]\ntakeover_s = {takeover_s}\n"),
        ));
        files
    });
    let b_key = fresh_key(&test_box, "agent-b");
    test_box.enroll_with(&repo, &keys, &keys.executor, &[("agent-b", &b_key)]);
    test_box.git_ok(&repo, &["config", "bay.machine", "box-a"]);
    let b = test_box.clone_of(&host, "b");
    test_box.git_ok(&b, &["config", "bay.agentKey", b_key.to_str().unwrap()]);
    test_box.git_ok(&b, &["config", "bay.machine", "box-b"]);
    (test_box, host, repo, b, b_key)
}

#[test]
fn adoption_and_live_heartbeats_are_scoped_to_their_actor() {
    let (test_box, host, repo, b, b_key) = live_case();
    let started = test_box.bay_ok(&repo, &["start", LIVE_INTENT, "--paths", "src/"]);
    let started_at = Instant::now();
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());

    // An adopt inside the takeover window is refused at the hook, by the host's reflog. It is
    // pushed first, well inside the window, so a loaded box cannot age the chain past it.
    let adopt = adopt_commit(&test_box, &b, &b_key, &id);
    let change_ref = format!("refs/bay/changes/{id}");
    let why = refused(&test_box, &b, &[&format!("{adopt}:{change_ref}")]);
    assert!(
        why.contains("CHANGE_SUPERSEDED") && why.contains("its chain head moved"),
        "{why}"
    );

    // A's session heartbeats through the agent hook: an agent-signed summary naming its change,
    // with no local path in it.
    let tip = a_heartbeat(&test_box, &host, &view);
    let summary: serde_json::Value =
        serde_json::from_str(&test_box.git_bare(&host, &["show", &format!("{tip}:live.json")]))
            .unwrap();
    assert_eq!(summary["schema"], "bay.live/1", "{summary}");
    assert_eq!(summary["machine"], "box-a", "{summary}");
    assert!(
        summary["key"]
            .as_str()
            .is_some_and(|key| key.starts_with("SHA256:")),
        "{summary}"
    );
    let row = &summary["sessions"][0];
    assert_eq!(row["change"], id.as_str(), "{summary}");
    assert!(
        row.get("view").is_none() && row.get("repo").is_none(),
        "{summary}"
    );

    // A live update signed by actor B for A's machine ref is refused; B's own ref is B's.
    let forged = live_commit(&test_box, &b, &b_key, "agent-b", "box-a", &id);
    let why = refused(&test_box, &b, &[&format!("+{forged}:refs/bay/live/box-a")]);
    assert!(
        why.contains("refs/bay/live/box-a: is bound to agent agent"),
        "{why}"
    );
    assert_ne!(live_tip(&test_box, &host, "box-a"), Some(forged));
    let own = live_commit(&test_box, &b, &b_key, "agent-b", "box-b", &id);
    test_box.git_ok(
        &b,
        &[
            "push",
            "-q",
            "origin",
            &format!("+{own}:refs/bay/live/box-b"),
        ],
    );

    // A's own heartbeat delays adoption: B's start is refused while A's session beats, and so
    // is B's adopt past the chain's window while A's heartbeat is recent.
    let beat_a = || a_hook_event(&test_box, &view);
    let (code, refusal) = while_beating(beat_a, || b_start(&test_box, &b));
    assert_eq!(code, 10, "{refusal}");
    assert_eq!(refusal["reason"], "CHANGE_SUPERSEDED", "{refusal}");
    assert!(
        refusal["detail"]["why"]
            .as_str()
            .is_some_and(|why| why.contains("heartbeat on refs/bay/live/box-a names it")),
        "{refusal}"
    );
    // The chain head is past the window from here on, whatever the box's load was.
    let past = Duration::from_secs(TAKEOVER_S + 1);
    std::thread::sleep(past.saturating_sub(started_at.elapsed()));
    a_heartbeat(&test_box, &host, &view);
    let why = refused(&test_box, &b, &[&format!("{adopt}:{change_ref}")]);
    assert!(
        why.contains("its heartbeat on refs/bay/live/box-a"),
        "{why}"
    );

    // A's session dies without SessionEnd while a sibling session on box-a keeps firing: the
    // dead session's row ages out on its own heartbeat, so box-a's fresh summaries do not keep
    // the change un-adoptable. B's heartbeat naming A's change does not delay adoption either:
    // once A's last heartbeat is older than the window, B adopts while both beat.
    std::thread::sleep(Duration::from_secs(TAKEOVER_S + 1));
    let beat = || {
        a_hook_event_of(&test_box, &repo, "s-sibling");
        let commit = live_commit(&test_box, &b, &b_key, "agent-b", "box-b", &id);
        let _ = test_box
            .git(&b)
            .args([
                "push",
                "-q",
                "origin",
                &format!("+{commit}:refs/bay/live/box-b"),
            ])
            .output();
    };
    let sibling_before = live_tip(&test_box, &host, "box-a");
    let asked = Instant::now();
    let (code, adopted) = while_beating(beat, || b_start(&test_box, &b));
    assert_eq!(code, 0, "{adopted}");
    assert_eq!(adopted["change"], id.as_str());
    assert_eq!(adopted["detail"]["adopted"], true, "{adopted}");
    // The refusal above is the guard: a heartbeat other than A's that counted would refuse B's
    // start. This bound only catches a start that waits on the beats, which never stop while it
    // runs; it allows the gate box's load (90 s, as every background wait there).
    assert!(
        asked.elapsed() < Duration::from_secs(90),
        "a heartbeat other than A's delayed the adoption: {:?}",
        asked.elapsed()
    );
    assert_ne!(
        live_tip(&test_box, &host, "box-a"),
        sibling_before,
        "the sibling session on box-a never published"
    );
    let record = test_box.git_bare(&host, &["show", &format!("{change_ref}:change.json")]);
    assert!(record.contains("\"agent-b\""), "{record}");
}

#[test]
fn a_live_ref_holds_one_small_summary_of_its_own_machine_and_key() {
    let (test_box, host, _repo, b, b_key) = live_case();
    let id = "0000000000000000";
    let push = |commit: &str, machine: &str| {
        refused(
            &test_box,
            &b,
            &[&format!("+{commit}:refs/bay/live/{machine}")],
        )
    };
    let summary = live_summary(&b_key, "agent-b", "box-b", id);
    let sign = |tree: &str, parents: &[&str]| {
        signed_commit(&test_box, &b, &b_key, "agent-b", tree, parents, "live")
    };
    let tree = blob_tree(&test_box, &b, "live.json", &summary.to_string());

    // Unsigned.
    let unsigned = test_box.git_ok(&b, &["commit-tree", "--no-gpg-sign", &tree, "-m", "live"]);
    assert!(
        push(&unsigned, "box-b").contains("is not signed by an agent key"),
        "unsigned"
    );
    // With a parent: a live ref is one commit, not a history.
    let first = sign(&tree, &[]);
    let child = sign(&tree, &[&first]);
    let why = push(&child, "box-b");
    assert!(why.contains("is not a parentless live commit"), "{why}");
    // Another file beside live.json, or a summary past the size bound.
    let two = files_tree(
        &test_box,
        &b,
        &[("live.json", &summary.to_string()), ("payload", "x")],
    );
    let why = push(&sign(&two, &[]), "box-b");
    assert!(why.contains("must hold live.json alone"), "{why}");
    let mut big = summary.clone();
    big["padding"] = serde_json::json!("x".repeat(40 * 1024));
    let why = push(
        &sign(
            &blob_tree(&test_box, &b, "live.json", &big.to_string()),
            &[],
        ),
        "box-b",
    );
    assert!(why.contains("must hold live.json alone"), "{why}");
    // Another machine's summary, or one naming another key or another agent.
    let why = push(&first, "box-c");
    assert!(why.contains("summarises another machine"), "{why}");
    for (field, value) in [("key", "SHA256:not-this-key"), ("signer", "agent")] {
        let mut other = summary.clone();
        other[field] = serde_json::json!(value);
        let why = push(
            &sign(
                &blob_tree(&test_box, &b, "live.json", &other.to_string()),
                &[],
            ),
            "box-b",
        );
        assert!(
            why.contains("does not name the key and agent that signed it"),
            "{field}: {why}"
        );
    }

    // The good summary is accepted; the ref is never deleted, and B's key holds no second one.
    test_box.git_ok(
        &b,
        &[
            "push",
            "-q",
            "origin",
            &format!("+{first}:refs/bay/live/box-b"),
        ],
    );
    assert_eq!(live_tip(&test_box, &host, "box-b"), Some(first));
    let why = push("", "box-b");
    assert!(why.contains("a live ref is never deleted"), "{why}");
    let second = live_commit(&test_box, &b, &b_key, "agent-b", "box-c", id);
    let why = push(&second, "box-c");
    assert!(why.contains("already holds refs/bay/live/box-b"), "{why}");
    assert_eq!(live_tip(&test_box, &host, "box-c"), None);
}

/// The sessions `machine`'s summary on the host names.
fn sessions_on(test_box: &TestBox, host: &Path, machine: &str) -> Vec<String> {
    live_tip(test_box, host, machine).map_or_else(Vec::new, |tip| {
        let summary: serde_json::Value =
            serde_json::from_str(&test_box.git_bare(host, &["show", &format!("{tip}:live.json")]))
                .unwrap();
        summary["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|row| row["session"].as_str().map(str::to_owned))
            .collect()
    })
}

#[test]
fn a_burst_of_hook_events_ends_published_and_a_refused_live_push_shows_in_doctor() {
    let (test_box, host, repo, b, b_key) = live_case_with(120);
    // Two events back to back: the second comes while the first one's pusher runs, and no event
    // follows it, yet the host ends up with its state.
    a_hook_event_of(&test_box, &repo, "s-first");
    a_hook_event_of(&test_box, &repo, "s-second");
    let asked = Instant::now();
    while !sessions_on(&test_box, &host, "box-a").contains(&"s-second".to_owned()) {
        assert!(
            asked.elapsed() < Duration::from_secs(90),
            "the last event of a burst was never published: {:?}",
            sessions_on(&test_box, &host, "box-a")
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // A box whose machine name another key already holds is refused at the host; the detached
    // pusher's refusal is shown by doctor, not lost.
    let own = live_commit(
        &test_box,
        &b,
        &b_key,
        "agent-b",
        "box-b",
        "0000000000000000",
    );
    test_box.git_ok(
        &b,
        &[
            "push",
            "-q",
            "origin",
            &format!("+{own}:refs/bay/live/box-b"),
        ],
    );
    test_box.git_ok(&repo, &["config", "bay.machine", "box-b"]);
    let asked = Instant::now();
    loop {
        a_hook_event_of(&test_box, &repo, "s-third");
        let (_, doctor) = result_json(test_box.bay(&repo).arg("doctor"));
        let row = doctor["detail"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == "live")
            .cloned();
        if let Some(row) = row {
            assert_eq!(row["state"], "failed", "{row}");
            assert!(
                row["why"]
                    .as_str()
                    .is_some_and(|why| why.contains("is bound to agent agent-b")),
                "{row}"
            );
            break;
        }
        assert!(
            asked.elapsed() < Duration::from_secs(90),
            "doctor never showed the refused live push: {doctor}"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
    assert_eq!(live_tip(&test_box, &host, "box-b"), Some(own));
}
