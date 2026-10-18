//! G35. Publication never livelocks.
//!
//! After two `TRUNK_MOVED` refusals an operation joins the queue and lands in order; an
//! operation arriving while a queue is live holds a place without running a check
//! (`QUEUE_HELD`); a head whose check fails releases the queue at once; an executor killed
//! right after publishing leaves the queue free and the landed marker present (one atomic push);
//! a newer revision of a queued entry replaces it in place; a head released by its deadline
//! three times becomes `QUEUE_DEADLINE_EXCEEDED`; the hook refuses a reservation number that is
//! not above every existing one; a `HOOK_TIMEOUT` retries the same candidate, and five
//! uncounted retries (hook timeouts, a deadline release during a publish) end in
//! `PUBLISH_RETRY_EXHAUSTED`. The 20-agent load and worker outages and capabilities leave
//! `#[ignore]`; a policy landing during the head's check is G26's.

mod support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

/// A trunk whose check moves trunk (from another clone) on its first `moves` runs, then passes,
/// or fails from run `fail_from` on. Every run appends a line to `runs`.
struct Queued {
    test_box: TestBox,
    repo: PathBuf,
    remote: PathBuf,
    runs: PathBuf,
}

fn setup(moves: u32, fail_from: u32) -> Queued {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let runs = test_box.path().join("runs");
    let other = test_box.path().join("mover");
    let before = format!(
        "echo \"$tree\" >> '{runs}'; n=$(wc -l < '{runs}' | tr -d ' '); \
         if [ \"$n\" -le {moves} ]; then ( cd '{other}' && git pull -q --rebase && echo \"$n\" > docs/move-$n.md && git add docs && git -c user.name=t -c user.email=t@t commit -q -m \"move $n\" && git push -q origin main ); fi; \
         if [ {fail_from} -gt 0 ] && [ \"$n\" -ge {fail_from} ]; then printf '%s' \"{{\\\"schema\\\":\\\"bay.check.result/1\\\",\\\"exercised\\\":{{\\\"commit\\\":\\\"$commit\\\",\\\"tree\\\":\\\"$tree\\\"}},\\\"plan\\\":{{\\\"selected\\\":1,\\\"completed\\\":1}},\\\"checks\\\":[{{\\\"id\\\":\\\"t\\\",\\\"verdict\\\":\\\"product_failure\\\"}}]}}\"; exit 0; fi",
        runs = runs.display(),
        other = other.display(),
    );
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("docs/keep.md", "k\n")],
        &check_files(&before, &passing_result(1), 1, 60),
    );
    test_box.clone_of(&remote, "mover");
    Queued {
        test_box,
        repo,
        remote,
        runs,
    }
}

fn runs(queued: &Queued) -> usize {
    std::fs::read_to_string(&queued.runs).map_or(0, |text| text.lines().count())
}

