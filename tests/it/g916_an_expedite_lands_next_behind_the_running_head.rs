//! BACKLOG 326 (G916): `bay merge --expedite <why>` puts a change next, right behind the head
//! that runs now, past the changes that wait behind it, within the repository's `[expedite]`
//! limits. A runner grants it, and its place says so. An ask bay cannot grant leaves the change in
//! line as a plain arrival, and the result says why (`EXPEDITE_REFUSED`).
//!
//! Guards to mutate: in `src/publish.rs`, the grant in `expedite` (the hotfix lands after
//! `later`); in `src/expedite.rs`, the `Off` arm of `refusal` (the ask with no `[expedite]`
//! is not refused); in `src/hook.rs` `expedite_grant`, return `Ok` at once (the host takes an
//! expedite place that no ask gives).

#![cfg(unix)]

use crate::support;

use serde_json::json;

use support::queued::{Check, Held, OTHER_PRODUCER, entry_commit, finished, host};
use support::{AGENT, EXECUTOR, PRODUCER};

const CONFIG: &str =
    "[runner]\nlease_s = 30\nheartbeat_s = 1\nahead = 4\n\n[expedite]\nper_day = 2\n";

/// Where change `id` landed on main, the oldest landing first.
fn landed_at(held: &Held, id: &str) -> Option<usize> {
    let mut merged = held.test_box.trunk_change_ids(&held.remote);
    merged.reverse();
    merged.iter().position(|change| change == id)
}

#[test]
fn an_expedite_lands_before_the_changes_that_waited_behind_the_head() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        fails: "false",
    });
    let slow = held.queue("slow", "src");
    let later = held.queue("later", "src");
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    let runner_a = held.spawn(&a);
    held.wait_held();
    let (hotfix, code, asked) = held.expedite("hotfix", "src", "main's users cannot build");
    assert_eq!(code, 0, "{asked}");
    assert!(asked["reason"].is_null(), "{asked}");
    assert!(asked["detail"]["expedite"]["refused"].is_null(), "{asked}");
    assert_eq!(
        asked["detail"]["expedite"]["why"],
        "main's users cannot build"
    );
    let pass = held.pass(&b);
    let opened = bay::repo::Repo::open(&held.repo).unwrap();
    let queue = bay::queue::Queue::read(&opened).unwrap();
    let granted = queue
        .entries
        .values()
        .find(|entry| entry.change == hotfix)
        .unwrap_or_else(|| panic!("the expedite holds no place: {pass}"));
    assert!(granted.expedite, "the place is not an expedite: {pass}");
    held.go();
    let _ = finished(runner_a);
    for _ in 0..4 {
        if landed_at(&held, &later).is_some() {
            break;
        }
        let _ = held.pass(&b);
    }
    let (slow, hotfix, later) = (
        landed_at(&held, &slow),
        landed_at(&held, &hotfix),
        landed_at(&held, &later),
    );
    assert!(
        slow.is_some() && hotfix.is_some() && later.is_some(),
        "{slow:?} {hotfix:?} {later:?}"
    );
    assert!(
        hotfix < later,
        "the expedite landed after the change behind the head"
    );
}

#[test]
fn an_expedite_the_repository_does_not_grant_waits_its_turn_and_says_why() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let _first = held.queue("first", "src");
    let (hotfix, _, asked) = held.expedite("hotfix", "src", "a hotfix");
    assert_eq!(asked["decision"], "queued", "{asked}");
    assert_eq!(asked["reason"], "EXPEDITE_REFUSED", "{asked}");
    assert_eq!(asked["detail"]["expedite"]["refused"], "off", "{asked}");
    let b = held.executor("exec-b", OTHER_PRODUCER);
    let _ = held.pass(&b);
    let _ = held.pass(&b);
    assert!(
        landed_at(&held, &hotfix).is_some(),
        "the refused ask never landed as a plain arrival"
    );
}

#[test]
fn the_host_refuses_an_expedite_place_no_ask_gives() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    // The runner's own key signs every record here, so only the place can be refused.
    let runner = bay::signing::Signer {
        key: held.keys.executor.clone(),
        name: EXECUTOR.to_owned(),
    };
    // GIVEN `one` at the head of the line, `two` behind it, and `hotfix` asking an expedite
    let _one = held.queue("one", "src");
    let two = held.queue("two", "docs");
    let (hotfix, code, asked) = held.expedite("hotfix", "src", "a hotfix");
    assert_eq!(code, 0, "{asked}");
    let opened = bay::repo::Repo::open(&held.repo).unwrap();
    let queue = bay::queue::Queue::read(&opened).unwrap();
    let front = queue.head().cloned().unwrap();
    let next = queue.entries.keys().max().map_or(1, |seq| seq + 1);
    let request = |change: &str| {
        opened
            .git
            .run(&["rev-parse", &format!("refs/bay/requests/{change}")])
            .unwrap()
    };
    let place = |change: &str, rank: u64| {
        json!({
            "op_id": format!("{change}.1"),
            "change": change,
            "rank": rank,
            "after": front.seq,
            "expedite": bay::expedite::grant_entry("a hotfix", AGENT, &request(change), EXECUTOR),
        })
    };
    let push = |entry: &serde_json::Value| {
        let commit = entry_commit(&held.test_box, &held.repo, &runner, entry);
        let out = held
            .test_box
            .git(&held.repo)
            .args([
                "push",
                "-q",
                "origin",
                &format!("{commit}:refs/bay/queue/{next}"),
            ])
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    // WHEN a runner pushes an expedite place for `two`, whose request asked none
    let (took, said) = push(&place(&two, front.rank));
    // THEN the host refuses it
    assert!(!took, "an expedite place no request asked was taken");
    assert!(
        said.contains("names the request of its own operation"),
        "{said}"
    );

    // AND it refuses `hotfix`'s place anywhere but right behind the head
    let (took, said) = push(&place(&hotfix, front.rank + 1));
    assert!(!took, "an expedite place off the head was taken");
    assert!(said.contains("stands right behind the head"), "{said}");

    // AND it takes `hotfix`'s place right behind the head
    let (took, said) = push(&place(&hotfix, front.rank));
    assert!(took, "{said}");
}
