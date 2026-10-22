//! G77. A talk addressed by change reaches its lander (`DESIGN` § 11c; `plans/wave2.md` G77,
//! corrected by the review dossier's K6).
//!
//! `bay talk --to-change <change-id|commit> <what> …` sends the talk to the actor that landed the
//! change. The recipient comes from bay's publication record only: the trunk commit must verify
//! as an executor-signed publication, and the actor is read from that change's record, never
//! from a trailer typed on a commit. A commit on trunk that is no bay publication is refused with
//! `OUTSIDE_MERGE`, naming the commit, and nothing is sent; a change or commit that is not
//! on trunk is `CHANGE_NOT_MERGED`. `--to-change` names the recipient, so a recipient given
//! beside it is refused.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use serde_json::Value;
use support::result_json;
use support::talk::{TalkCase, from_now, hosted_talk_landing, talk_ref};

/// Starts a change on `src/` as `agent`, writes `file`, and lands it. Returns (id, landing).
fn land(case: &TalkCase, intent: &str, file: &str) -> (String, String) {
    let (id, view) = start(case, intent);
    std::fs::write(view.join(file), "fn landed() {}\n").unwrap();
    let (code, landed) = result_json(
        case.test_box
            .bay(&view)
            .env("BAY_ACTOR", "agent")
            .env("BAY_SESSION", "s-agent")
            .args(["merge", "--wait"]),
    );
    assert_eq!(code, 0, "{landed}");
    (id, case.host_tip("refs/heads/main").unwrap())
}

fn start(case: &TalkCase, intent: &str) -> (String, PathBuf) {
    let (code, started) = result_json(
        case.bay_as("agent")
            .args(["start", intent, "--paths", "src/"]),
    );
    assert_eq!(code, 0, "{started}");
    (
        started["change"].as_str().unwrap().to_owned(),
        PathBuf::from(started["detail"]["view"].as_str().unwrap()),
    )
}

/// `bay talk --to-change <to> <what>` as `agent-c`, a delivery talk about `about`.
fn talk_to_change(case: &TalkCase, to: &str, what: &str, about: &str) -> (i32, Value) {
    let follow_up = from_now(3600);
    case.talk(
        "agent-c",
        &[
            "--to-change",
            to,
            what,
            "--kind",
            "delivery",
            "--change",
            about,
            "--follow-up",
            &follow_up,
        ],
    )
}

/// A commit on the host's trunk made outside bay: `landing`'s message with its actor trailer
/// typed as `actor`, on the trunk tip, never signed by the executor. The host's own update
/// stands for a forge that takes any push.
fn forge_on_trunk(case: &TalkCase, landing: &str, actor: &str) -> String {
    let message = case
        .test_box
        .git_bare(&case.host, &["show", "-s", "--format=%B", landing]);
    assert!(message.contains("\nActor: agent\n"), "{message}");
    let message = message.replace("\nActor: agent\n", &format!("\nActor: {actor}\n"));
    let tip = case.host_tip("refs/heads/main").unwrap();
    let tree = format!("{tip}^{{tree}}");
    let forged = case.test_box.git_bare(
        &case.host,
        &["commit-tree", &tree, "-p", &tip, "-m", &message],
    );
    case.test_box.git_bare(
        &case.host,
        &["update-ref", "refs/heads/main", &forged, &tip],
    );
    forged
}

fn talks_to(case: &TalkCase, who: &str) -> Vec<String> {
    case.test_box
        .git_bare(
            &case.host,
            &[
                "for-each-ref",
                "--format=%(refname)",
                &format!("refs/bay/talk/{who}/"),
            ],
        )
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_talk_to_a_change_or_its_commit_reaches_the_actor_that_landed_it() {
    let case = hosted_talk_landing();
    let (x, landing) = land(&case, "CARD-x", "src/x.rs");

    let (code, sent) = talk_to_change(&case, &x, "Fix the crash in x", &x);
    assert_eq!(code, 0, "{sent}");
    assert_eq!(sent["detail"]["who"], "agent", "{sent}");
    assert_eq!(sent["detail"]["to_change"]["change"], x.as_str(), "{sent}");
    assert_eq!(
        sent["detail"]["to_change"]["landing"],
        landing.as_str(),
        "{sent}"
    );
    let id = sent["detail"]["talk"].as_str().unwrap();
    assert!(case.host_tip(&talk_ref("agent", id)).is_some(), "{sent}");

    // The landing commit resolves to its change, and to the same lander.
    let (code, sent) = talk_to_change(&case, &landing, "Explain the timeout in x", &x);
    assert_eq!(code, 0, "{sent}");
    assert_eq!(sent["detail"]["who"], "agent", "{sent}");
    assert_eq!(sent["detail"]["to_change"]["change"], x.as_str(), "{sent}");
}

#[test]
fn a_typed_actor_trailer_outside_bay_never_routes_a_talk() {
    let case = hosted_talk_landing();
    let (x, landing) = land(&case, "CARD-x", "src/x.rs");
    // A commit outside bay claims x's change id and types another actor.
    let forged = forge_on_trunk(&case, &landing, "agent-b");

    let (code, refused) = talk_to_change(&case, &forged, "Fix the crash in x", &x);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "OUTSIDE_MERGE", "{refused}");
    assert_eq!(refused["detail"]["commit"], forged.as_str(), "{refused}");
    assert_eq!(talks_to(&case, "agent-b"), Vec::<String>::new());

    // The change id still reaches its lander, past the later commit that claims it.
    let (code, sent) = talk_to_change(&case, &x, "Fix the crash in x", &x);
    assert_eq!(code, 0, "{sent}");
    assert_eq!(sent["detail"]["who"], "agent", "{sent}");
    assert_eq!(
        sent["detail"]["to_change"]["landing"],
        landing.as_str(),
        "{sent}"
    );
    assert_eq!(talks_to(&case, "agent-b"), Vec::<String>::new());
}

#[test]
fn an_unlanded_change_or_commit_is_change_not_landed_and_a_second_recipient_is_refused() {
    let case = hosted_talk_landing();
    let (x, _) = land(&case, "CARD-x", "src/x.rs");
    let (open, _view) = start(&case, "CARD-open");

    let (code, refused) = talk_to_change(&case, &open, "Finish this", &open);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHANGE_NOT_MERGED", "{refused}");

    // A commit that is not on trunk.
    let off_trunk = case.test_box.git_ok(
        &case.c,
        &[
            "commit-tree",
            "refs/remotes/origin/main^{tree}",
            "-p",
            "refs/remotes/origin/main",
            "-m",
            "off trunk",
        ],
    );
    let (code, refused) = talk_to_change(&case, &off_trunk, "Finish this", &open);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHANGE_NOT_MERGED", "{refused}");

    // `--to-change` names the recipient: a recipient beside it is refused.
    let follow_up = from_now(3600);
    let (code, refused) = case.talk(
        "agent-c",
        &[
            "agent-b",
            "Fix the crash in x",
            "--to-change",
            &x,
            "--kind",
            "delivery",
            "--change",
            &x,
            "--follow-up",
            &follow_up,
        ],
    );
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "TALK_REFUSED", "{refused}");
    assert_eq!(talks_to(&case, "agent-b"), Vec::<String>::new());
}
