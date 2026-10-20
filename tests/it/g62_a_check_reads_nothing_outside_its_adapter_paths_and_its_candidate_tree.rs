//! G62. A check reads nothing outside its adapter paths and its candidate tree.
//!
//! The check sandbox is an allow-list: the check sees its candidate tree and its adapter, the
//! system files a program needs to start, a private `/tmp`, `/proc` and `/dev`, and nothing
//! else. A file in the executor's `HOME`, in a temporary directory outside the repository, the
//! box's global Git config and the repository's own `.git/config` are all absent, while the
//! candidate's files are there. The network is denied unless the check's `[[check]]` entry says
//! `network = true`; that entry is judged at the base, so a candidate that adds it to its own
//! policy does not get the network (the G26 property).
//!
//! Each case needs the check sandbox. On a machine without one it is skipped, unless
//! `BAY_REQUIRE_SANDBOX` is set, which the gate sets where `bwrap` is present: then it fails.

#![cfg(unix)]

use crate::support;

use std::net::TcpListener;
use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

/// Every evidence record in `repo`'s notes, as one text.
fn records(test_box: &TestBox, repo: &Path) -> String {
    let notes = test_box.git_ok(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/notes/bay/evidence/",
        ],
    );
    notes
        .lines()
        .flat_map(|notes_ref| {
            test_box
                .git_ok(
                    repo,
                    &["ls-tree", "-r", "--format=%(objectname)", notes_ref],
                )
                .lines()
                .map(|blob| test_box.git_ok(repo, &["cat-file", "blob", blob]))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// A trunk whose one check runs `before`, then prints `result`, and a change on `src/`.
/// `extra` is appended to the check's `[[check]]` entry. Returns (box, repo, remote, view).
fn submitted(before: &str, result: &str, extra: &str) -> (TestBox, PathBuf, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files(before, result, 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(extra);
        }
    }
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (_, view) = test_box.start_and_edit(&repo);
    (test_box, repo, remote, view)
}

fn land(test_box: &TestBox, view: &Path) -> (i32, serde_json::Value) {
    result_json(test_box.bay(view).args(["land", "--wait"]))
}

/// The record of the check names the sandbox it ran in.
fn assert_ran_in(test_box: &TestBox, repo: &Path, sandbox: &str, landed: &serde_json::Value) {
    let records = records(test_box, repo);
    assert!(
        records.contains(&format!("\"sandbox\": \"{sandbox}\"")),
        "no evidence record from a {sandbox} run: {landed}\n{records}"
    );
}

/// A shell line that exits 3, naming the path, when the check can see any of `paths`.
fn sees_none_of(paths: &[&Path]) -> String {
    let quoted: Vec<String> = paths
        .iter()
        .map(|path| format!("'{}'", path.display()))
        .collect();
    format!(
        "for f in {}; do if [ -e \"$f\" ] || cat \"$f\" >/dev/null 2>&1; then echo \"the check saw $f\" >&2; exit 3; fi; done",
        quoted.join(" ")
    )
}

#[test]
fn a_check_cannot_see_home_a_temporary_file_or_the_repository() {
    let Some(sandbox) = support::sandbox() else {
        eprintln!("no check sandbox on this machine; G62 cannot run");
        return;
    };
    let test_box = TestBox::new();
    // Files no deny-list names: one in the executor's HOME, one in a temporary directory
    // outside the repository. The box's global Git config and the repository's own
    // `.git/config` must be absent too.
    let in_home = test_box.home.join("secret.txt");
    std::fs::write(&in_home, "home secret\n").unwrap();
    let outside = test_box.path().join("outside").join("secret.txt");
    std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
    std::fs::write(&outside, "outside secret\n").unwrap();
    let repo = test_box.repo_any("repo");
    let repo_config = repo.join(".git").join("config");
    assert!(repo_config.is_file(), "{}", repo_config.display());
    let global = test_box.global_config.clone();
    let before = sees_none_of(&[&in_home, &outside, &global, &repo_config, &test_box.home]);
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files(&before, &passing_result(1), 1, 60),
    );
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, landed) = land(&test_box, &view);
    assert_eq!(
        code, 0,
        "the check saw a file outside its allow-list: {landed}"
    );
    assert_eq!(landed["decision"], "landed", "{landed}");
    assert_eq!(test_box.trunk_change_ids(&remote).len(), 1);
    assert_ran_in(&test_box, &repo, sandbox, &landed);
}

