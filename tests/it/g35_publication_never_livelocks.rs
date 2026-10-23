//! G35. Publication never livelocks.
//!
//! After two `MAIN_MOVED` refusals an operation joins the queue and lands in order; an
//! operation arriving while a queue is live holds a place without running a check
//! (`QUEUED`); a head whose check fails releases the queue at once; an executor killed
//! right after publishing leaves the queue free and the landed marker present (one atomic push);
//! a newer revision of a queued entry replaces it in place; a head released by its deadline
//! three times becomes `QUEUE_DEADLINE_EXCEEDED`; the hook refuses a reservation number that is
//! not above every existing one; a `HOOK_TIMEOUT` retries the same candidate, and five
//! uncounted retries (hook timeouts, deadline releases during a publish) end in
//! `MERGE_RETRY_EXHAUSTED`; a policy landing during the head's check releases it with
//! `POLICY_CHANGED` while the queue keeps its order; 20 agents and three executors at once all
//! land with bounded check time; a worker outage releases each head as `unavailable`, never
//! by its deadline, and all three queued operations land once the workers return; a box that
//! does not declare a check's environment (`bay.checkEnvironment`) runs nothing for it and
//! waits with `CHECK_UNAVAILABLE`.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

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

/// Check-script shell that runs `snippet` as the test (it logs the run and may move trunk)
/// and sets the check's `n` to the number of runs logged so far.
fn queued_box_relay(test_box: &TestBox, snippet: &str) -> String {
    let relayed = test_box.relay(&format!("exec 3>&1 >/dev/null\n{snippet}\necho \"$n\" >&3"));
    format!("n=$({relayed})\n")
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
    // The test logs the run and moves trunk while the check waits, and answers the run's
    // number: the check sees nothing of the box (G62).
    let logged = queued_box_relay(
        &test_box,
        &format!(
            "echo \"$tree\" >> '{runs}'; n=$(wc -l < '{runs}' | tr -d ' '); \
             if [ \"$n\" -le {moves} ]; then ( cd '{other}' && git pull -q --rebase && echo \"$n\" > docs/move-$n.md && git add docs && git -c user.name=t -c user.email=t@t commit -q -m \"move $n\" && git push -q origin main ); fi; \
             if [ {policy_at} -gt 0 ] && [ \"$n\" -eq {policy_at} ]; then ( cd '{other}' && git pull -q --rebase && sed 's/^revision = .*/revision = 2/' .bay/policy.toml > policy.new && mv policy.new .bay/policy.toml && git add .bay/policy.toml && git -c user.name=t -c user.email=t@t commit -q -m \"policy revision 2\" && git push -q origin main ); fi",
            runs = runs.display(),
            other = other.display(),
        ),
    );
    let before = format!(
        "{logged}if [ {fail_from} -gt 0 ] && [ \"$n\" -ge {fail_from} ]; then printf '%s' \"{{\\\"schema\\\":\\\"bay.check.result/1\\\",\\\"exercised\\\":{{\\\"commit\\\":\\\"$commit\\\",\\\"tree\\\":\\\"$tree\\\"}},\\\"plan\\\":{{\\\"selected\\\":1,\\\"completed\\\":1}},\\\"checks\\\":[{{\\\"id\\\":\\\"t\\\",\\\"verdict\\\":\\\"product_failure\\\"}}]}}\"; exit 0; fi"
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
    let (code, queued_result) = result_json(queued.test_box.bay(&view).arg("merge"));
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
        .bay_ok(&executor, &["runner", "run", "--once"]);
    let result = &first["detail"]["ran"][0];
    assert_eq!(result["decision"], "queued", "{first}");
    assert_eq!(result["reason"], "QUEUED");
    assert_eq!(
        queue_refs(&queued.test_box, &queued.remote),
        "refs/bay/queue/1"
    );

    let second = queued
        .test_box
        .bay_ok(&executor, &["runner", "run", "--once"]);
    assert_eq!(second["detail"]["ran"][0]["decision"], "merged", "{second}");
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
        .bay_ok(&executor, &["runner", "run", "--once"]);
    let before = runs(&queued);

    let (second, view) = land(&queued, "second");
    let (code, held) = result_json(queued.test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{held}");
    assert_eq!(held["decision"], "queued");
    assert!(held["reason"] == "QUEUED", "{held}");
    assert_eq!(held["detail"]["position"], 2, "{held}");
    assert_eq!(runs(&queued), before, "no check ran for the arrival");

    for _ in 0..3 {
        queued
            .test_box
            .bay_ok(&executor, &["runner", "run", "--once"]);
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
        .bay_ok(&executor, &["runner", "run", "--once"]);
    let failed = queued
        .test_box
        .bay_ok(&executor, &["runner", "run", "--once"]);
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
        .bay_ok(&executor, &["runner", "run", "--once"]);
    let killed = queued
        .test_box
        .bay(&executor)
        .env("BAY_TEST_KILL_AT", "after-publish")
        .args(["runner", "run", "--once"])
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
        .bay_ok(&executor, &["runner", "run", "--once"]);
    // A second change queues behind the head.
    let (second, view) = land(&queued, "second");
    let (code, _) = result_json(queued.test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0);
    // Its newer revision takes the same place.
    std::fs::write(view.join("src/second.rs"), "fn x() { newer }\n").unwrap();
    let (code, newer) = result_json(queued.test_box.bay(&view).args(["merge", "--wait"]));
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
        .bay_ok(&executor, &["runner", "run", "--once"]);
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
    test_box.add_origin(&repo, &host);
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
            .args(["--json", "runner", "run", "--once"])
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
/// (two `MAIN_MOVED` refusals before the queue, then one check as head) plus one per
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
    let mut files = check_files(
        &format!(
            "{}sleep 0.3",
            test_box.relay(&format!("echo \"$tree\" >> '{}'", runs.display()))
        ),
        &passing_result(1),
        1,
        60,
    );
    // No executor dies here, so the lease is the default one, not the short lease of the tests
    // that kill one: on a loaded box an executor's chain can stay still for seconds while it
    // publishes, and a short lease would let another executor take that operation over and
    // check it again, which is worker time this bound does not count.
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\nlease_s = 60\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let mut ids = Vec::new();
    for agent in 0..AGENTS {
        let file = format!("agent{agent}");
        let started = test_box.bay_ok(&repo, &["start", &file, "--paths", "src/"]);
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(format!("src/{file}.rs")), "fn x() {}\n").unwrap();
        let (code, submitted) = result_json(test_box.bay(&view).arg("merge"));
        assert_eq!(code, 0, "{submitted}");
        ids.push(started["change"].as_str().unwrap().to_owned());
    }
    let executors: Vec<PathBuf> = (0..EXECUTORS)
        .map(|n| {
            let clone = test_box.clone_of(&remote, &format!("executor-{n}"));
            test_box.git_ok(&clone, &["config", "bay.runnerName", &format!("exec-{n}")]);
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
        assert_eq!(change["operation"]["state"], "merged", "{change}");
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
            .bay_ok(&executor, &["runner", "run", "--once"]);
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
        .bay_ok(&executor, &["runner", "run", "--once"]);
    let result = ran["detail"]["ran"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["change"] == second.as_str())
        .cloned()
        .unwrap();
    assert_eq!(result["decision"], "merged", "{ran}");
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
        .bay_ok(&executor, &["runner", "run", "--once"]);
    assert_eq!(landed["detail"]["ran"][0]["decision"], "merged", "{landed}");
    let cut = queued
        .test_box
        .bay_ok(&executor, &["runner", "run", "--once"]);
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
        .bay_ok(&executor, &["runner", "run", "--once"]);
    let result = ran["detail"]["ran"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["change"] == second.as_str())
        .cloned()
        .unwrap();
    assert_eq!(result["decision"], "merged", "{ran}");
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
        .bay_ok(&executor, &["runner", "run", "--once"]);
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
        .bay_ok(&executor, &["runner", "run", "--once"]);
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
        .bay_ok(&executor, &["runner", "run", "--once"]);
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

/// [`check_files`] with the policy's one check in `environment` instead of `local`.
fn in_environment(files: Vec<(String, String)>, environment: &str) -> Vec<(String, String)> {
    files
        .into_iter()
        .map(|(path, text)| {
            let text = if path == ".bay/policy.toml" {
                text.replace(
                    "environment = \"local\"",
                    &format!("environment = \"{environment}\""),
                )
            } else {
                text
            };
            (path, text)
        })
        .collect()
}

/// Every note ref under `refs/notes/bay/evidence/` in `dir`, a work clone or the bare remote.
fn evidence_refs(test_box: &TestBox, dir: &Path) -> String {
    test_box.git_ok(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/notes/bay/evidence/",
        ],
    )
}

/// The lease, execution budget and publish window of the outage case, scaled down: a
/// progress deadline is `EXECUTION_BUDGET_S + PUBLISH_WINDOW_S` seconds.
const LEASE_S: u64 = 1;
const EXECUTION_BUDGET_S: u64 = 6;
const PUBLISH_WINDOW_S: u64 = 1;

/// The outage case's config: the scaled lease, and with `scaled_deadline` the scaled execution
/// budget and publish window. A check that runs to an answer (before and after the outage) runs
/// without the budget: on a loaded box it can take longer than the scaled budget, which would
/// end it `unknown`, never queued. In the outage the adapter answers `unavailable` at once, so
/// the scaled budget there only sets the progress deadline the outage must outlast.
fn outage_config(scaled_deadline: bool) -> String {
    let mut config = format!("[runner]\nlease_s = {LEASE_S}\nheartbeat_s = 1\ntakeover_s = 2\n");
    if scaled_deadline {
        use std::fmt::Write;
        let _ = write!(
            config,
            "execution_budget_s = {EXECUTION_BUDGET_S}\npublish_window_s = {PUBLISH_WINDOW_S}\n"
        );
    }
    config
}

/// How many places one operation can hold within `span` of an outage, by the rule of
/// `DESIGN` § 6: the place it held when the outage began, and one per rejoin, the first one
/// lease after the `unavailable` release and each later gap doubled, up to ten leases.
fn places_within(span: std::time::Duration) -> usize {
    let mut places = 1;
    let mut elapsed = 0;
    let mut gap = LEASE_S;
    // The passes are a second apart: allow one pass of slack per gap.
    while elapsed + gap <= span.as_secs() + places as u64 {
        elapsed += gap;
        places += 1;
        gap = (gap * 2).min(10 * LEASE_S);
    }
    places
}

/// A trunk whose check moves trunk from another clone on its first two runs, as `setup(2, 0)`
/// does, and whose adapter answers `unavailable` while `marker` exists (`INTERFACES` § 3),
/// under the scaled lease ([`outage_config`]). Returns it and the marker.
fn setup_outage() -> (Queued, PathBuf) {
    let test_box = TestBox::new();
    let marker = test_box.path().join("outage");
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let runs = test_box.path().join("runs");
    let other = test_box.path().join("mover");
    let before = format!(
        "if {outage}then printf '%s' '{{\"schema\":\"bay.check.result/1\",\"unavailable\":\"no worker answered\"}}'; exit 0; fi; \
         {logged}",
        outage = test_box.relay(&format!("[ -e '{}' ]", marker.display())),
        logged = queued_box_relay(
            &test_box,
            &format!(
                "echo \"$tree\" >> '{runs}'; n=$(wc -l < '{runs}' | tr -d ' '); \
                 if [ \"$n\" -le 2 ]; then ( cd '{other}' && git pull -q --rebase && echo \"$n\" > docs/move-$n.md && git add docs && git -c user.name=t -c user.email=t@t commit -q -m \"move $n\" && git push -q origin main ); fi",
                runs = runs.display(),
                other = other.display(),
            ),
        ),
    );
    let mut files = check_files(&before, &passing_result(1), 1, 60);
    files.push((".bay/config.toml".to_owned(), outage_config(false)));
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("docs/keep.md", "k\n")],
        &files,
    );
    test_box.clone_of(&remote, "mover");
    let queued = Queued {
        test_box,
        repo,
        remote,
        runs,
    };
    (queued, marker)
}

/// The queue refs on the remote, and the reason of each release among them.
fn release_reasons(queued: &Queued) -> (String, Vec<String>) {
    let refs = queue_refs(&queued.test_box, &queued.remote);
    let reasons = refs
        .lines()
        .filter(|name| name.contains("queue-done/"))
        .map(|name| {
            let release = queued
                .test_box
                .git_bare(&queued.remote, &["show", &format!("{name}:release.json")]);
            let release: serde_json::Value = serde_json::from_str(&release).unwrap();
            release["reason"].as_str().unwrap_or_default().to_owned()
        })
        .collect();
    (refs, reasons)
}

/// A two-hour worker outage with three queued operations and their agents gone ends with all
/// three landing after the workers return, none `QUEUE_DEADLINE_EXCEEDED` (`ROADMAP` G35). The
/// adapter answers `unavailable` while a marker file exists, so every head is released at once
/// with reason `unavailable` and waits outside the queue for a worker; the executor's passes
/// span more than three (scaled) progress deadlines. The queue grows by at most the backoff's
/// bound, and once the marker is gone each change lands exactly once (`DESIGN` § 6;
/// rulings-open-questions.md Q4). The scaled deadline lands on trunk just before the outage and
/// the unscaled config just before its end ([`outage_config`]); each executor pass reads the
/// config at the trunk it fetched.
#[test]
fn a_worker_outage_with_three_queued_operations_ends_with_all_three_landed() {
    let (queued, marker) = setup_outage();
    let ids: Vec<String> = ["first", "second", "third"]
        .iter()
        .map(|file| land(&queued, file).0)
        .collect();
    let executor = queued.test_box.clone_of(&queued.remote, "executor");
    queued
        .test_box
        .bay_ok(&executor, &["runner", "run", "--once"]);
    assert_eq!(
        queue_refs(&queued.test_box, &queued.remote),
        "refs/bay/queue/1\nrefs/bay/queue/2\nrefs/bay/queue/3",
        "all three hold a place before the outage"
    );
    assert_eq!(runs(&queued), 2);

    // The outage: the executor keeps making passes, its agents gone, under the scaled deadline.
    queued
        .test_box
        .commit_config(&queued.repo, &outage_config(true));
    std::fs::write(&marker, "down").unwrap();
    let mut looping = queued
        .test_box
        .bay_process(&executor)
        .args(["--json", "runner", "run", "--interval-s", "1"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Duration::from_secs(EXECUTION_BUDGET_S + PUBLISH_WINDOW_S);
    let span = deadline * 3 + std::time::Duration::from_secs(2);
    // The outage lasts at least three progress deadlines, and until each head was released
    // three times: as many releases as `QUEUE_DEADLINE_EXCEEDED` counts.
    let began = std::time::Instant::now();
    let (refs, reasons) = loop {
        let (refs, reasons) = release_reasons(&queued);
        let long_enough = began.elapsed() >= span && reasons.len() >= 9;
        if long_enough || began.elapsed() >= std::time::Duration::from_secs(300) {
            break (refs, reasons);
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    };
    let outage = began.elapsed();
    assert!(
        reasons.len() >= 9 && reasons.iter().all(|reason| reason == "unavailable"),
        "every head was released as unavailable, three times each, none by its deadline: {reasons:?}"
    );
    for id in &ids {
        let status = queued.test_box.bay_ok(&queued.repo, &["status", id]);
        let op = &status["detail"]["changes"][0]["operation"];
        assert_eq!(op["waiting_for"], "worker", "{status}");
    }
    let reservations = refs.lines().count() - reasons.len();
    assert!(
        reservations <= 3 * places_within(outage),
        "{reservations} reservations in {outage:?}: {refs}"
    );
    assert_eq!(runs(&queued), 2, "no check ran to an answer in the outage");

    // The workers return: further passes land all three, each once. A pass that began before
    // the unscaled config landed may meet trunk moved under its head's check: that head checks
    // again, a `MAIN_MOVED` retry, never a publish retry. The deadline only stops a stuck run.
    queued
        .test_box
        .commit_config(&queued.repo, &outage_config(false));
    std::fs::remove_file(&marker).unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(600);
    while queued.test_box.trunk_change_ids(&queued.remote).len() < 3
        && std::time::Instant::now() < until
    {
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let _ = looping.kill();
    let _ = looping.wait();
    let mut landed = queued.test_box.trunk_change_ids(&queued.remote);
    landed.sort();
    let mut expected = ids.clone();
    expected.sort();
    assert_eq!(landed, expected, "each Change-Id once on trunk");
    for id in &ids {
        let status = queued.test_box.bay_ok(&queued.repo, &["status", id]);
        let op = &status["detail"]["changes"][0]["operation"];
        assert_eq!(op["state"], "merged", "{status}");
        assert_eq!(op["merge_retries"], 0, "{status}");
    }
}

/// A worker with the wrong capabilities is not a substitute (`DESIGN` § 7, `ROADMAP` G35): the
/// box runs a required pair only when it declares that pair's environment
/// (`bay.checkEnvironment`). The policy's one check needs `env-b` and the box declares `env-a`:
/// `land --wait` waits with `CHECK_UNAVAILABLE` for a worker, runs nothing, writes no evidence
/// and moves no trunk. Once the box declares `env-b`, its executor lands the change under a
/// record that names `env-b` (rulings-open-questions.md Q4).
#[test]
fn a_worker_with_the_wrong_capabilities_is_not_a_substitute() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let runs_file = test_box.path().join("runs");
    let files = in_environment(
        check_files(
            &test_box.relay(&format!("echo \"$tree\" >> '{}'", runs_file.display())),
            &passing_result(1),
            1,
            60,
        ),
        "env-b",
    );
    let trunk = test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let declare = |environment: &str| {
        test_box.git_ok(
            test_box.path(),
            &[
                "config",
                "--global",
                "--add",
                "bay.checkEnvironment",
                environment,
            ],
        );
    };
    declare("env-a");
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, waiting) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{waiting}");
    assert_eq!(waiting["decision"], "queued", "{waiting}");
    assert_eq!(waiting["reason"], "CHECK_UNAVAILABLE", "{waiting}");
    assert_eq!(waiting["waiting_for"], "worker", "{waiting}");
    assert_eq!(
        waiting["detail"]["environments"],
        serde_json::json!(["env-b"]),
        "{waiting}"
    );
    assert!(
        !runs_file.exists(),
        "the adapter ran on a box without env-b"
    );
    assert_eq!(
        evidence_refs(&test_box, &repo),
        "",
        "no evidence was written"
    );
    assert_eq!(
        evidence_refs(&test_box, &remote),
        "",
        "no evidence was pushed"
    );
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), trunk);

    declare("env-b");
    let executor = test_box.clone_of(&remote, "executor");
    let ran = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["decision"], "merged", "{ran}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
    let tree = test_box.git_bare(&remote, &["rev-parse", "main^{tree}"]);
    let evidence: String = evidence_refs(&test_box, &remote)
        .lines()
        .map(|notes| {
            test_box.git_bare(
                &remote,
                &["notes", &format!("--ref={notes}"), "show", &tree],
            )
        })
        .collect();
    assert!(
        evidence.contains("\"environment\": \"env-b\""),
        "{evidence}"
    );
}

/// A policy change lands while the head checks: the head is released with `POLICY_CHANGED` and
/// rejoins at the back; the queued entries keep their positions and are rechecked under the new
/// policy when they become head; the release counts toward neither `QUEUE_DEADLINE_EXCEEDED`
/// nor `MERGE_RETRY_EXHAUSTED` (`DESIGN` § 6, `ROADMAP` G35).
#[test]
fn a_policy_landing_during_the_heads_check_releases_it_and_the_queue_keeps_its_order() {
    // Runs 1–2 move trunk, so the first change queues; its run 3 as head lands a policy edit.
    let queued = setup_with_policy_edit(2, 0, 3);
    let (first, _) = land(&queued, "first");
    let executor = queued.test_box.clone_of(&queued.remote, "executor");
    queued
        .test_box
        .bay_ok(&executor, &["runner", "run", "--once"]);
    let mut behind = Vec::new();
    for file in ["second", "third"] {
        let (id, view) = land(&queued, file);
        let (code, held) = result_json(queued.test_box.bay(&view).args(["merge", "--wait"]));
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
            .bay_ok(&executor, &["runner", "run", "--once"]);
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
    assert_eq!(op["merge_retries"], 0, "{status}");
    assert_eq!(op["state"], "merged", "{status}");
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

/// The trunk hook's time bound in the hook-timeout cases. An honest judgement takes about 1 s
/// on an idle box and several on a loaded one; every push to the host is judged within this
/// bound (the chain, the evidence, trunk), so it is well above that. A slowed push
/// (`BAY_TEST_HOOK_SLOW_PUSHES`) sleeps past the bound, whatever it is.
const HOOK_BOUND_S: &str = "10";

/// A bay-hosted, signed trunk whose trunk hook has a [`HOOK_BOUND_S`] time bound, and a change
/// ready to land. Returns the box, the host, the change id and its view.
fn hosted_with_hook_bound() -> (TestBox, PathBuf, String, PathBuf) {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    test_box.git_bare(&host, &["config", "bay.hookTimeoutS", HOOK_BOUND_S]);
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
            .args(["merge", "--wait"]),
    );
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert_eq!(test_box.trunk_change_ids(&host), vec![id.clone()]);
    let status = test_box.bay_ok(&view, &["status", &id]);
    let op = &status["detail"]["changes"][0]["operation"];
    assert_eq!(op["merge_retries"], 2, "{status}");
    assert_eq!(op["main_moved"], 0, "{status}");
    assert_eq!(op["attempts"], 1, "the same candidate: {status}");
}

/// A trunk hook that always times out: after five uncounted retries the operation is blocked
/// with `MERGE_RETRY_EXHAUSTED`, and trunk does not move (DESIGN § 6).
#[test]
fn five_hook_timeouts_end_in_publish_retry_exhausted() {
    let (test_box, host, id, view) = hosted_with_hook_bound();
    let trunk = test_box.git_bare(&host, &["rev-parse", "main"]);
    let (code, blocked) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_HOOK_SLOW_PUSHES", "100")
            .args(["merge", "--wait"]),
    );
    assert_eq!(code, 10, "{blocked}");
    assert_eq!(blocked["reason"], "MERGE_RETRY_EXHAUSTED", "{blocked}");
    assert_eq!(test_box.git_bare(&host, &["rev-parse", "main"]), trunk);
    let status = test_box.bay_ok(&view, &["status", &id]);
    let op = &status["detail"]["changes"][0]["operation"];
    assert_eq!(op["merge_retries"], 5, "{status}");
    assert_eq!(op["outcome"], "MERGE_RETRY_EXHAUSTED", "{status}");
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
        .bay_ok(&executor, &["runner", "run", "--once"]);
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
        .args(["--json", "runner", "run", "--once"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let paused = queued.test_box.path().join(format!("go-{seq}.paused"));
    // The head checks before it pauses; this deadline only stops a run that is stuck.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
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
/// refused; it rejoins the queue, the release counts toward `MERGE_RETRY_EXHAUSTED` and not
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
            .bay_ok(&executor, &["runner", "run", "--once"]);
    }
    assert_eq!(
        queued.test_box.trunk_change_ids(&queued.remote),
        vec![id.clone()]
    );
    let status = queued.test_box.bay_ok(&queued.repo, &["status", &id]);
    let op = &status["detail"]["changes"][0]["operation"];
    assert_eq!(op["merge_retries"], 1, "{status}");
}

/// The progress deadline releases the head while it publishes, five times in a row. Each
/// release rejoins the queue and counts as an uncounted publish retry, never toward
/// `QUEUE_DEADLINE_EXCEEDED`, so the third does not block it; the fifth ends the operation in
/// `MERGE_RETRY_EXHAUSTED`, and trunk never moves (`DESIGN` § 6, `INTERFACES` § 2).
#[test]
fn the_fifth_deadline_release_mid_publish_ends_in_publish_retry_exhausted() {
    let queued = setup(2, 0);
    let (id, _) = land(&queued, "first");
    let executor = queued.test_box.clone_of(&queued.remote, "executor");
    queued
        .test_box
        .bay_ok(&executor, &["runner", "run", "--once"]);
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
            assert_eq!(result["reason"], "QUEUED", "release {seq}: {ran}");
            assert_eq!(result["detail"]["seq"], seq + 1, "release {seq}: {ran}");
        } else {
            assert_eq!(result["reason"], "MERGE_RETRY_EXHAUSTED", "{ran}");
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
    assert_eq!(op["merge_retries"], 5, "{status}");
    assert_eq!(op["outcome"], "MERGE_RETRY_EXHAUSTED", "{status}");
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
        .bay_ok(&executor, &["runner", "run", "--once"]);
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
    assert_eq!(op["merge_retries"], 0, "{status}");
}
