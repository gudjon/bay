//! G21. A terminal outcome outlives its drafts.
//!
//! After the change closes, retention expires and `gc --prune=now` runs, `status` still
//! explains the outcome; a retry of the expired op id is refused as ambiguous; a new operation
//! on the same `Change-Id` is refused `CHANGE_ALREADY_LANDED`; a key revoked after a
//! publication does not make it unverifiable; the hook refuses a cut of an open change or one
//! that drops the latest request; a cut racing a late publishing push keeps what the push needs.
//! Marker compaction is not built.

mod support;

use std::cell::Cell;
use std::path::Path;

use support::{TestBox, check_files, passing_result, result_json, write_executable};

fn gc(test_box: &TestBox, dir: &Path, bare: bool) {
    let git_dir = if bare {
        format!("--git-dir={}", dir.display())
    } else {
        format!("--git-dir={}", dir.join(".git").display())
    };
    test_box.git_ok(
        test_box.path(),
        &[&git_dir, "reflog", "expire", "--expire=now", "--all"],
    );
    test_box.git_ok(test_box.path(), &[&git_dir, "gc", "-q", "--prune=now"]);
}

#[test]
fn the_outcome_is_explained_after_retention_and_gc() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[retention]\nwindow_s = 0\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);

    // A draft that is superseded before the landing, then the landed revision.
    let (id, view) = test_box.start_and_edit(&repo);
    test_box.bay_ok(&repo, &["start", "--resume", &id]);
    let superseded = test_box.git_ok(&repo, &["rev-parse", &format!("refs/bay/changes/{id}^2")]);
    std::fs::write(view.join("src/new.rs"), "fn new() { final }\n").unwrap();
    let (code, _) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0);

    let executor = test_box.clone_of(&remote, "executor");
    test_box.use_keys(&executor, &keys);
    let landed = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(landed["detail"]["ran"][0]["decision"], "landed", "{landed}");
    let cut = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert!(
        cut["detail"]["ran"][0]["retention_cut"].is_string(),
        "{cut}"
    );

    gc(&test_box, &remote, true);
    gc(&test_box, &executor, false);
    let missing = test_box
        .git(test_box.path())
        .args([
            &format!("--git-dir={}", remote.display()),
            "cat-file",
            "-e",
            &superseded,
        ])
        .status()
        .unwrap();
    assert!(!missing.success(), "the superseded draft was collected");

    // A fresh clone still gets the outcome: landed, from trunk, with its terminal record.
    let fresh = test_box.clone_of(&remote, "fresh");
    let status = test_box.bay_ok(&fresh, &["status", &id]);
    let change = &status["detail"]["changes"][0];
    assert_eq!(change["state"], "landed", "{status}");
    assert_eq!(change["retention_cut"], true);
    assert_eq!(change["terminal"][0]["op_id"], format!("{id}.1"));
    assert_eq!(change["terminal"][0]["outcome"], "published");

    // A retry of the expired op id is refused as ambiguous.
    let opened = bay::repo::Repo::open(&executor).unwrap();
    let step = Cell::new("read");
    let retry = bay::publish::execute(&opened, &id, &format!("{id}.1"), &step).unwrap();
    assert_eq!(retry.decision, bay::result::Decision::Failed);
    assert!(retry.to_json().unwrap().contains("ambiguous"));

    // A new operation on the same change is refused: the publication verifies by itself.
    std::fs::write(view.join("src/new.rs"), "fn new() { more }\n").unwrap();
    let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHANGE_ALREADY_LANDED");

    // Revoking the executor key later does not make the publication unverifiable.
    test_box.enroll_executor(&repo, &keys, &keys.rogue);
    let after = test_box.clone_of(&remote, "after-revocation");
    let status = test_box.bay_ok(&after, &["status", &id]);
    assert_eq!(
        status["detail"]["changes"][0]["state"], "landed",
        "{status}"
    );
    assert_eq!(
        status["detail"]["changes"][0]["anomalies"],
        serde_json::json!([])
    );
}