#[test]
fn a_check_reads_its_candidate_and_its_adapter() {
    let Some(sandbox) = support::sandbox() else {
        eprintln!("no check sandbox on this machine; G62 cannot run");
        return;
    };
    let before = "[ \"$(cat src/a.rs)\" = 'fn a() {}' ] || { echo no src/a.rs >&2; exit 3; }\n\
                  [ \"$(cat src/new.rs)\" = 'fn new() {}' ] || { echo no src/new.rs >&2; exit 3; }\n\
                  cat \"$BAY_ADAPTER_DIR/scripts/bay-check\" >/dev/null || exit 3\n\
                  echo scratch > \"$TMPDIR/written\" && echo own > \"$HOME/written\" || exit 3";
    let (test_box, repo, remote, view) = submitted(before, &passing_result(1), "");
    let (code, landed) = land(&test_box, &view);
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "landed", "{landed}");
    assert_eq!(test_box.trunk_change_ids(&remote).len(), 1);
    assert_ran_in(&test_box, &repo, sandbox, &landed);
}

/// The adapter of the network cases: it connects to `port` on 127.0.0.1 and passes when that
/// works, and reports a product failure when it does not.
fn connects_to(port: u16) -> (String, String) {
    let before = format!(
        "bash -c true || {{ echo no bash >&2; exit 4; }}\n\
         if bash -c 'exec 3<>/dev/tcp/127.0.0.1/{port}' 2>/dev/null; then net=yes; else net=no; fi"
    );
    let pass = passing_result(1);
    let fail = pass.replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let result = format!("if [ \"$net\" = yes ]; then {pass}; else {fail}; fi");
    (before, result)
}

#[test]
fn a_check_has_no_network_unless_its_entry_says_network_true() {
    let Some(sandbox) = support::sandbox() else {
        eprintln!("no check sandbox on this machine; G62 cannot run");
        return;
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let (before, result) = connects_to(listener.local_addr().unwrap().port());

    // No `network` setting: the check runs in a network namespace of its own.
    let (test_box, repo, remote, view) = submitted(&before, &result, "");
    let (code, blocked) = land(&test_box, &view);
    assert_eq!(code, 10, "the check reached the network: {blocked}");
    assert_eq!(blocked["reason"], "CHECK_FAILED", "{blocked}");
    assert_eq!(test_box.trunk_change_ids(&remote), Vec::<String>::new());
    assert_ran_in(&test_box, &repo, sandbox, &blocked);

    // `network = true` in the entry at the base: the same check connects and passes.
    let (test_box, repo, remote, view) = submitted(&before, &result, "network = true\n");
    let (code, landed) = land(&test_box, &view);
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "landed", "{landed}");
    assert_eq!(test_box.trunk_change_ids(&remote).len(), 1);
    assert_ran_in(&test_box, &repo, sandbox, &landed);
    drop(listener);
}

#[test]
fn a_candidate_cannot_grant_itself_the_network() {
    let Some(_sandbox) = support::sandbox() else {
        eprintln!("no check sandbox on this machine; G62 cannot run");
        return;
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let (before, result) = connects_to(listener.local_addr().unwrap().port());
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let files = check_files(&before, &result, 1, 60);
    let policy = files
        .iter()
        .find(|(path, _)| path == ".bay/policy.toml")
        .unwrap()
        .1
        .clone();
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let started = test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "src/", ".bay/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    // The candidate raises the revision and grants its check the network: the base's policy,
    // which does not, still judges it.
    std::fs::write(
        view.join(".bay/policy.toml"),
        format!(
            "{}network = true\n",
            policy.replacen("revision = 1", "revision = 2", 1)
        ),
    )
    .unwrap();
    let (code, blocked) = land(&test_box, &view);
    assert_eq!(
        code, 10,
        "the candidate's own policy granted the network: {blocked}"
    );
    assert_eq!(blocked["reason"], "CHECK_FAILED", "{blocked}");
    assert_eq!(test_box.trunk_change_ids(&remote), Vec::<String>::new());
    drop(listener);
}
