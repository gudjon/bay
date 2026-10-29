//! G491 (owner ruling 2026-10-25, the talk uplift's security clause; bay 0.6.0). Talk is off
//! unless the policy on main says `[talk] enabled = true`, and the table's other keys narrow it.
//!
//! Off, `bay talk` refuses (`TALK_REFUSED`, `detail.talk_off`), an accept fetches and writes
//! nothing (`talk_accept.state` is `off`), and a bay host refuses a talk ref pushed around bay.
//! On, `kinds` (also `senders`, `recipients`) refuses a talk the table does not name, and
//! `wake = false` lets a talk arrive and wakes nobody.
//!
//! Guards to mutate: `src/policy.rs` `TalkPolicy::on` (always true); `src/talk/send.rs`
//! `allowed` in `open` (the answer) and in `deliver` (the kinds); `src/talk/accept.rs` the `off`
//! return and the `wakes` branch; `src/hook.rs` `Judge::talk`'s off check.

#![cfg(unix)]

use crate::support;

use support::talk::{TalkCase, from_now, hosted_talk_seeded};
use support::{TestBox, UNCHECKED_POLICY, result_json, unchecked_files};

/// A talk host whose trunk policy states `talk` (a `[talk]` table, or none).
fn host_with(talk: &str) -> TalkCase {
    let talk = talk.to_owned();
    hosted_talk_seeded(move |test_box, a| {
        test_box.seed_trunk(
            a,
            &[
                ("src/a.rs", "fn a() {}\n"),
                (".bay/policy.toml", &format!("{UNCHECKED_POLICY}{talk}")),
            ],
        );
    })
}

fn delivery(case: &TalkCase, follow_up: &str) -> (i32, serde_json::Value) {
    case.talk(
        "agent",
        &[
            "agent-b",
            "Review the parser change",
            "--kind",
            "delivery",
            "--change",
            "abcd2345",
            "--follow-up",
            follow_up,
        ],
    )
}

#[test]
fn off_main_refuses_the_send_the_accept_and_the_host() {
    let case = host_with("\n[talk]\nenabled = false\n");
    let (code, refused) = delivery(&case, &from_now(3600));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "TALK_REFUSED", "{refused}");
    assert_eq!(refused["detail"]["talk_off"], true, "{refused}");

    // An answer is refused for talk being off before any talk is looked up.
    let (code, refused) = case.talk(
        "agent-b",
        &["--answer", "abcdefghijklmnop", "--value", "yes"],
    );
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["detail"]["talk_off"], true, "{refused}");

    let (code, status) = case.status("agent-b");
    assert_eq!(code, 0, "{status}");
    assert_eq!(
        status["detail"]["talk_accept"]["state"], "off",
        "an accept writes nothing while talk is off: {status}"
    );

    let stderr = case.refused(&case.a, "HEAD:refs/bay/talk/agent-b/abcdefghijklmnop");
    assert!(
        stderr.contains("talk is off on this host"),
        "the host takes no talk ref while main turns talk off: {stderr}"
    );
}

/// A policy that states no `[talk]` table at all: a plain remote under `approvals` (G445), so no
/// key registry or shared talk seed (which turns talk on) is involved.
#[test]
fn a_policy_with_no_talk_table_is_off() {
    let test_box = TestBox::new();
    let remote = test_box.bare_any("remote.git");
    let a = test_box.repo_without_remote_any("a");
    test_box.add_origin(&a, &remote);
    let mut files = unchecked_files();
    files[0].1.push_str("\n[policy]\nsigning = \"approvals\"\n");
    test_box.seed_with_check(&a, &[("src/a.rs", "fn a() {}\n")], &files);
    let follow_up = from_now(3600);
    let (code, refused) = result_json(
        test_box
            .bay(&a)
            .env("BAY_ACTOR", "agent")
            .env("BAY_SESSION", "s-agent")
            .args([
                "talk",
                "agent-b",
                "Review the parser change",
                "--kind",
                "delivery",
                "--change",
                "abcd2345",
                "--follow-up",
                &follow_up,
            ]),
    );
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["detail"]["talk_off"], true, "{refused}");
}

#[test]
fn on_main_the_kinds_it_names_are_the_only_kinds() {
    let case = host_with("\n[talk]\nenabled = true\nkinds = [\"discovery\"]\n");
    let (code, refused) = delivery(&case, &from_now(3600));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "TALK_REFUSED", "{refused}");
    assert_eq!(refused["detail"]["talk_off"], false, "{refused}");
    assert!(
        refused["detail"]["why"]
            .as_str()
            .is_some_and(|why| why.contains("kinds")),
        "{refused}"
    );
}

#[test]
fn wake_false_lets_a_talk_arrive_and_wakes_nobody() {
    let case = host_with("\n[talk]\nenabled = true\nwake = false\n");
    let (code, sent) = delivery(&case, &from_now(3600));
    assert_eq!(code, 0, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();
    let (_, accepted) = case.status("agent-b");
    assert!(
        accepted["detail"]["talk_accept"]["acked"]
            .as_array()
            .is_some_and(|acked| acked.iter().any(|talk| talk == id.as_str())),
        "the talk arrives: {accepted}"
    );
    let (_, status) = case.status("agent");
    let row = status["detail"]["talks"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["talk"] == id.as_str()))
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        row["wake"]["reason"], "TALK_WAKE_OFF",
        "nothing is woken under wake = false: {status}"
    );
}
