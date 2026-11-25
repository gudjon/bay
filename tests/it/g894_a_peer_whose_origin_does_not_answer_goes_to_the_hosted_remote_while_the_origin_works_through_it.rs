//! G894. A peer whose origin does not answer goes to the hosted remote while the origin works
//! through it, and waits for an origin that tails (BACKLOG 290).
//!
//! In `through` mode the hosted remote holds every ref the origin holds: the origin takes a push
//! only after the hosted remote took it. So a call whose origin never answered (ssh found no
//! listener) goes to the hosted remote, and the store's calls go there for a while. An origin
//! that tails says so on the hosted remote first (`refs/bay-origin/tails`): it may hold what the
//! hosted remote lacks, so a peer waits for it.

// Each case runs shell scripts (the check adapter and an ssh), so these run on Unix only.
#![cfg(unix)]

use std::path::{Path, PathBuf};

use crate::support;

use support::{TestBox, check_files, passing_result};

/// The origin a peer names: no one answers there.
const ORIGIN: &str = "ssh://origin.invalid/srv/origin.git";

/// The value of `name` in the bare repository `bare`, `None` when it has none.
fn value(test_box: &TestBox, bare: &Path, name: &str) -> Option<String> {
    let out = test_box
        .git(test_box.path())
        .arg(format!("--git-dir={}", bare.display()))
        .args(["rev-parse", "--verify", "-q", name])
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8(out.stdout).unwrap().trim().to_owned())
}

/// A hosted remote with a checked trunk and a repository that reads it: (hosted, repository).
fn fleet(test_box: &TestBox) -> (PathBuf, PathBuf) {
    let hosted = test_box.bare_any("hosted.git");
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &hosted);
    let files = check_files("true", &passing_result(1), 1, 60);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    (hosted, repo)
}

/// Names [`ORIGIN`] as the store's origin, reached by an ssh that finds no listener there.
fn origin_down(test_box: &TestBox, repo: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let ssh = test_box.path().join("dead-ssh");
    std::fs::write(
        &ssh,
        "#!/bin/sh\necho 'ssh: connect to host origin.invalid port 22: Connection refused' >&2\nexit 255\n",
    )
    .unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    test_box.git_ok(repo, &["config", "core.sshCommand", ssh.to_str().unwrap()]);
    test_box.git_ok(repo, &["config", "bay.origin", ORIGIN]);
}

/// `bay merge --wait` in `view`: its exit code, its result and its stderr.
fn merge(test_box: &TestBox, view: &Path) -> (Option<i32>, serde_json::Value, String) {
    let out = test_box
        .bay(view)
        .args(["merge", "--wait", "--json"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let result = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|error| panic!("not JSON ({error}); stderr: {stderr}"));
    (out.status.code(), result, stderr)
}

#[test]
fn a_change_lands_on_the_hosted_remote_while_its_origin_does_not_answer() {
    let test_box = TestBox::new();
    let (hosted, repo) = fleet(&test_box);
    origin_down(&test_box, &repo);

    // The start writes the change's record past the origin, and so does the landing.
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed, stderr) = merge(&test_box, &view);
    assert_eq!(code, Some(0), "{landed}\n{stderr}");
    let commit = landed["detail"]["commit"].as_str().unwrap().to_owned();
    assert_eq!(value(&test_box, &hosted, "refs/heads/main"), Some(commit));
    assert!(
        value(&test_box, &hosted, &format!("refs/bay/changes/{id}")).is_some(),
        "the change's record is on the hosted remote"
    );
    // The process says once where its calls went, and why.
    assert!(
        stderr.contains(&format!("ORIGIN_UNREACHABLE {ORIGIN}")),
        "{stderr}"
    );
    assert!(stderr.contains("Connection refused"), "{stderr}");
    assert_eq!(stderr.matches("ORIGIN_UNREACHABLE").count(), 1, "{stderr}");
}

#[test]
fn a_change_waits_for_an_origin_that_tails() {
    let test_box = TestBox::new();
    let (hosted, repo) = fleet(&test_box);
    let (_, view) = test_box.start_and_edit(&repo);
    let trunk = value(&test_box, &hosted, "refs/heads/main").unwrap();

    // The origin said on the hosted remote that it tails, then stopped answering.
    test_box.git_ok(
        &repo,
        &[
            "push",
            "-q",
            hosted.to_str().unwrap(),
            &format!("{trunk}:refs/bay-origin/tails"),
        ],
    );
    origin_down(&test_box, &repo);

    // The change is kept here until main can be reached, and the answer says why.
    let (code, queued, stderr) = merge(&test_box, &view);
    assert_eq!(code, Some(0), "{queued}\n{stderr}");
    assert_eq!(queued["decision"], "queued", "{queued}");
    assert_eq!(queued["waiting_for"], "remote", "{queued}");
    assert!(
        stderr.contains(&format!(
            "ORIGIN_UNREACHABLE {ORIGIN}: the origin tails, so bay waits for it"
        )),
        "{stderr}"
    );
    assert_eq!(value(&test_box, &hosted, "refs/heads/main"), Some(trunk));
}
