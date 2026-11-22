//! G855 (a fleet finding, 2026-11-22: on 0.6.137 one `bay queue --deadline 0` took 1 s and 442 MB
//! where 0.6.136 took 0.2 s and 14 MB). The line read the request times of its arriving and taken
//! changes with `git show -s`, and `git show` runs its diff on each commit, rename detection too,
//! even with `-s`. A request commit's tree differs from its parent's in every path the change
//! touches, so each one cost about 0.4 s. The line now reads those times in one `cat-file --batch`,
//! which runs no diff: a read of the line runs no `git show`, and still gives each row its time.
//!
//! A lease's beat time is read from its commit object in the same way.
//!
//! Guards to mutate, each alone: `src/line.rs` `commit_times`: read the times with
//! `git show -s --format=%H %ct` again; `src/lease.rs` `beat_at`: read the time with
//! `git show -s --format=%ct` again. Each time the log holds the `show` call, and this test fails.

use crate::support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use support::PRODUCER;
use support::queued::{Check, finished, host};

#[test]
fn a_read_of_the_line_reads_request_times_with_no_diff() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n"), ("docs/a.md", "a\n")],
        hold_first: true,
        fails: "false",
    });
    let observer = held.test_box.clone_of(&held.remote, "observer");

    // GIVEN a change a runner checks under its lease, with no place in the line
    let started = held
        .test_box
        .bay_ok(&held.repo, &["start", "lone", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/lone.rs"), "fn lone() {}\n").unwrap();
    let submitted = held.test_box.bay_ok(&view, &["merge"]);
    assert_eq!(submitted["decision"], "queued", "{submitted}");
    let runner = held.executor("exec-a", PRODUCER);
    let pass = held.spawn(&runner);
    held.wait_held();
    let lease = format!("refs/bay/lease/{id}");
    let start = Instant::now();
    while held
        .test_box
        .git_bare(
            &held.remote,
            &["for-each-ref", "--format=%(refname)", &lease],
        )
        .trim()
        .is_empty()
    {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the runner never beat its lease of {id}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // AND a second change that arrives while the runner checks the first
    let next = held
        .test_box
        .bay_ok(&held.repo, &["start", "next", "--paths", "docs/"]);
    let next_id = next["change"].as_str().unwrap().to_owned();
    let next_view = PathBuf::from(next["detail"]["view"].as_str().unwrap());
    std::fs::write(next_view.join("docs/next.md"), "next\n").unwrap();
    let submitted = held.test_box.bay_ok(&next_view, &["merge"]);
    assert_eq!(submitted["decision"], "queued", "{submitted}");

    // AND another clone whose status swept the remote, where every `git show` is logged (a notes
    // read is not a commit's diff)
    let _ = held.test_box.bay_ok(&observer, &["status"]);
    support::sweeps_ended(&observer);
    let shows = held.test_box.path().join("show-calls");
    held.test_box.wrap_git(&format!(
        "case \" $* \" in\n  *\" notes \"*) ;;\n  *\" show \"*) echo \"$*\" >> '{}' ;;\nesac",
        shows.display()
    ));

    // WHEN that clone reads the line from its store, then from the remote
    let stored = held
        .test_box
        .bay_ok(&observer, &["queue", "--deadline", "0"]);
    let line = held.test_box.bay_ok(&observer, &["queue"]);
    support::sweeps_ended(&observer);

    // THEN each read gives the taken change and the arriving change their request times
    for read in [&line, &stored] {
        let detail = &read["detail"];
        assert_eq!(detail["taken"][0]["change"], id.as_str(), "{read}");
        assert!(detail["taken"][0]["requested_at"].is_string(), "{read}");
        assert_eq!(detail["arriving"][0]["change"], next_id.as_str(), "{read}");
        assert!(detail["arriving"][0]["requested_at"].is_string(), "{read}");
    }
    assert_eq!(stored["detail"]["from_store"], true, "{stored}");

    // AND neither read ran `git show` in that clone
    let observer_name = observer.display().to_string();
    let logged = std::fs::read_to_string(&shows).unwrap_or_default();
    let ran: Vec<&str> = logged
        .lines()
        .filter(|call| call.contains(&observer_name))
        .collect();
    assert!(ran.is_empty(), "a read of the line ran git show: {ran:?}");

    held.go();
    let ran = finished(pass);
    assert_eq!(ran["detail"]["ran"][0]["decision"], "merged", "{ran}");
}