#[test]
fn the_hook_refuses_a_cut_of_an_open_change_or_one_dropping_the_latest_request() {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.git_ok(&repo, &["remote", "add", "origin", host.to_str().unwrap()]);
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);

    // An executor-signed cut of a chain whose change has not landed.
    let (open, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0);
    let cut = |id: &str, keep_request: bool| {
        let git = bay::git::Git::new(&repo);
        let mut chain = bay::change::Chain::new(&git);
        chain.sign_with(Some(bay::signing::Signer {
            key: keys.executor.clone(),
            name: support::EXECUTOR.to_owned(),
        }));
        let (head, mut record) = chain.read(id).unwrap();
        let revision = chain.drafts(id).unwrap()[0].clone();
        let request = record
            .operations
            .last()
            .and_then(|op| op.request_id.clone())
            .unwrap();
        record.retention_cut = true;
        record.operations.clear();
        let mut links = vec![revision.as_str()];
        if keep_request {
            links.push(request.as_str());
        }
        chain.cut(&head, &record, &links, "retention cut").unwrap()
    };
    let head = cut(&open, true);
    let out = test_box
        .git(&repo)
        .args([
            "push",
            "-f",
            "origin",
            &format!("{head}:refs/bay/changes/{open}"),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("retention cut of an open change is refused")
    );

    // A landed change, cut without its latest request.
    let (landed, view) = test_box.start_and_edit(&repo);
    std::fs::write(view.join("src/other.rs"), "fn other() {}\n").unwrap();
    let (code, _) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0);
    let head = cut(&landed, false);
    let out = test_box
        .git(&repo)
        .args([
            "push",
            "-f",
            "origin",
            &format!("{head}:refs/bay/changes/{landed}"),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("keeps the latest land request"));
}

