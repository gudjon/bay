//! G35. Publication never livelocks.
//!
//! After two `TRUNK_MOVED` refusals an operation joins the queue and lands in order; an
//! operation arriving while a queue is live holds a place without running a check
//! (`QUEUE_HELD`); a head whose check fails releases the queue at once; an executor killed
//! right after publishing leaves the queue free and the landed marker present (one atomic push);
//! a newer revision of a queued entry replaces it in place; a head released by its deadline
//! three times becomes `QUEUE_DEADLINE_EXCEEDED`; the hook refuses a reservation number that is
//! not above every existing one; a `HOOK_TIMEOUT` retries the same candidate, and five
//! uncounted retries (hook timeouts, deadline releases during a publish) end in
//! `PUBLISH_RETRY_EXHAUSTED`; a policy landing during the head's check releases it with
//! `POLICY_CHANGED` while the queue keeps its order; 20 agents and three executors at once all
//! land with bounded check time. The worker outage and the worker capabilities stay
//! `#[ignore]`: how a box reports that it has no check capability (`CHECK_UNAVAILABLE`) is not
//! decided yet.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

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
    setup_with_policy_edit(moves, fail_from, 0)
}

/// As [`setup`], and run `policy_at` (when above zero) lands a rules-only policy edit
/// (`revision = 2`, the same checks) from the other clone while it checks.
fn setup_with_policy_edit(moves: u32, fail_from: u32, policy_at: u32) -> Queued {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let runs = test_box.path().join("runs");
    let other = test_box.path().join("mover");
    let before = format!(
        "echo \"$tree\" >> '{runs}'; n=$(wc -l < '{runs}' | tr -d ' '); \
         if [ \"$n\" -le {moves} ]; then ( cd '{other}' && git pull -q --rebase && echo \"$n\" > docs/move-$n.md && git add docs && git -c user.name=t -c user.email=t@t commit -q -m \"move $n\" && git push -q origin main ); fi; \
         if [ {policy_at} -gt 0 ] && [ \"$n\" -eq {policy_at} ]; then ( cd '{other}' && git pull -q --rebase && sed 's/^revision = .*/revision = 2/' .bay/policy.toml > policy.new && mv policy.new .bay/policy.toml && git add .bay/policy.toml && git -c user.name=t -c user.email=t@t commit -q -m \"policy revision 2\" && git push -q origin main ); fi; \
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

/// A livelock is a stall: the load case's executors stop once no change landed for this long,
/// however long the whole run takes on a loaded box (each landing takes seconds there).
const LOAD_STALL: std::time::Duration = std::time::Duration::from_secs(300);

/// Runs `executor run --once` in `clone` until `agents` changes are on trunk or no change has
/// landed for [`LOAD_STALL`]; `progress` is the landed count and when it last grew, shared by
/// every executor. Returns each pass's result.
fn executor_passes(
    test_box: &TestBox,
    clone: &Path,
    remote: &Path,
    progress: &std::sync::Mutex<(usize, std::time::Instant)>,
    agents: usize,
) -> Vec<serde_json::Value> {
    let mut ran = Vec::new();
    loop {
        let landed = test_box.trunk_change_ids(remote).len();
        let stalled = {
            let mut last = progress.lock().unwrap();
            if landed > last.0 {
                *last = (landed, std::time::Instant::now());
            }
            last.1.elapsed() >= LOAD_STALL
        };
        if landed >= agents || stalled {
            return ran;
        }
        let out = test_box
            .bay_process(clone)
            .args(["--json", "executor", "run", "--once"])
            .output()
            .unwrap();
        let pass: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
            serde_json::json!({"decision": "failed", "stdout": String::from_utf8_lossy(&out.stdout)})
        });
        ran.push(pass);
    }
}

