//! G879. An origin sends its hosted remote every push it takes.
//!
//! A bay origin (`bay host <path> --upstream <url>`) serves a fleet's bay traffic near the fleet
//! while the hosted remote stays its upstream. In `through` mode the origin takes a push only
//! after the hosted remote took it: a ref that moved there refuses the push as stale and the
//! origin takes the hosted remote's value, and `bay host serve` takes what other writers push to
//! the hosted remote. In `tail` mode bay's refs go to the hosted remote after the origin takes
//! them, main still goes first, and a ref another writer moved there is named a STRAY and
//! written over. Once an origin tails it stays tailing.

// Each case runs shell scripts (the check adapter), so these run on Unix only.
#![cfg(unix)]

use std::path::{Path, PathBuf};

use crate::support;

use support::{TestBox, check_files, formats, passing_result, result_json};

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

fn serve_once(test_box: &TestBox, origin: &Path) -> serde_json::Value {
    test_box.bay_ok(
        test_box.path(),
        &["host", "serve", origin.to_str().unwrap(), "--once"],
    )
}

/// A hosted remote with a checked trunk, its origin holding what it holds, and a repository whose
/// config sends bay's traffic to the origin: (hosted, origin, repository).
fn fleet(test_box: &TestBox) -> (PathBuf, PathBuf, PathBuf) {
    let hosted = test_box.bare_any("hosted.git");
    let origin = test_box.path().join("origin.git");
    let (object_format, _) = formats();
    let made = test_box.bay_ok(
        test_box.path(),
        &[
            "host",
            origin.to_str().unwrap(),
            "--object-format",
            &object_format,
            "--upstream",
            hosted.to_str().unwrap(),
        ],
    );
    assert_eq!(made["detail"]["mode"], "through", "{made}");
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &hosted);
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        format!("[origin]\nurl = \"{}\"\n", origin.display()),
    ));
    let trunk = test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let served = serve_once(test_box, &origin);
    assert_eq!(served["detail"]["mirrored"]["taken"], 1, "{served}");
    assert_eq!(value(test_box, &origin, "refs/heads/main"), Some(trunk));
    (hosted, origin, repo)
}

/// A new commit on `base` in `repo` that no branch holds.
fn commit_on(test_box: &TestBox, repo: &Path, base: &str, message: &str) -> String {
    let tree = test_box.git_ok(repo, &["rev-parse", &format!("{base}^{{tree}}")]);
    test_box.git_ok(repo, &["commit-tree", &tree, "-p", base, "-m", message])
}

fn forwarding_marks(origin: &Path) -> usize {
    std::fs::read_dir(origin.join("bay/forwarding")).map_or(0, Iterator::count)
}

#[test]
fn a_landing_through_an_origin_reaches_the_hosted_remote_in_the_same_push() {
    let test_box = TestBox::new();
    let (hosted, origin, repo) = fleet(&test_box);

    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let commit = landed["detail"]["commit"].as_str().unwrap().to_owned();

    // The origin holds the landing and the change's record, and so does the hosted remote:
    // nothing but the origin's hook wrote to it.
    assert_eq!(
        value(&test_box, &origin, "refs/heads/main").as_deref(),
        Some(commit.as_str())
    );
    assert_eq!(
        value(&test_box, &hosted, "refs/heads/main").as_deref(),
        Some(commit.as_str())
    );
    let record = format!("refs/bay/changes/{id}");
    assert!(value(&test_box, &origin, &record).is_some());
    assert_eq!(
        value(&test_box, &hosted, &record),
        value(&test_box, &origin, &record)
    );
    // Each forward's mark ended when the origin took its push.
    assert_eq!(forwarding_marks(&origin), 0);
}

