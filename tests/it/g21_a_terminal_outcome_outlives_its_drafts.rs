//! G21. A terminal outcome outlives its drafts.
//!
//! After the change closes, retention expires and `gc --prune=now` runs, `status` still
//! explains the outcome; a retry of the expired op id is refused as ambiguous; a new operation
//! on the same `Change-Id` is refused `CHANGE_ALREADY_LANDED`; a key revoked after a
//! publication does not make it unverifiable; the hook refuses a cut of an open change or one
//! that drops the latest request; a cut racing a late publishing push keeps what the push needs.
//! Past the ref budget an executor compacts the markers of a landed, cut change into the shard
//! index (`DESIGN` § 10a, ruling R11): a re-create of a compacted name is refused at the host,
//! `start` still answers `CHANGE_ALREADY_LANDED`, and the landing is still found.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

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
    test_box.add_origin(&repo, &host);
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
    test_box.add_origin(&repo, &host);
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

/// `refs/bay/landed-index/<shard>` and the path that records `name` (ruling R11): the shard is
/// the first byte of sha256(name) in decimal, the path is the name with `/` as `%2F`.
fn index_entry(name: &str) -> (String, String) {
    use sha2::Digest as _;
    let shard = sha2::Sha256::digest(name.as_bytes())[0];
    (
        format!("refs/bay/landed-index/{shard}"),
        name.replace('/', "%2F"),
    )
}