#[test]
fn a_cut_racing_a_late_publishing_push_keeps_what_the_push_needs() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[retention]\nwindow_s = 0\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0);

    // The executor stalls just before its publishing push.
    let executor = test_box.clone_of(&remote, "executor");
    let release = test_box.path().join("release");
    let mut child = Killed(Some(
        test_box
            .bay_process(&executor)
            .env("BAY_TEST_PAUSE_AT", "before-publish")
            .env("BAY_TEST_PAUSE_FILE", &release)
            .args(["executor", "run", "--once"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    ));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !test_box.path().join("release.paused").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // A retention pass meanwhile cuts nothing: the change has not landed.
    let other = test_box.clone_of(&remote, "other");
    test_box.git_ok(
        &other,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/bay/changes/*:refs/bay/changes/*",
        ],
    );
    let request = test_box.git_bare(&remote, &["rev-parse", &format!("refs/bay/requests/{id}")]);
    let opened = bay::repo::Repo::open(&other).unwrap();
    assert_eq!(bay::publish::retain(&opened, &id).unwrap(), None);
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", &format!("refs/bay/requests/{id}")]),
        request
    );

    // The late push still lands, with its marker.
    std::fs::write(&release, "go").unwrap();
    assert!(child.0.take().unwrap().wait().unwrap().success());
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id.clone()]);
    test_box.git_bare(&remote, &["rev-parse", &format!("refs/bay/landed/{id}")]);
}

/// Kills a background process if the test ends before it does.
struct Killed(Option<std::process::Child>);

impl Drop for Killed {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// `status` refreshes every local chain; a fetch that fails for one change never rewrites that
/// change from another change's fetched chain (a stale `FETCH_HEAD`).
#[test]
fn a_failed_chain_fetch_never_rewrites_another_change() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[retention]\nwindow_s = 0\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (one, one_view) = test_box.start_and_edit(&repo);
    let (two, two_view) = test_box.start_and_edit(&repo);
    // status walks the chains in ref order: `cut` comes first, `other` after it.
    let ((cut, cut_view), other) = if one < two {
        ((one, one_view), two)
    } else {
        ((two, two_view), one)
    };
    let (code, landed) = result_json(test_box.bay(&cut_view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    // An executor elsewhere cuts the landed chain, so this clone's copy is behind a rewrite.
    let executor = test_box.clone_of(&remote, "executor");
    test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    let remote_cut = test_box.git_bare(&remote, &["rev-parse", &format!("refs/bay/changes/{cut}")]);
    assert!(
        test_box
            .git_bare(&remote, &["show", &format!("{remote_cut}:change.json")])
            .contains("\"retention_cut\": true"),
        "the executor cut {cut}"
    );
    // The connection drops for the other change's fetches. git fails before it writes
    // FETCH_HEAD, so FETCH_HEAD still holds the cut chain fetched just before.
    let other_ref = format!("refs/bay/changes/{other}");
    let real_git = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real_git = String::from_utf8(real_git.stdout).unwrap();
    write_executable(
        &test_box.fakebin.join("git"),
        &format!(
            "#!/bin/sh\ncase \"$*\" in *fetch*{other_ref}*) echo 'fatal: unable to access the remote' >&2; exit 128;; esac\nexec \"{}\" \"$@\"\n",
            real_git.trim()
        ),
    );
    let before = test_box.git_ok(&repo, &["rev-parse", &other_ref]);

    test_box.bay_ok(&repo, &["status"]);
    assert_eq!(
        test_box.git_ok(&repo, &["rev-parse", &other_ref]),
        before,
        "{other}'s chain was rewritten after its fetch failed"
    );
    assert_eq!(
        test_box.git_ok(&repo, &["rev-parse", &format!("refs/bay/changes/{cut}")]),
        remote_cut,
        "the cut chain is brought in"
    );
}

/// An abandoned change is released like a landed one (DESIGN § 5, § 10a): after the retention
/// window an executor cuts its chain, the hook accepts that cut, the cut keeps the latest draft
/// and the abandon request, and status still explains the outcome.
#[test]
fn an_abandoned_change_is_cut_after_retention_and_keeps_its_draft() {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.git_ok(&repo, &["remote", "add", "origin", host.to_str().unwrap()]);
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[retention]\nwindow_s = 0\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);

    let (id, view) = test_box.start_and_edit(&repo);
    let (code, abandoned) = result_json(test_box.bay(&view).args(["land", "--abandon"]));
    assert_eq!(code, 0, "{abandoned}");
    let request = abandoned["detail"]["request"].as_str().unwrap().to_owned();
    let chain_ref = format!("refs/bay/changes/{id}");
    let draft = test_box.git_bare(&host, &["rev-parse", &format!("{chain_ref}^2")]);

    let executor = test_box.clone_of(&host, "executor");
    test_box.use_keys(&executor, &keys);
    let ran = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    let cut = &ran["detail"]["ran"][0];
    assert_eq!(cut["change"], id.as_str(), "{ran}");
    assert!(
        cut["retention_cut"].is_string(),
        "the executor cut the chain: {ran}"
    );

    let head = test_box.git_bare(&host, &["rev-parse", &chain_ref]);
    let record = test_box.git_bare(&host, &["show", &format!("{head}:change.json")]);
    assert!(record.contains("\"retention_cut\": true"), "{record}");
    assert!(record.contains("\"state\": \"abandoned\""), "{record}");
    let parents = test_box.git_bare(&host, &["rev-list", "--parents", "-n", "1", &head]);
    assert!(
        parents.contains(&draft),
        "the cut keeps the latest draft: {parents}"
    );
    assert!(
        parents.contains(&request),
        "the cut keeps the abandon request: {parents}"
    );

    let status = test_box.bay_ok(&repo, &["status", &id]);
    let change = &status["detail"]["changes"][0];
    assert_eq!(change["state"], "abandoned", "{status}");
    assert_eq!(change["retention_cut"], true, "{status}");
}