fn land(queued: &Queued, file: &str) -> (String, PathBuf) {
    let started = queued
        .test_box
        .bay_ok(&queued.repo, &["start", file, "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(format!("src/{file}.rs")), "fn x() {}\n").unwrap();
    let (code, queued_result) = result_json(queued.test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{queued_result}");
    (id, view)
}

fn queue_refs(test_box: &TestBox, remote: &Path) -> String {
    test_box.git_bare(
        remote,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/bay/queue/",
            "refs/bay/queue-done/",
        ],
    )
}

#[test]
fn two_trunk_moves_put_the_operation_in_the_queue_and_it_lands_as_head() {
    let queued = setup(2, 0);
    let (id, _) = land(&queued, "first");
    let executor = queued.test_box.clone_of(&queued.remote, "executor");
    let first = queued
        .test_box
        .bay_ok(&executor, &["executor", "run", "--once"]);
    let result = &first["detail"]["ran"][0];
    assert_eq!(result["decision"], "queued", "{first}");
    assert_eq!(result["reason"], "PUBLICATION_QUEUED");
    assert_eq!(
        queue_refs(&queued.test_box, &queued.remote),
        "refs/bay/queue/1"
    );

    let second = queued
        .test_box
        .bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(second["detail"]["ran"][0]["decision"], "landed", "{second}");
    assert_eq!(queued.test_box.trunk_change_ids(&queued.remote), vec![id]);
    // Trunk, the head's release and the landed marker moved together.
    assert_eq!(
        queue_refs(&queued.test_box, &queued.remote),
        "refs/bay/queue-done/1\nrefs/bay/queue/1"
    );
    let release = queued.test_box.git_bare(
        &queued.remote,
        &["show", "refs/bay/queue-done/1:release.json"],
    );
    assert!(release.contains("\"published\""), "{release}");
}

#[test]
fn an_arrival_while_the_queue_is_live_holds_a_place_without_a_check_and_lands_in_order() {
    let queued = setup(2, 0);
    let (first, _) = land(&queued, "first");
    let executor = queued.test_box.clone_of(&queued.remote, "executor");
    queued
        .test_box
        .bay_ok(&executor, &["executor", "run", "--once"]);
    let before = runs(&queued);

    let (second, view) = land(&queued, "second");
    let (code, held) = result_json(queued.test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{held}");
    assert_eq!(held["decision"], "queued");
    assert!(
        held["reason"] == "QUEUE_HELD" || held["reason"] == "PUBLICATION_QUEUED",
        "{held}"
    );
    assert_eq!(held["detail"]["position"], 2, "{held}");
    assert_eq!(runs(&queued), before, "no check ran for the arrival");

    for _ in 0..3 {
        queued
            .test_box
            .bay_ok(&executor, &["executor", "run", "--once"]);
    }
    let ids = queued.test_box.trunk_change_ids(&queued.remote);
    assert_eq!(
        ids,
        vec![second, first],
        "the queue's order, newest first on trunk"
    );
}

#[test]
fn a_head_whose_check_fails_releases_the_queue_at_once() {
    // Runs 1–2 move trunk, so the first change queues; its run 3 as head fails.
    let queued = setup(2, 3);
    let (first, _) = land(&queued, "first");
    let executor = queued.test_box.clone_of(&queued.remote, "executor");
    queued
        .test_box
        .bay_ok(&executor, &["executor", "run", "--once"]);
    let failed = queued
        .test_box
        .bay_ok(&executor, &["executor", "run", "--once"]);
    let result = failed["detail"]["ran"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["change"] == first.as_str())
        .cloned()
        .unwrap();
    assert_eq!(result["reason"], "CHECK_FAILED", "{failed}");
    let release = queued.test_box.git_bare(
        &queued.remote,
        &["show", "refs/bay/queue-done/1:release.json"],
    );
    assert!(release.contains("check_failed"), "{release}");
}

#[test]
fn an_executor_killed_right_after_publishing_leaves_the_queue_free() {
    let queued = setup(2, 0);
    let (id, _) = land(&queued, "first");
    let executor = queued.test_box.clone_of(&queued.remote, "executor");
    queued
        .test_box
        .bay_ok(&executor, &["executor", "run", "--once"]);
    let killed = queued
        .test_box
        .bay(&executor)
        .env("BAY_TEST_KILL_AT", "after-publish")
        .args(["executor", "run", "--once"])
        .output()
        .unwrap();
    assert_eq!(killed.status.code(), Some(137));
    assert_eq!(
        queue_refs(&queued.test_box, &queued.remote),
        "refs/bay/queue-done/1\nrefs/bay/queue/1"
    );
    let marker = queued.test_box.git_bare(
        &queued.remote,
        &["rev-parse", &format!("refs/bay/landed/{id}")],
    );
    assert_eq!(
        marker,
        queued
            .test_box
            .git_bare(&queued.remote, &["rev-parse", "main"])
    );
}

#[test]
fn a_newer_revision_of_a_queued_entry_replaces_it_in_place() {
    let queued = setup(2, 0);
    let (_first, _) = land(&queued, "first");
    let executor = queued.test_box.clone_of(&queued.remote, "executor");
    queued
        .test_box
        .bay_ok(&executor, &["executor", "run", "--once"]);
    // A second change queues behind the head.
    let (second, view) = land(&queued, "second");
    let (code, _) = result_json(queued.test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0);
    // Its newer revision takes the same place.
    std::fs::write(view.join("src/second.rs"), "fn x() { newer }\n").unwrap();
    let (code, newer) = result_json(queued.test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{newer}");
    assert_eq!(newer["detail"]["seq"], 2, "{newer}");
    assert_eq!(newer["op_id"], format!("{second}.2"));
    let entry = queued
        .test_box
        .git_bare(&queued.remote, &["show", "refs/bay/queue/2:entry.json"]);
    assert!(
        entry.contains(&format!("{second}.2")) && entry.contains("replaces"),
        "{entry}"
    );
}

#[test]
fn a_head_released_by_its_deadline_three_times_is_queue_deadline_exceeded() {
    let queued = setup(0, 0);
    let (id, _) = land(&queued, "first");
    // Three earlier places of this operation, each released by the progress deadline.
    let git = bay::git::Git::new(&queued.repo);
    for seq in 1..=3u64 {
        let entry =
            bay::queue::release_commit(&git, None, seq, &format!("{id}.1"), "deadline").unwrap();
        let reservation = {
            let text = format!("{{\"op_id\":\"{id}.1\",\"change\":\"{id}\"}}\n");
            let mut child = queued
                .test_box
                .git(&queued.repo)
                .args(["hash-object", "-w", "--stdin"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            std::io::Write::write_all(&mut child.stdin.take().unwrap(), text.as_bytes()).unwrap();
            let blob = String::from_utf8(child.wait_with_output().unwrap().stdout)
                .unwrap()
                .trim()
                .to_owned();
            let mut child = queued
                .test_box
                .git(&queued.repo)
                .arg("mktree")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            std::io::Write::write_all(
                &mut child.stdin.take().unwrap(),
                format!("100644 blob {blob}\tentry.json\n").as_bytes(),
            )
            .unwrap();
            let tree = String::from_utf8(child.wait_with_output().unwrap().stdout)
                .unwrap()
                .trim()
                .to_owned();
            queued
                .test_box
                .git_ok(&queued.repo, &["commit-tree", &tree, "-m", "reserve"])
        };
        queued.test_box.git_ok(
            &queued.repo,
            &[
                "push",
                "-q",
                "origin",
                &format!("{reservation}:refs/bay/queue/{seq}"),
                &format!("{entry}:refs/bay/queue-done/{seq}"),
            ],
        );
    }
    let executor = queued.test_box.clone_of(&queued.remote, "executor");
    let ran = queued
        .test_box
        .bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(
        ran["detail"]["ran"][0]["reason"], "QUEUE_DEADLINE_EXCEEDED",
        "{ran}"
    );
    assert_eq!(
        queued.test_box.trunk_change_ids(&queued.remote),
        Vec::<String>::new()
    );
}

#[test]
fn the_hook_refuses_a_reservation_that_is_not_above_every_existing_one() {
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
    let signer = bay::signing::Signer {
        key: keys.executor.clone(),
        name: support::EXECUTOR.to_owned(),
    };
    let git = bay::git::Git::new(&repo);
    let release = bay::queue::release_commit(&git, Some(&signer), 5, "x.1", "published").unwrap();
    // A signed reservation at 5, then one at 3: the second is below an existing number.
    let opened = bay::repo::Repo::open(&repo).unwrap();
    let seq = bay::queue::join(&opened, Some(&signer), "aaaaaaaa.1", "aaaaaaaa").unwrap();
    assert_eq!(seq, 1);
    test_box.git_ok(
        &repo,
        &[
            "push",
            "-q",
            "origin",
            &format!("{release}:refs/bay/queue-done/1"),
        ],
    );
    let reservation = test_box.git_ok(&repo, &["rev-parse", "refs/bay/queue/1"]);
    // Place 3 is above every existing number; place 2, after it, is not.
    test_box.git_ok(
        &repo,
        &[
            "push",
            "-q",
            "origin",
            &format!("{reservation}:refs/bay/queue/3"),
        ],
    );
    let out = test_box
        .git(&repo)
        .args(["push", "origin", &format!("{reservation}:refs/bay/queue/2")])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("above every existing one"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
#[ignore = "load, retry exhaustion, worker outages and policy releases are not built"]
fn twenty_agents_with_twelve_minute_checks_all_reach_a_stated_outcome() {
    panic!("G35: the load and failure-injection cases are not implemented");
}

/// A bay-hosted, signed trunk whose trunk hook has a 3 s time bound (an honest judgement takes about 1 s), and a change ready to land.
/// Returns the box, the host, the change id and its view.
fn hosted_with_hook_bound() -> (TestBox, PathBuf, String, PathBuf) {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    test_box.git_bare(&host, &["config", "bay.hookTimeoutS", "3"]);
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
    let (id, view) = test_box.start_and_edit(&repo);
    (test_box, host, id, view)
}

/// The trunk hook times out on the first two publishing pushes: each is `HOOK_TIMEOUT`, which
/// retries the same candidate after a short wait and counts as neither a trunk move nor a
/// deadline release (`INTERFACES` § 2). The third push lands.
#[test]
fn a_hook_timeout_retries_the_same_candidate_uncounted() {
    let (test_box, host, id, view) = hosted_with_hook_bound();
    let (code, landed) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_HOOK_SLOW_PUSHES", "2")
            .args(["land", "--wait"]),
    );
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "landed", "{landed}");
    assert_eq!(test_box.trunk_change_ids(&host), vec![id.clone()]);
    let status = test_box.bay_ok(&view, &["status", &id]);
    let op = &status["detail"]["changes"][0]["operation"];
    assert_eq!(op["publish_retries"], 2, "{status}");
    assert_eq!(op["trunk_moved"], 0, "{status}");
    assert_eq!(op["attempts"], 1, "the same candidate: {status}");
}

/// A trunk hook that always times out: after five uncounted retries the operation is blocked
/// with `PUBLISH_RETRY_EXHAUSTED`, and trunk does not move (DESIGN § 6).
#[test]
fn five_hook_timeouts_end_in_publish_retry_exhausted() {
    let (test_box, host, id, view) = hosted_with_hook_bound();
    let trunk = test_box.git_bare(&host, &["rev-parse", "main"]);
    let (code, blocked) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_HOOK_SLOW_PUSHES", "100")
            .args(["land", "--wait"]),
    );
    assert_eq!(code, 10, "{blocked}");
    assert_eq!(blocked["reason"], "PUBLISH_RETRY_EXHAUSTED", "{blocked}");
    assert_eq!(test_box.git_bare(&host, &["rev-parse", "main"]), trunk);
    let status = test_box.bay_ok(&view, &["status", &id]);
    let op = &status["detail"]["changes"][0]["operation"];
    assert_eq!(op["publish_retries"], 5, "{status}");
    assert_eq!(op["outcome"], "PUBLISH_RETRY_EXHAUSTED", "{status}");
}

/// Queues a change, then runs its head attempt paused just before the publishing push, and
/// creates the head's release there as a progress deadline would. Returns the queued trunk, the
/// change id, the executor clone, and the paused child with its release file.
fn head_released_mid_publish() -> (Queued, String, PathBuf, std::process::Child, PathBuf) {
    let queued = setup(2, 0);
    let (id, _) = land(&queued, "first");
    let executor = queued.test_box.clone_of(&queued.remote, "executor");
    queued
        .test_box
        .bay_ok(&executor, &["executor", "run", "--once"]);
    let go = queued.test_box.path().join("go");
    let child = queued
        .test_box
        .bay_process(&executor)
        .env("BAY_TEST_PAUSE_AT", "before-publish")
        .env("BAY_TEST_PAUSE_FILE", &go)
        .args(["--json", "executor", "run", "--once"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let paused = queued.test_box.path().join("go.paused");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !paused.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the head never paused"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let git = bay::git::Git::new(&queued.repo);
    let release =
        bay::queue::release_commit(&git, None, 1, &format!("{id}.1"), "deadline").unwrap();
    queued.test_box.git_ok(
        &queued.repo,
        &[
            "push",
            "-q",
            "origin",
            &format!("{release}:refs/bay/queue-done/1"),
        ],
    );
    (queued, id, executor, child, go)
}

/// A head whose release the progress deadline created while it was publishing has its push
/// refused; it rejoins the queue, the release counts toward `PUBLISH_RETRY_EXHAUSTED` and not
/// toward `QUEUE_DEADLINE_EXCEEDED`, and it lands (DESIGN § 6).
#[test]
fn a_deadline_release_mid_publish_rejoins_and_counts_as_a_publish_retry() {
    let (queued, id, executor, child, go) = head_released_mid_publish();
    std::fs::write(&go, "go").unwrap();
    let out = child.wait_with_output().unwrap();
    let ran: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(ran["detail"]["ran"][0]["decision"], "queued", "{ran}");
    for _ in 0..2 {
        queued
            .test_box
            .bay_ok(&executor, &["executor", "run", "--once"]);
    }
    assert_eq!(
        queued.test_box.trunk_change_ids(&queued.remote),
        vec![id.clone()]
    );
    let status = queued.test_box.bay_ok(&queued.repo, &["status", &id]);
    let op = &status["detail"]["changes"][0]["operation"];
    assert_eq!(op["publish_retries"], 1, "{status}");
}