/// The load case, scaled to a test: 20 agents each submit one change and leave, and three
/// executors (each its own executor and producer name) work them at once, so their own
/// publications move trunk under each other's checks. Every operation reaches a stated
/// outcome, here a landing, each change lands exactly once, no executor reports a failure,
/// and worker time per landing stays bounded: the checks run are at most three per landing
/// (two `TRUNK_MOVED` refusals before the queue, then one check as head) plus one per
/// executor for a head checked while a publisher that passed the gate before the queue was
/// live landed (`DESIGN` § 6, `ROADMAP` G35). The 12-minute checks are scaled down to a
/// fraction of a second; the arithmetic of the example load is the design's, not this test's.
#[test]
fn twenty_agents_and_three_executors_all_land_with_bounded_check_time() {
    const AGENTS: usize = 20;
    const EXECUTORS: usize = 3;
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let runs = test_box.path().join("runs");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files(
            &format!("echo \"$tree\" >> '{}'; sleep 0.3", runs.display()),
            &passing_result(1),
            1,
            60,
        ),
    );
    let mut ids = Vec::new();
    for agent in 0..AGENTS {
        let file = format!("agent{agent}");
        let started = test_box.bay_ok(&repo, &["start", &file, "--paths", "src/"]);
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(format!("src/{file}.rs")), "fn x() {}\n").unwrap();
        let (code, submitted) = result_json(test_box.bay(&view).arg("land"));
        assert_eq!(code, 0, "{submitted}");
        ids.push(started["change"].as_str().unwrap().to_owned());
    }
    let executors: Vec<PathBuf> = (0..EXECUTORS)
        .map(|n| {
            let clone = test_box.clone_of(&remote, &format!("executor-{n}"));
            test_box.git_ok(
                &clone,
                &["config", "bay.executorName", &format!("exec-{n}")],
            );
            test_box.git_ok(
                &clone,
                &["config", "bay.producerName", &format!("exec-{n}-checks")],
            );
            clone
        })
        .collect();

    let progress = std::sync::Mutex::new((0usize, std::time::Instant::now()));
    let reports: Vec<Vec<serde_json::Value>> = std::thread::scope(|scope| {
        let workers: Vec<_> = executors
            .iter()
            .map(|clone| {
                let (test_box, remote, progress) = (&test_box, &remote, &progress);
                scope.spawn(move || executor_passes(test_box, clone, remote, progress, AGENTS))
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect()
    });

    // No pass and no operation in it ends in an undefined state.
    for pass in reports.iter().flatten() {
        assert_eq!(pass["decision"], "ok", "{pass}");
        for result in pass["detail"]["ran"].as_array().unwrap() {
            assert_ne!(result["decision"], "failed", "{result}");
        }
    }
    let mut landed = test_box.trunk_change_ids(&remote);
    assert_eq!(landed.len(), AGENTS, "every change lands, once: {landed:?}");
    landed.sort();
    landed.dedup();
    let mut expected = ids.clone();
    expected.sort();
    assert_eq!(landed, expected);
    let status = test_box.bay_ok(&repo, &["status"]);
    let changes = status["detail"]["changes"].as_array().unwrap();
    assert_eq!(changes.len(), AGENTS, "{status}");
    for change in changes {
        assert_eq!(change["operation"]["state"], "published", "{change}");
    }
    let checks = std::fs::read_to_string(&runs).unwrap().lines().count();
    eprintln!("G35 load: {checks} checks for {AGENTS} landings");
    assert!(
        checks <= 3 * AGENTS + EXECUTORS,
        "worker time per landing is bounded: {checks} checks for {AGENTS} landings"
    );
}

/// The head always releases (`DESIGN` § 6): a reservation whose operation already landed (a
/// second place two executors took for it at once, or one whose release a crash lost) is never
/// run again, so the executor releases it at once instead of letting it hold the queue for
/// good, and the next change publishes.
#[test]
fn a_head_whose_operation_already_landed_is_released_at_once() {
    let queued = setup(2, 0);
    let (first, _) = land(&queued, "first");
    let executor = queued.test_box.clone_of(&queued.remote, "executor");
    for _ in 0..2 {
        queued
            .test_box
            .bay_ok(&executor, &["executor", "run", "--once"]);
    }
    assert_eq!(
        queued.test_box.trunk_change_ids(&queued.remote),
        vec![first.clone()]
    );
    // A second place for the landed operation, behind its released first one.
    let repo = bay::repo::Repo::open(&queued.repo).unwrap();
    let stale = bay::queue::join(&repo, None, &format!("{first}.1"), &first).unwrap();
    assert_eq!(stale, 2);

    let (second, _) = land(&queued, "second");
    let ran = queued
        .test_box
        .bay_ok(&executor, &["executor", "run", "--once"]);
    let result = ran["detail"]["ran"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["change"] == second.as_str())
        .cloned()
        .unwrap();
    assert_eq!(result["decision"], "landed", "{ran}");
    let release = queued.test_box.git_bare(
        &queued.remote,
        &["show", "refs/bay/queue-done/2:release.json"],
    );
    assert!(release.contains("\"published\""), "{release}");
    assert_eq!(
        queued.test_box.trunk_change_ids(&queued.remote),
        vec![second, first]
    );
}

/// The head always releases (`DESIGN` § 6), also for a place of a change that the retention
/// cut closed: the cut chain keeps no operations, and an expired op id is never run again
/// (§ 5, § 9a). The executor releases that place at once with the change's state, and the next
/// change publishes; nothing waits for a progress deadline, which `--once` never sees fire.
#[test]
fn a_head_whose_change_the_retention_cut_closed_is_released_at_once() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[retention]\nwindow_s = 0\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let runs = test_box.path().join("runs");
    let queued = Queued {
        test_box,
        repo,
        remote,
        runs,
    };
    let (first, _) = land(&queued, "first");
    let executor = queued.test_box.clone_of(&queued.remote, "executor");
    let landed = queued
        .test_box
        .bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(landed["detail"]["ran"][0]["decision"], "landed", "{landed}");
    let cut = queued
        .test_box
        .bay_ok(&executor, &["executor", "run", "--once"]);
    assert!(
        cut["detail"]["ran"][0]["retention_cut"].is_string(),
        "{cut}"
    );
    // A place for the expired operation, as a crash that lost its release would leave it.
    let repo = bay::repo::Repo::open(&queued.repo).unwrap();
    let stale = bay::queue::join(&repo, None, &format!("{first}.1"), &first).unwrap();
    assert_eq!(stale, 1);

    let (second, _) = land(&queued, "second");
    let ran = queued
        .test_box
        .bay_ok(&executor, &["executor", "run", "--once"]);
    let result = ran["detail"]["ran"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["change"] == second.as_str())
        .cloned()
        .unwrap();
    assert_eq!(result["decision"], "landed", "{ran}");
    let release = queued.test_box.git_bare(
        &queued.remote,
        &["show", "refs/bay/queue-done/1:release.json"],
    );
    assert!(release.contains("\"landed\""), "{release}");
}

