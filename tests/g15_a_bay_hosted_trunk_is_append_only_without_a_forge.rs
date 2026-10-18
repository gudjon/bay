//! G15. A bay-hosted trunk is append-only without a forge.
//!
//! On a `bay host init` repository a force push, a deletion and a recreation of trunk are
//! refused by the trunk-scoped hook; bay's own refs (`refs/bay/*`, `refs/notes/bay/*`) can
//! still be pruned by a retention cut; `bay doctor` refuses a host without the trunk hook.

mod support;

use support::{TestBox, check_files, passing_result, result_json, row};

fn push_fails(test_box: &TestBox, repo: &std::path::Path, args: &[&str], why: &str) {
    let out = test_box.git(repo).arg("push").args(args).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "push {args:?} was accepted");
    assert!(stderr.contains(why), "push {args:?}: {stderr}");
}

#[test]
fn the_trunk_hook_refuses_rewrites_and_allows_retention_cuts() {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    let created = test_box.host_init(&host);
    assert_eq!(created["detail"]["ref format"], "reftable");
    assert_eq!(
        test_box.git_bare(&host, &["rev-parse", "--show-ref-format"]),
        "reftable"
    );

    let repo = test_box.repo_without_remote_any("repo");
    test_box.git_ok(&repo, &["remote", "add", "origin", host.to_str().unwrap()]);
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );

    // A force push and a deletion of trunk are refused.
    test_box.git_ok(&repo, &["commit", "-q", "--amend", "-m", "rewritten"]);
    push_fails(
        &test_box,
        &repo,
        &["-f", "origin", "HEAD:main"],
        "non-fast-forward update is refused",
    );
    // Git itself refuses deleting the host's current branch; switch that off to show the hook
    // refuses on its own.
    test_box.git_bare(&host, &["config", "receive.denyDeleteCurrent", "ignore"]);
    push_fails(
        &test_box,
        &repo,
        &["origin", ":main"],
        "deleting it is refused",
    );
    test_box.git_ok(&repo, &["reset", "-q", "--hard", "origin/main"]);

    // A landing through bay is an ordinary fast-forward and is accepted.
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");

    // Trunk that held landings cannot be recreated, even after someone removed it on the host.
    test_box.git_bare(&host, &["update-ref", "-d", "refs/heads/main"]);
    test_box.git_ok(&repo, &["fetch", "-q", "origin"]);
    push_fails(
        &test_box,
        &repo,
        &[
            "origin",
            &format!(
                "{}:refs/heads/main",
                landed["detail"]["commit"].as_str().unwrap()
            ),
        ],
        "recreating it is refused",
    );

    // bay's own refs stay prunable: a non-fast-forward cut of a chain and a notes deletion.
    let chain = format!("refs/bay/changes/{id}");
    let older = test_box.git_ok(&repo, &["rev-parse", &format!("{chain}~1")]);
    test_box.git_ok(
        &repo,
        &["push", "-q", "-f", "origin", &format!("{older}:{chain}")],
    );
    assert_eq!(test_box.git_bare(&host, &["rev-parse", &chain]), older);
    test_box.git_ok(
        &repo,
        &["push", "-q", "origin", ":refs/notes/bay/evidence/local"],
    );
}

#[test]
fn doctor_refuses_a_host_without_the_trunk_hook() {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let (code, result) = result_json(test_box.bay(&host).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    assert_eq!(row(&result, "trunk-hook")["state"], "green");

    let plain = test_box.bare("plain.git", "sha1", "reftable");
    let (code, result) = result_json(test_box.bay(&plain).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(row(&result, "trunk-hook")["state"], "red");
}

#[test]
fn host_init_below_2_54_is_refused_and_creates_nothing() {
    // Git runs a hook defined in configuration only from 2.54. Below it the trunk hook is
    // silently ignored and a force push is accepted, so Host init not found: 2(SERVFAIL) must refuse to create
    // a host whose trunk would not be append-only.
    let test_box = TestBox::new();
    test_box.fake_git_version("2.53.0");
    let host = test_box.path().join("host.git");
    let (code, result) =
        result_json(
            test_box
                .bay(test_box.path())
                .args(["host", "init", host.to_str().unwrap()]),
        );
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["decision"], "blocked");
    assert_eq!(result["reason"], "GIT_BELOW_FLOOR");
    assert_eq!(result["detail"]["role"], "bay-hosted-trunk");
    assert_eq!(result["detail"]["version"], "2.53.0");
    assert_eq!(result["detail"]["floor"], "2.54");
    assert!(
        !host.exists(),
        "a refused host init created {}",
        host.display()
    );
}