#[test]
fn a_ref_moved_on_the_hosted_remote_refuses_the_push_as_stale_and_the_origin_takes_it() {
    let test_box = TestBox::new();
    let (hosted, origin, repo) = fleet(&test_box);
    let trunk = value(&test_box, &origin, "refs/heads/main").unwrap();

    // Another writer moves main on the hosted remote.
    let moved = commit_on(&test_box, &repo, &trunk, "elsewhere");
    test_box.git_ok(
        &repo,
        &[
            "push",
            "-q",
            hosted.to_str().unwrap(),
            &format!("{moved}:refs/heads/main"),
        ],
    );

    // A push to the origin that read main before is refused as stale, and the hosted remote
    // keeps the other writer's main.
    let mine = commit_on(&test_box, &repo, &trunk, "mine");
    let out = test_box
        .git(&repo)
        .args([
            "push",
            origin.to_str().unwrap(),
            &format!("{mine}:refs/heads/main"),
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "the stale push was taken");
    assert!(stderr.contains("stale info"), "{stderr}");
    assert_eq!(
        value(&test_box, &hosted, "refs/heads/main"),
        Some(moved.clone())
    );
    // The origin took the hosted remote's main, so the next read sees it.
    assert_eq!(value(&test_box, &origin, "refs/heads/main"), Some(moved));
    assert_eq!(forwarding_marks(&origin), 0);
}

#[test]
fn a_push_the_origin_refuses_never_reaches_the_hosted_remote() {
    let test_box = TestBox::new();
    let (hosted, origin, repo) = fleet(&test_box);
    let trunk = value(&test_box, &origin, "refs/heads/main").unwrap();

    // A rewrite of main: the origin's trunk hook refuses it before the hosted remote sees it.
    let tree = test_box.git_ok(&repo, &["rev-parse", &format!("{trunk}^{{tree}}")]);
    let rewrite = test_box.git_ok(&repo, &["commit-tree", &tree, "-m", "rewritten"]);
    let out = test_box
        .git(&repo)
        .args([
            "push",
            origin.to_str().unwrap(),
            &format!("+{rewrite}:refs/heads/main"),
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "the rewrite was taken");
    assert!(
        stderr.contains("non-fast-forward update is refused"),
        "{stderr}"
    );
    assert_eq!(
        value(&test_box, &hosted, "refs/heads/main"),
        Some(trunk.clone())
    );
    assert_eq!(value(&test_box, &origin, "refs/heads/main"), Some(trunk));
}

#[test]
fn a_serve_pass_takes_what_another_writer_pushed_to_the_hosted_remote() {
    let test_box = TestBox::new();
    let (hosted, origin, repo) = fleet(&test_box);
    let trunk = value(&test_box, &origin, "refs/heads/main").unwrap();

    // A peer whose bay reads no origin writes a record and a branch to the hosted remote.
    let record = commit_on(&test_box, &repo, &trunk, "a record");
    test_box.git_ok(
        &repo,
        &[
            "push",
            "-q",
            hosted.to_str().unwrap(),
            &format!("{record}:refs/bay/test/peer"),
            &format!("{record}:refs/heads/topic"),
        ],
    );
    let served = serve_once(&test_box, &origin);
    let mirrored = &served["detail"]["mirrored"];
    assert_eq!(mirrored["taken"], 2, "{served}");
    // Only the record is bay's own: the stray a move to `tail` waits for none of.
    assert_eq!(mirrored["strays"], 1, "{served}");
    assert_eq!(
        value(&test_box, &origin, "refs/bay/test/peer"),
        Some(record.clone())
    );
    assert_eq!(value(&test_box, &origin, "refs/heads/topic"), Some(record));

    // A deletion on the hosted remote is taken too.
    test_box.git_ok(
        &repo,
        &[
            "push",
            "-q",
            hosted.to_str().unwrap(),
            ":refs/bay/test/peer",
        ],
    );
    let served = serve_once(&test_box, &origin);
    assert_eq!(served["detail"]["mirrored"]["deleted"], 1, "{served}");
    assert_eq!(value(&test_box, &origin, "refs/bay/test/peer"), None);
}

#[test]
fn a_tailing_origin_sends_bays_refs_after_it_takes_them_and_stays_tailing() {
    let test_box = TestBox::new();
    let (hosted, origin, repo) = fleet(&test_box);
    test_box.git_bare(&origin, &["config", "bay.upstreamMode", "tail"]);
    let trunk = value(&test_box, &origin, "refs/heads/main").unwrap();

    // A change's record goes to the origin alone; the tail sends it.
    let (id, view) = test_box.start_and_edit(&repo);
    let record = format!("refs/bay/changes/{id}");
    assert!(value(&test_box, &origin, &record).is_some());
    assert_eq!(value(&test_box, &hosted, &record), None);
    let served = serve_once(&test_box, &origin);
    assert_eq!(served["detail"]["mode"], "tail", "{served}");
    assert_eq!(
        value(&test_box, &hosted, &record),
        value(&test_box, &origin, &record)
    );

    // Main still goes to the hosted remote before the origin takes it.
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let commit = landed["detail"]["commit"].as_str().unwrap().to_owned();
    assert_eq!(value(&test_box, &hosted, "refs/heads/main"), Some(commit));

    // What is live on a machine stays on the origin.
    let live = commit_on(&test_box, &repo, &trunk, "live");
    test_box.git_ok(
        &repo,
        &[
            "push",
            "-q",
            origin.to_str().unwrap(),
            &format!("{live}:refs/bay/live/test"),
        ],
    );
    serve_once(&test_box, &origin);
    assert_eq!(value(&test_box, &hosted, "refs/bay/live/test"), None);

    // A ref another writer moved on the hosted remote is named a stray, then written over.
    let mine = commit_on(&test_box, &repo, &trunk, "mine");
    let theirs = commit_on(&test_box, &repo, &trunk, "theirs");
    let later = commit_on(&test_box, &repo, &mine, "later");
    let at = |commit: &str| format!("{commit}:refs/bay/test/x");
    test_box.git_ok(&repo, &["push", "-q", origin.to_str().unwrap(), &at(&mine)]);
    serve_once(&test_box, &origin);
    assert_eq!(value(&test_box, &hosted, "refs/bay/test/x"), Some(mine));
    test_box.git_ok(
        &repo,
        &["push", "-q", "-f", hosted.to_str().unwrap(), &at(&theirs)],
    );
    test_box.git_ok(
        &repo,
        &["push", "-q", origin.to_str().unwrap(), &at(&later)],
    );
    let served = serve_once(&test_box, &origin);
    assert_eq!(served["detail"]["tailed"]["strays"], 1, "{served}");
    assert_eq!(value(&test_box, &hosted, "refs/bay/test/x"), Some(theirs));
    serve_once(&test_box, &origin);
    assert_eq!(value(&test_box, &hosted, "refs/bay/test/x"), Some(later));

    // The config says through again, and the origin still tails: the hosted remote may lag it.
    test_box.git_bare(&origin, &["config", "bay.upstreamMode", "through"]);
    let served = serve_once(&test_box, &origin);
    assert_eq!(served["detail"]["mode"], "tail", "{served}");
    assert_eq!(served["detail"]["configured"], "through", "{served}");
}

#[test]
fn an_unknown_mode_is_refused_and_a_plain_host_is_no_origin() {
    let test_box = TestBox::new();
    let (_, origin, _) = fleet(&test_box);
    test_box.git_bare(&origin, &["config", "bay.upstreamMode", "mirror"]);
    let (code, served) = result_json(test_box.bay(test_box.path()).args([
        "host",
        "serve",
        origin.to_str().unwrap(),
        "--once",
    ]));
    assert_ne!(code, 0, "{served}");
    assert!(served.to_string().contains("through or tail"), "{served}");

    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let (code, served) = result_json(test_box.bay(test_box.path()).args([
        "host",
        "serve",
        host.to_str().unwrap(),
        "--once",
    ]));
    assert_ne!(code, 0, "{served}");
    assert!(served.to_string().contains("not an origin"), "{served}");
}