/// G21's compaction clause (`DESIGN` § 10a): over a ref budget of 1, the executor moves the
/// landed, intent and landed-intent markers of a cut change into the shard index.
#[test]
fn a_compacted_marker_is_never_created_again_and_still_answers() {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[retention]\nwindow_s = 0\nref_budget = 1\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);

    let started = test_box.bay_ok(&repo, &["start", "COMPACT-1", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let intent = started["detail"]["intent"].as_str().unwrap().to_owned();
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let marker = format!("refs/bay/landed/{id}");
    let landed_intent = intent.replace("refs/bay/intent/", "refs/bay/landed-intent/");
    let names = [marker.as_str(), intent.as_str(), landed_intent.as_str()];
    let targets: Vec<String> = names
        .iter()
        .map(|name| test_box.git_bare(&host, &["rev-parse", name]))
        .collect();

    // A marker is never deleted on its own, only by its compaction.
    for name in names {
        let out = test_box
            .git(&repo)
            .args(["push", "origin", &format!(":{name}")])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{name} was deleted");
        assert!(
            stderr.contains("except by its compaction"),
            "{name}: {stderr}"
        );
    }

    an_uncut_marker_is_not_compacted(&test_box, &host, &repo, &keys, &marker, &targets[0]);

    // A pass cuts the landed chain; the bay refs are over the budget, so it compacts.
    let executor = test_box.clone_of(&host, "executor");
    test_box.use_keys(&executor, &keys);
    let mut compacted = serde_json::Value::Null;
    for _ in 0..2 {
        let ran = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
        if let Some(entry) = ran["detail"]["ran"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["compacted"].is_array())
        {
            compacted = entry["compacted"].clone();
            break;
        }
    }
    for (name, target) in names.iter().zip(&targets) {
        assert!(
            compacted
                .as_array()
                .is_some_and(|list| list.iter().any(|entry| entry == *name)),
            "{name} was compacted: {compacted}"
        );
        assert_eq!(
            test_box.git_bare(&host, &["for-each-ref", "--format=%(refname)", name]),
            "",
            "{name} is gone from the host"
        );
        let (shard, path) = index_entry(name);
        assert_eq!(
            test_box.git_bare(&host, &["cat-file", "blob", &format!("{shard}:{path}")]),
            *target,
            "{shard} records {name}"
        );
    }

    // A re-create of each compacted name is refused at the host, by the shard index.
    for (name, target) in names.iter().zip(&targets) {
        let out = test_box
            .git(&repo)
            .args(["push", "origin", &format!("{target}:{name}")])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{name} was created again");
        let (shard, _) = index_entry(name);
        assert!(
            stderr.contains("is compacted") && stderr.contains(&shard),
            "{name}: {stderr}"
        );
    }

    compacted_markers_still_answer(&test_box, &host, &keys, &id, &intent);
    shard_guards_refuse(&test_box, &host, &repo, &keys, &marker, &targets);
    one_push_moves_a_bounded_number_of_markers(&test_box, &host, &targets[0]);
}

/// An executor-signed compaction of `marker` (at `target`) while retention has not cut its
/// chain is refused at the host: only a landed, cut change gives up its markers (§ 10a).
fn an_uncut_marker_is_not_compacted(
    test_box: &TestBox,
    host: &Path,
    repo: &Path,
    keys: &support::TestKeys,
    marker: &str,
    target: &str,
) {
    let (shard, path) = index_entry(marker);
    let tip = ref_at(test_box, host, &shard);
    let early = forge_shard(
        test_box,
        repo,
        Some(&keys.executor),
        tip.as_deref(),
        tip.as_deref(),
        &[(path, target.to_owned())],
    );
    let (pushed, stderr) = push(
        test_box,
        repo,
        &[
            "--atomic",
            &format!("{early}:{shard}"),
            &format!(":{marker}"),
        ],
    );
    assert!(!pushed, "a marker of an uncut change was compacted");
    assert!(
        stderr.contains("has not landed with its chain cut by retention"),
        "{stderr}"
    );
}

/// The host's shard guards, each refused for its own reason. The shard index of the landed
/// `marker` holds its record now; `aaaaaaaa` is a name no push deletes.
fn shard_guards_refuse(
    test_box: &TestBox,
    host: &Path,
    repo: &Path,
    keys: &support::TestKeys,
    marker: &str,
    targets: &[String],
) {
    test_box.git_ok(
        repo,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/bay/landed-index/*:refs/bay/landed-index/*",
        ],
    );
    let (shard, marker_path) = index_entry(marker);
    let tip = ref_at(test_box, host, &shard).expect("the shard index exists");
    let fake = "refs/bay/landed/aaaaaaaa";
    let (fake_shard, fake_path) = index_entry(fake);
    let fake_tip = ref_at(test_box, host, &fake_shard);
    let refused = |forged: &str, onto: &str, why: &str| {
        let (pushed, stderr) = push(test_box, repo, &[&format!("+{forged}:{onto}")]);
        assert!(!pushed, "{onto} took {forged}, which {why} refuses");
        assert!(stderr.contains(why), "{why}: {stderr}");
    };
    // A record of a name the push does not delete: a record only stands in for a ref it
    // replaces.
    let forged = forge_shard(
        test_box,
        repo,
        Some(&keys.executor),
        fake_tip.as_deref(),
        fake_tip.as_deref(),
        &[(fake_path, targets[0].clone())],
    );
    refused(&forged, &fake_shard, "does not delete");
    // A rewrite that is not a fast-forward.
    let forged = forge_shard(test_box, repo, Some(&keys.executor), None, None, &[]);
    refused(&forged, &shard, "takes only a fast-forward");
    // An append that no executor key signed.
    let forged = forge_shard(test_box, repo, None, Some(&tip), Some(&tip), &[]);
    refused(&forged, &shard, "signed by an executor key");
    // An append that changes a record it already holds.
    let forged = forge_shard(
        test_box,
        repo,
        Some(&keys.executor),
        Some(&tip),
        Some(&tip),
        &[(marker_path, targets[1].clone())],
    );
    refused(&forged, &shard, "only appends records");
    // A record placed in a shard that is not its own.
    let stranger = ["bbbbbbbb", "cccccccc", "dddddddd", "eeeeeeee"]
        .iter()
        .map(|id| format!("refs/bay/landed/{id}"))
        .find(|name| index_entry(name).0 != shard)
        .unwrap();
    let forged = forge_shard(
        test_box,
        repo,
        Some(&keys.executor),
        Some(&tip),
        Some(&tip),
        &[(index_entry(&stranger).1, targets[0].clone())],
    );
    refused(&forged, &shard, "is not a compacted name of shard");
    // The shard index itself is never deleted.
    let (pushed, stderr) = push(test_box, repo, &[&format!(":{shard}")]);
    assert!(!pushed, "{shard} was deleted");
    assert!(stderr.contains("is never deleted"), "{stderr}");
}

/// One push moves at most `PUSH_MAX` (4096) markers, judged before any of them is read.
fn one_push_moves_a_bounded_number_of_markers(test_box: &TestBox, host: &Path, target: &str) {
    let zero = "0".repeat(target.len());
    let mut input = String::new();
    // Valid change ids (8 characters of lower-case base32), so only the bound refuses them.
    let alphabet = b"abcdefghijklmnopqrstuvwxyz234567";
    for n in 0..4097_usize {
        let id: String = (0..8)
            .map(|place| char::from(alphabet[(n >> (5 * place)) & 31]))
            .collect();
        std::fmt::Write::write_fmt(
            &mut input,
            format_args!("{target} {zero} refs/bay/landed/{id}\n"),
        )
        .unwrap();
    }
    let judged = test_box
        .bay_process(host)
        .env("GIT_DIR", host)
        .args(["host", "hook", "pre-receive"])
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            std::io::Write::write_all(&mut child.stdin.take().unwrap(), input.as_bytes())?;
            child.wait_with_output()
        })
        .unwrap();
    let stderr = String::from_utf8_lossy(&judged.stderr);
    assert!(
        !judged.status.success(),
        "4097 markers were moved in one push"
    );
    assert!(stderr.contains("at most 4096 markers"), "{stderr}");
}

/// After compaction, `start` and `undo` still find the landing of change `id`, and the host
/// still knows the generation of `intent` landed.
fn compacted_markers_still_answer(
    test_box: &TestBox,
    host: &Path,
    keys: &support::TestKeys,
    id: &str,
    intent: &str,
) {
    // A start on the compacted intent still answers CHANGE_ALREADY_LANDED.
    let fresh = test_box.clone_of(host, "fresh");
    test_box.use_keys(&fresh, keys);
    let (code, again) =
        result_json(
            test_box
                .bay(&fresh)
                .args(["start", "COMPACT-1", "--paths", "src/"]),
        );
    assert_eq!(code, 10, "{again}");
    assert_eq!(again["reason"], "CHANGE_ALREADY_LANDED", "{again}");
    assert_eq!(again["change"], id, "{again}");

    // The landing is still found: undo opens its compensating change.
    let (code, undone) = result_json(test_box.bay(&fresh).args(["undo", id]));
    assert_eq!(code, 0, "{undone}");
    assert_ne!(undone["reason"], "CHANGE_NOT_LANDED", "{undone}");

    // The host still knows generation 1 landed: `start --new` claims generation 2.
    let (code, next) =
        result_json(
            test_box
                .bay(&fresh)
                .args(["start", "COMPACT-1", "--new", "--paths", "src/"]),
        );
    assert_eq!(code, 0, "{next}");
    let claim = next["detail"]["intent"].as_str().unwrap_or_default();
    assert_eq!(claim, intent.replace("/1", "/2"), "{next}");
}

/// The target of `name` on the bare repository `bare`, if it exists.
fn ref_at(test_box: &TestBox, bare: &Path, name: &str) -> Option<String> {
    let listed = test_box.git_bare(bare, &["for-each-ref", "--format=%(objectname)", name]);
    (!listed.is_empty()).then_some(listed)
}

/// A push from `repo` to origin: whether it was taken, and its stderr.
fn push(test_box: &TestBox, repo: &Path, args: &[&str]) -> (bool, String) {
    let out = test_box
        .git(repo)
        .args(["push", "origin"])
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A shard index commit forged in `repo`: the tree of `base` (or an empty one) with each
/// `(path, target)` record set, on `parent`, signed by `key` when one is given.
fn forge_shard(
    test_box: &TestBox,
    repo: &Path,
    key: Option<&Path>,
    base: Option<&str>,
    parent: Option<&str>,
    records: &[(String, String)],
) -> String {
    let index = test_box.path().join("forged-index");
    let _ = std::fs::remove_file(&index);
    let with_index = |args: &[&str]| {
        let out = test_box
            .git(repo)
            .env("GIT_INDEX_FILE", &index)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    if let Some(base) = base {
        with_index(&["read-tree", base]);
    }
    let record = test_box.path().join("forged-record");
    for (path, target) in records {
        std::fs::write(&record, format!("{target}\n")).unwrap();
        let blob = with_index(&["hash-object", "-w", record.to_str().unwrap()]);
        with_index(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},{path}"),
        ]);
    }
    let tree = with_index(&["write-tree"]);
    let signing_key = key.map(|key| format!("user.signingkey={}", key.display()));
    let mut forge = vec!["-c", "gpg.format=ssh"];
    if let Some(signing_key) = &signing_key {
        forge.extend(["-c", signing_key.as_str()]);
    }
    forge.extend(["commit-tree", tree.as_str(), "-m", "forged"]);
    forge.push(if key.is_some() { "-S" } else { "--no-gpg-sign" });
    if let Some(parent) = parent {
        forge.extend(["-p", parent]);
    }
    let forged = test_box
        .git(repo)
        .env("GIT_COMMITTER_NAME", support::EXECUTOR)
        .env("GIT_COMMITTER_EMAIL", "exec@bay.invalid")
        .env("GIT_AUTHOR_NAME", support::EXECUTOR)
        .env("GIT_AUTHOR_EMAIL", "exec@bay.invalid")
        .args(&forge)
        .output()
        .unwrap();
    assert!(forged.status.success(), "{forged:?}");
    String::from_utf8(forged.stdout).unwrap().trim().to_owned()
}

/// § 6 and § 10a: a landed-intent marker names the landing of a change that held that claim.
/// A landing that also creates the landed claim of another, open change is refused, so no
/// executor can mark a generation landed that never landed and then compact it away.
#[test]
fn a_landed_claim_is_created_only_by_its_own_landing() {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);

    let open = test_box.bay_ok(&repo, &["start", "STRAY-A", "--paths", "src/"]);
    let open_claim = open["detail"]["intent"].as_str().unwrap().to_owned();
    let stray = open_claim.replace("refs/bay/intent/", "refs/bay/landed-intent/");
    let landing = test_box.bay_ok(&repo, &["start", "STRAY-C", "--paths", "src/"]);
    let view = std::path::PathBuf::from(landing["detail"]["view"].as_str().unwrap());

    // The landing runs against a copy of the host without its hook, so its pushes can be
    // replayed onto the host with one ref more.
    let copy = test_box.path().join("copy.git");
    test_box.git_ok(
        test_box.path(),
        &[
            "clone",
            "-q",
            "--mirror",
            host.to_str().unwrap(),
            copy.to_str().unwrap(),
        ],
    );
    test_box.git_ok(
        &repo,
        &["remote", "set-url", "origin", copy.to_str().unwrap()],
    );
    std::fs::write(view.join("src/c.rs"), "fn c() {}\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    test_box.git_ok(
        &repo,
        &["remote", "set-url", "origin", host.to_str().unwrap()],
    );

    let listed = |bare: &Path| -> std::collections::BTreeMap<String, String> {
        test_box
            .git_bare(bare, &["for-each-ref", "--format=%(refname) %(objectname)"])
            .lines()
            .filter_map(|line| line.split_once(' '))
            .map(|(name, oid)| (name.to_owned(), oid.to_owned()))
            .collect()
    };
    let (before, after) = (listed(&host), listed(&copy));
    let publication_ref = |name: &str| {
        name == "refs/heads/main"
            || name.starts_with("refs/bay/landed/")
            || name.starts_with("refs/bay/landed-intent/")
            || name.starts_with("refs/bay/queue-done/")
    };
    let moved: Vec<(&String, &String)> = after
        .iter()
        .filter(|(name, oid)| before.get(*name) != Some(*oid))
        .collect();
    let replay = |names: &[String]| {
        let mut args = vec![
            "--git-dir".to_owned(),
            copy.display().to_string(),
            "push".to_owned(),
            "-q".to_owned(),
            "--atomic".to_owned(),
            host.display().to_string(),
        ];
        args.extend(names.iter().cloned());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = test_box.git(test_box.path()).args(&args).output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let first: Vec<String> = moved
        .iter()
        .filter(|(name, _)| !publication_ref(name))
        .map(|(name, _)| format!("{name}:{name}"))
        .collect();
    let (pushed, stderr) = replay(&first);
    assert!(pushed, "the landing's own refs: {stderr}");
    let mut publication: Vec<String> = moved
        .iter()
        .filter(|(name, _)| publication_ref(name))
        .map(|(name, _)| format!("{name}:{name}"))
        .collect();
    let commit = after["refs/heads/main"].clone();
    let mut with_stray = publication.clone();
    with_stray.push(format!("{commit}:{stray}"));
    let (pushed, stderr) = replay(&with_stray);
    assert!(!pushed, "{stray} was created by another change's landing");
    assert!(
        stderr.contains("only by the landing of a change that holds"),
        "{stderr}"
    );
    // The same landing without the stray landed claim is taken: the refusal was for it alone.
    let (pushed, stderr) = replay(&std::mem::take(&mut publication));
    assert!(pushed, "the landing itself: {stderr}");
    assert_eq!(ref_at(&test_box, &host, &stray), None);
}
