//! G597 (2026-10-31): the retention cut of an ended change keeps none of its operations, while
//! the request ref the change answered stays on the remote. `bay queue` named every such change
//! unread ("its merge request is not in its record here"), 46 of them on one repository, though
//! nothing waited. An ended change waits for nothing: a landed one by its own record here, an
//! abandoned one by the remote's record, as adoption can open it again.
//!
//! A landed change, then an abandoned one, each cut by retention with its request ref still on
//! the remote: `bay queue` from another clone names neither as arriving or unread.
//!
//! G599 (2026-10-31): the cut of a change merged from one clone and abandoned before a runner
//! ran it kept no land request, as the runner read it from a requests ref it never fetches. The
//! host refused that cut, and every runner pass tried it again. The abandoned half asks for it.
//!
//! Guards to mutate: `src/line.rs`, `requests`: drop the `retention_cut` arm (each change is
//! named unread); `src/publish.rs`, `abandoned_keeps`: drop the land request (the abandoned
//! change is never cut).

use crate::support;

use std::path::PathBuf;

use serde_json::{Value, json};
use support::PRODUCER;
use support::queued::{Check, Held, host};

/// The remote's record of `change`.
fn remote_record(held: &Held, change: &str) -> Value {
    let text = held.test_box.git_bare(
        &held.remote,
        &["show", &format!("refs/bay/changes/{change}:change.json")],
    );
    serde_json::from_str(&text).unwrap()
}

/// Runner passes in `runner` until the remote's record of `change` is cut by retention.
fn cut(held: &Held, runner: &std::path::Path, change: &str) {
    for _ in 0..6 {
        let _ = held.pass(runner);
        if remote_record(held, change)["retention_cut"] == true {
            return;
        }
    }
    panic!("{change} was never cut: {}", remote_record(held, change));
}

/// Asserts that the remote still holds the request ref of `change`.
fn request_kept(held: &Held, change: &str) {
    let refs = held.test_box.git_bare(
        &held.remote,
        &["for-each-ref", "--format=%(refname)", "refs/bay/requests/"],
    );
    assert!(
        refs.lines()
            .any(|name| name == format!("refs/bay/requests/{change}")),
        "{refs}"
    );
}

/// Asserts that `bay queue` from a new clone shows nothing waiting and nothing unread.
fn nothing_waits(held: &Held, name: &str) {
    let observer = held.test_box.clone_of(&held.remote, name);
    let line = held.test_box.bay_ok(&observer, &["queue"]);
    assert_eq!(line["detail"]["depth"], 0, "{line}");
    assert_eq!(line["detail"]["arriving"], json!([]), "{line}");
    assert_eq!(line["detail"]["unread"], json!([]), "{line}");
}

#[test]
fn bay_queue_names_no_ended_change_unread_after_its_retention_cut() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\n\n[retention]\nwindow_s = 0\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let runner = held.executor("exec-a", PRODUCER);

    // GIVEN a change that landed and was cut by retention, its request ref still on the
    // remote WHEN another clone reads the line THEN nothing waits and nothing is unread.
    let landed = held.queue("one", "src");
    cut(&held, &runner, &landed);
    assert_eq!(remote_record(&held, &landed)["state"], "landed");
    request_kept(&held, &landed);
    nothing_waits(&held, "observer-landed");

    // GIVEN a submitted change that was abandoned and cut by retention WHEN another clone
    // reads the line THEN nothing waits and nothing is unread.
    let started = held
        .test_box
        .bay_ok(&held.repo, &["start", "dropped", "--paths", "src/"]);
    let dropped = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/dropped.rs"), "fn dropped() {}\n").unwrap();
    let submitted = held.test_box.bay_ok(&view, &["merge"]);
    assert_eq!(submitted["decision"], "queued", "{submitted}");
    let abandoned = held.test_box.bay_ok(&view, &["abandon"]);
    assert_eq!(abandoned["decision"], "ok", "{abandoned}");
    cut(&held, &runner, &dropped);
    assert_eq!(remote_record(&held, &dropped)["state"], "abandoned");
    request_kept(&held, &dropped);
    nothing_waits(&held, "observer-abandoned");
}