/// A release the remote refuses never stops the supervisor (`DESIGN` § 6, § 9a): the pass
/// reports it under `unreleased`, runs every operation it can, and the next pass tries the
/// release again. Here the remote refuses every release while a landed operation's stale place
/// is head, then accepts them again.
#[test]
fn a_release_the_remote_refuses_is_reported_and_tried_again_on_the_next_pass() {
    let queued = setup(0, 0);
    let (first, _) = land(&queued, "first");
    let executor = queued.test_box.clone_of(&queued.remote, "executor");
    queued
        .test_box
        .bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(
        queued.test_box.trunk_change_ids(&queued.remote),
        vec![first.clone()]
    );
    let repo = bay::repo::Repo::open(&queued.repo).unwrap();
    let stale = bay::queue::join(&repo, None, &format!("{first}.1"), &first).unwrap();
    assert_eq!(stale, 1);
    let hook = queued.remote.join("hooks/pre-receive");
    std::fs::write(
        &hook,
        "#!/bin/sh\nwhile read old new name; do case \"$name\" in refs/bay/queue-done/*) echo 'no releases today' >&2; exit 1;; esac; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let (second, _) = land(&queued, "second");
    let held = queued
        .test_box
        .bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(held["decision"], "ok", "{held}");
    assert!(
        held["detail"]["unreleased"]
            .as_str()
            .is_some_and(|message| message.contains("queue entry 1")),
        "{held}"
    );
    assert_eq!(
        queued.test_box.trunk_change_ids(&queued.remote),
        vec![first.clone()]
    );

    std::fs::remove_file(&hook).unwrap();
    let ran = queued
        .test_box
        .bay_ok(&executor, &["executor", "run", "--once"]);
    assert!(ran["detail"]["unreleased"].is_null(), "{ran}");
    assert_eq!(
        queued.test_box.trunk_change_ids(&queued.remote),
        vec![second, first]
    );
}

/// Two executors that queue one operation at once both create a reservation; the later one
/// sees the earlier and releases its own, so the operation holds one place (`DESIGN` § 6).
#[test]
fn one_operation_queued_twice_at_once_holds_one_place() {
    let queued = setup(0, 0);
    let repo = bay::repo::Repo::open(&queued.repo).unwrap();
    let first = bay::queue::join(&repo, None, "aaaaaaaa.1", "aaaaaaaa").unwrap();
    // The second executor read the queue before the first reservation existed.
    let second = bay::queue::join(&repo, None, "aaaaaaaa.1", "aaaaaaaa").unwrap();
    assert_eq!((first, second), (1, 1));
    assert_eq!(
        queue_refs(&queued.test_box, &queued.remote),
        "refs/bay/queue-done/2\nrefs/bay/queue/1\nrefs/bay/queue/2"
    );
    let queue = bay::queue::Queue::read(&repo).unwrap();
    assert_eq!(queue.of("aaaaaaaa.1").map(|entry| entry.seq), Some(1));
    assert_eq!(queue.position(1), 1);
}

/// A two-hour worker outage with three queued operations and their agents gone ends with all
/// three landing after the workers return, none `QUEUE_DEADLINE_EXCEEDED` (`ROADMAP` G35).
/// It needs a head that cannot start its check to be released `CHECK_UNAVAILABLE` and wait
/// outside the queue (`waiting_for: "worker"`, `DESIGN` § 6), and the spec does not yet say how a
/// box or its adapter reports that no capable worker answered (BACKLOG 12).
#[test]
#[ignore = "how a box reports no check capability is not decided (CHECK_UNAVAILABLE, BACKLOG 12)"]
fn a_worker_outage_with_three_queued_operations_ends_with_all_three_landed() {
    panic!("G35: CHECK_UNAVAILABLE has no decided source yet");
}

/// A worker with the wrong capabilities is not a substitute (`DESIGN` § 7, `ROADMAP` G35). The
/// check request names its `requires`, but how a worker's capabilities are declared and matched
/// is not decided.
#[test]
#[ignore = "how a worker's capabilities are declared and matched is not decided (DESIGN § 7)"]
fn a_worker_with_the_wrong_capabilities_is_not_a_substitute() {
    panic!("G35: worker capabilities have no decided form yet");
}

/// A policy change lands while the head checks: the head is released with `POLICY_CHANGED` and
/// rejoins at the back; the queued entries keep their positions and are rechecked under the new
/// policy when they become head; the release counts toward neither `QUEUE_DEADLINE_EXCEEDED`
/// nor `PUBLISH_RETRY_EXHAUSTED` (`DESIGN` § 6, `ROADMAP` G35).
#[test]
fn a_policy_landing_during_the_heads_check_releases_it_and_the_queue_keeps_its_order() {
    // Runs 1–2 move trunk, so the first change queues; its run 3 as head lands a policy edit.
    let queued = setup_with_policy_edit(2, 0, 3);
    let (first, _) = land(&queued, "first");
    let executor = queued.test_box.clone_of(&queued.remote, "executor");
    queued
        .test_box
        .bay_ok(&executor, &["executor", "run", "--once"]);
    let mut behind = Vec::new();
    for file in ["second", "third"] {
        let (id, view) = land(&queued, file);
        let (code, held) = result_json(queued.test_box.bay(&view).args(["land", "--wait"]));
        assert_eq!(code, 0, "{held}");
        assert_eq!(held["decision"], "queued", "{held}");
        behind.push((id, held["detail"]["seq"].as_u64().unwrap()));
    }
    assert_eq!(
        behind.iter().map(|(_, seq)| *seq).collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert_eq!(
        runs(&queued),
        2,
        "no check ran for the entries behind the head"
    );

    for _ in 0..8 {
        if queued.test_box.trunk_change_ids(&queued.remote).len() == 3 {
            break;
        }
        queued
            .test_box
            .bay_ok(&executor, &["executor", "run", "--once"]);
    }
    let release = queued.test_box.git_bare(
        &queued.remote,
        &["show", "refs/bay/queue-done/1:release.json"],
    );
    assert!(release.contains("\"policy_changed\""), "{release}");
    // The entries behind the head kept their places and landed first, in order; the released
    // head rejoined at the back.
    for (id, seq) in &behind {
        let entry = queued.test_box.git_bare(
            &queued.remote,
            &["show", &format!("refs/bay/queue/{seq}:entry.json")],
        );
        assert!(entry.contains(&format!("{id}.1")), "{entry}");
    }
    let rejoined = queued
        .test_box
        .git_bare(&queued.remote, &["show", "refs/bay/queue/4:entry.json"]);
    assert!(rejoined.contains(&format!("{first}.1")), "{rejoined}");
    assert_eq!(
        queued.test_box.trunk_change_ids(&queued.remote),
        vec![first.clone(), behind[1].0.clone(), behind[0].0.clone()],
        "the queue's order, newest first on trunk"
    );
    // One check per entry as head under the new policy, and one recheck of the released head.
    assert_eq!(runs(&queued), 6);
    let status = queued.test_box.bay_ok(&queued.repo, &["status", &first]);
    let op = &status["detail"]["changes"][0]["operation"];
    assert_eq!(op["policy_changed"], 1, "{status}");
    assert_eq!(op["publish_retries"], 0, "{status}");
    assert_eq!(op["state"], "published", "{status}");
    // Each landed tree was checked under the policy it landed with.
    let notes = queued.test_box.git_bare(
        &queued.remote,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/notes/bay/evidence/",
        ],
    );
    for back in ["main", "main~1", "main~2"] {
        let tree = queued
            .test_box
            .git_bare(&queued.remote, &["rev-parse", &format!("{back}^{{tree}}")]);
        let evidence: String = notes
            .lines()
            .map(|notes_ref| {
                queued.test_box.git_bare(
                    &queued.remote,
                    &["notes", &format!("--ref={notes_ref}"), "show", &tree],
                )
            })
            .collect();
        assert!(
            evidence.contains("\"policy_revision\": 2"),
            "{back}: {evidence}"
        );
    }
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
    let (child, go) = pause_head_and_release(&queued, &executor, &id, 1);
    (queued, id, executor, child, go)
}

/// Runs the head attempt of `id` at queue place `seq` paused just before its publishing push,
/// and creates that place's release there, as a progress deadline would. Returns the paused
/// child and the file that lets it go on.
fn pause_head_and_release(
    queued: &Queued,
    executor: &Path,
    id: &str,
    seq: u64,
) -> (std::process::Child, PathBuf) {
    pause_head_and_release_as(queued, executor, id, seq, "deadline")
}

/// As [`pause_head_and_release`], with the release's `reason`.
fn pause_head_and_release_as(
    queued: &Queued,
    executor: &Path,
    id: &str,
    seq: u64,
    reason: &str,
) -> (std::process::Child, PathBuf) {
    let go = queued.test_box.path().join(format!("go-{seq}"));
    let mut child = queued
        .test_box
        .bay_process(executor)
        .env("BAY_TEST_PAUSE_AT", "before-publish")
        .env("BAY_TEST_PAUSE_FILE", &go)
        .args(["--json", "executor", "run", "--once"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let paused = queued.test_box.path().join(format!("go-{seq}.paused"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !paused.exists() {
        if child.try_wait().unwrap().is_some() {
            let out = child.wait_with_output().unwrap();
            panic!(
                "the head at place {seq} ended without reaching its publishing push: {}",
                String::from_utf8_lossy(&out.stdout)
            );
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the head never paused"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let git = bay::git::Git::new(&queued.repo);
    let release = bay::queue::release_commit(&git, None, seq, &format!("{id}.1"), reason).unwrap();
    queued.test_box.git_ok(
        &queued.repo,
        &[
            "push",
            "-q",
            "origin",
            &format!("{release}:refs/bay/queue-done/{seq}"),
        ],
    );
    (child, go)
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

/// The progress deadline releases the head while it publishes, five times in a row. Each
/// release rejoins the queue and counts as an uncounted publish retry, never toward
/// `QUEUE_DEADLINE_EXCEEDED`, so the third does not block it; the fifth ends the operation in
/// `PUBLISH_RETRY_EXHAUSTED`, and trunk never moves (`DESIGN` § 6, `INTERFACES` § 2).
#[test]
fn the_fifth_deadline_release_mid_publish_ends_in_publish_retry_exhausted() {
    let queued = setup(2, 0);
    let (id, _) = land(&queued, "first");
    let executor = queued.test_box.clone_of(&queued.remote, "executor");
    queued
        .test_box
        .bay_ok(&executor, &["executor", "run", "--once"]);
    let trunk = queued
        .test_box
        .git_bare(&queued.remote, &["rev-parse", "main"]);
    for seq in 1..=5u64 {
        let (child, go) = pause_head_and_release(&queued, &executor, &id, seq);
        std::fs::write(&go, "go").unwrap();
        let out = child.wait_with_output().unwrap();
        let ran: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let result = &ran["detail"]["ran"][0];
        if seq < 5 {
            assert_eq!(
                result["reason"], "PUBLICATION_QUEUED",
                "release {seq}: {ran}"
            );
            assert_eq!(result["detail"]["seq"], seq + 1, "release {seq}: {ran}");
        } else {
            assert_eq!(result["reason"], "PUBLISH_RETRY_EXHAUSTED", "{ran}");
        }
    }
    assert_eq!(
        queued
            .test_box
            .git_bare(&queued.remote, &["rev-parse", "main"]),
        trunk
    );
    let status = queued.test_box.bay_ok(&queued.repo, &["status", &id]);
    let op = &status["detail"]["changes"][0]["operation"];
    assert_eq!(op["publish_retries"], 5, "{status}");
    assert_eq!(op["outcome"], "PUBLISH_RETRY_EXHAUSTED", "{status}");
}

/// Only a release the progress deadline created is a publish retry (`DESIGN` § 6). A head
/// whose place was released for another reason while it published (here `superseded`, as a
/// newer revision's executor releases it) neither rejoins the queue nor uses a publish retry:
/// the newer revision is the one that joins at the back. Nothing is published.
#[test]
fn a_release_for_another_reason_mid_publish_is_no_publish_retry() {
    let queued = setup(2, 0);
    let (id, _) = land(&queued, "first");
    let executor = queued.test_box.clone_of(&queued.remote, "executor");
    queued
        .test_box
        .bay_ok(&executor, &["executor", "run", "--once"]);
    let (child, go) = pause_head_and_release_as(&queued, &executor, &id, 1, "superseded");
    std::fs::write(&go, "go").unwrap();
    let out = child.wait_with_output().unwrap();
    let ran: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let result = &ran["detail"]["ran"][0];
    assert_ne!(result["decision"], "queued", "{ran}");
    assert!(
        result["detail"]
            .as_str()
            .is_some_and(|message| message.contains("superseded")),
        "{ran}"
    );
    assert_eq!(
        queue_refs(&queued.test_box, &queued.remote),
        "refs/bay/queue-done/1\nrefs/bay/queue/1",
        "no second place"
    );
    assert_eq!(
        queued.test_box.trunk_change_ids(&queued.remote),
        Vec::<String>::new()
    );
    let status = queued.test_box.bay_ok(&queued.repo, &["status", &id]);
    let op = &status["detail"]["changes"][0]["operation"];
    assert_eq!(op["publish_retries"], 0, "{status}");
}
