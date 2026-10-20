//! G57. Only agents send talks, and open talks are capped (`DESIGN` § 11c; ruling T10 in
//! `plans/wave2.md`).
//!
//! A talk is signed by the agent key of the registry actor it names as its sender: an executor-
//! or producer-signed talk is refused (`TALK_SENDER_NOT_ACTOR`), by the sender's bay and by the
//! host. One sender holds at most `BAY_TALK_OPEN_MAX` (20) open talks to one recipient: the 21st
//! is refused (`TALK_BACKLOG`) naming the oldest open id, by the sender's bay and by the host; a
//! closed or superseded talk does not count, and another sender's talks do not count, in the same
//! push or not. Hundreds of closed talks never lock a recipient out, and a retry the host refuses
//! leaves no local copy to count.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::{Value, json};
use support::talk::{TalkCase, from_now, hosted_talk, talk_ref};

fn ask(case: &TalkCase, actor: &str, what: &str, more: &[&str]) -> (i32, Value) {
    let follow_up = from_now(3600);
    let mut args = vec![
        "agent-b",
        what,
        "--kind",
        "delivery",
        "--artifact",
        "dist/report.txt",
        "--follow-up",
        &follow_up,
    ];
    args.extend(more);
    case.talk(actor, &args)
}

#[test]
fn an_executor_or_producer_signed_talk_is_refused() {
    let case = hosted_talk();
    // The sender's bay: an agent key that is not the sender's agent key in the registry.
    case.test_box.git_ok(
        &case.a,
        &[
            "config",
            "bay.agentKey",
            case.keys.executor.to_str().unwrap(),
        ],
    );
    let (code, result) = ask(&case, "agent", "Publish the report", &[]);
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "TALK_SENDER_NOT_ACTOR", "{result}");
    case.test_box.git_ok(
        &case.a,
        &["config", "bay.agentKey", case.keys.agent.to_str().unwrap()],
    );

    // The host: the same body signed by the executor or the producer key.
    let (code, dry) = ask(&case, "agent", "Publish the report", &["--dry-run"]);
    assert_eq!(code, 0, "{dry}");
    let body = dry["detail"]["body"].clone();
    let name = talk_ref("agent-b", body["id"].as_str().unwrap());
    let git = bay::git::Git::new(&case.a);
    for (key, principal) in [
        (&case.keys.executor, support::EXECUTOR),
        (&case.keys.producer, support::PRODUCER),
    ] {
        let commit = bay::talk::root_commit(&git, &body, key, principal).unwrap();
        let stderr = case.refused(&case.a, &format!("{commit}:{name}"));
        assert!(stderr.contains("TALK_SENDER_NOT_ACTOR"), "{stderr}");
    }
    // Signed by another agent's key while naming `agent` as its sender.
    let commit = bay::talk::root_commit(&git, &body, &case.c_key, "agent-c").unwrap();
    let stderr = case.refused(&case.a, &format!("{commit}:{name}"));
    assert!(stderr.contains("TALK_SENDER_NOT_ACTOR"), "{stderr}");
    assert_eq!(case.host_tip(&name), None);

    // The sender's own agent key sends it.
    let (code, sent) = ask(&case, "agent", "Publish the report", &[]);
    assert_eq!(code, 0, "{sent}");
}

#[test]
fn the_twenty_first_open_talk_to_one_recipient_is_refused_naming_the_oldest() {
    let case = hosted_talk();
    let mut ids = Vec::new();
    for n in 1..=19 {
        let (code, sent) = ask(&case, "agent", &format!("Publish report {n}"), &[]);
        assert_eq!(code, 0, "talk {n}: {sent}");
        ids.push(sent["detail"]["talk"].as_str().unwrap().to_owned());
        // `sent` is a commit time, to the second: the first two are each the oldest in turn.
        if n <= 2 {
            std::thread::sleep(std::time::Duration::from_millis(1100));
        }
    }
    // The 20th comes in one push with another sender's talk to the same recipient: only this
    // pair's creates count toward this pair's backlog.
    let git = bay::git::Git::new(&case.a);
    let root = |actor: &str, what: &str, key: &std::path::Path| {
        let (code, dry) = ask(&case, actor, what, &["--dry-run"]);
        assert_eq!(code, 0, "{dry}");
        let body = dry["detail"]["body"].clone();
        let commit = bay::talk::root_commit(&git, &body, key, actor).unwrap();
        let id = body["id"].as_str().unwrap().to_owned();
        (format!("{commit}:{}", talk_ref("agent-b", &id)), id)
    };
    let (twentieth, id) = root("agent", "Publish report 20", &case.keys.agent);
    let (theirs, _) = root("agent-c", "Publish report c", &case.c_key);
    case.test_box
        .git_ok(&case.a, &["push", "-q", "origin", &twentieth, &theirs]);
    ids.push(id);
    let (code, full) = ask(&case, "agent", "Publish report 21", &[]);
    assert_eq!(code, 10, "{full}");
    assert_eq!(full["reason"], "TALK_BACKLOG", "{full}");
    assert_eq!(full["detail"]["oldest"], ids[0].as_str(), "{full}");
    assert_eq!(full["detail"]["open"], 20, "{full}");
    assert!(
        full["detail"]["why"].as_str().unwrap().contains(&ids[0]),
        "{full}"
    );

    // The host refuses the 21st when the commit skips the sender's bay.
    let (code, dry) = ask(&case, "agent", "Publish report 21", &["--dry-run"]);
    assert_eq!(code, 10, "{dry}");
    let (code, other) = ask(&case, "agent-c", "Publish report 21", &["--dry-run"]);
    assert_eq!(code, 0, "{other}");
    let mut body = other["detail"]["body"].clone();
    body["from"] = json!({"name": "agent", "session": "s-agent"});
    body["id"] = json!(bay::talk::talk_id(&body).unwrap());
    let commit = bay::talk::root_commit(&git, &body, &case.keys.agent, "agent").unwrap();
    let name = talk_ref("agent-b", body["id"].as_str().unwrap());
    let stderr = case.refused(&case.a, &format!("{commit}:{name}"));
    assert!(stderr.contains("TALK_BACKLOG"), "{stderr}");
    assert!(stderr.contains(&ids[0]), "{stderr}");

    // Another sender's talks to the same recipient are not this pair's backlog.
    let (code, sent) = ask(&case, "agent-c", "Publish report 21", &[]);
    assert_eq!(code, 0, "{sent}");

    // A closed talk does not count: superseding the oldest closes it once agent-b accepts.
    let (code, replaced) = ask(
        &case,
        "agent",
        "Publish report 1, corrected",
        &["--supersedes", &ids[0]],
    );
    assert_eq!(code, 0, "{replaced}");
    // Superseded is replaced on the record (ruling T1): before agent-b closes it, it is no
    // longer an open ask, and the successor takes its place.
    let (code, early) = ask(&case, "agent", "Publish report 22", &[]);
    assert_eq!(code, 10, "{early}");
    assert_eq!(early["detail"]["open"], 20, "{early}");
    assert_eq!(early["detail"]["oldest"], ids[1].as_str(), "{early}");
    let (code, _) = case.status("agent-b");
    assert_eq!(code, 0);
    let (code, _) = case.status("agent");
    assert_eq!(code, 0);
    let (code, sent) = ask(&case, "agent", "Publish report 22", &[]);
    assert_eq!(code, 10, "{sent}");
    assert_eq!(sent["detail"]["oldest"], ids[1].as_str(), "{sent}");
}

/// Closed talks never lock a pair out of talk: with hundreds of closed talks to one recipient,
/// the sender's bay, the host and the recipient's accept still read the open ones (ruling T6:
/// compaction keeps the count small; a read cap must never stand in for it).
#[test]
fn many_closed_talks_never_lock_a_recipient_out() {
    let case = hosted_talk();
    let (code, first) = ask(&case, "agent", "Publish the first report", &[]);
    assert_eq!(code, 0, "{first}");
    let old = first["detail"]["talk"].as_str().unwrap().to_owned();
    let (code, second) = ask(
        &case,
        "agent",
        "Publish the first report, corrected",
        &["--supersedes", &old],
    );
    assert_eq!(code, 0, "{second}");
    let (code, status) = case.status("agent-b");
    assert_eq!(code, 0, "{status}");
    let closed = case.host_tip(&talk_ref("agent-b", &old)).unwrap();
    // 600 closed talks to agent-b on the host (written past the hook, as a long history is).
    let input = (0..600u16)
        .map(|n| {
            let [high, low] = n.to_be_bytes();
            let id = bay::ids::base32(&[0, 0, 0, 0, 0, 0, 0, 0, high, low]);
            format!("create {} {closed}", talk_ref("agent-b", &id))
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let mut planted = case
        .test_box
        .git(case.test_box.path())
        .arg(format!("--git-dir={}", case.host.display()))
        .args(["update-ref", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(planted.stdin.as_mut().unwrap(), input.as_bytes()).unwrap();
    assert!(planted.wait().unwrap().success());

    let (code, sent) = ask(&case, "agent", "Publish the second report", &[]);
    assert_eq!(code, 0, "{sent}");
    assert_eq!(sent["detail"]["pushed"], true, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();
    let (code, status) = case.status("agent-b");
    assert_eq!(code, 0, "{status}");
    assert_eq!(
        status["detail"]["talk_accept"]["state"], "accepted",
        "{status}"
    );
    assert_eq!(
        status["detail"]["talk_accept"]["acked"],
        json!([id.as_str()]),
        "{status}"
    );
    let (code, status) = case.status("agent");
    assert_eq!(code, 0, "{status}");
    assert_eq!(
        status["detail"]["talk_accept"]["state"], "accepted",
        "{status}"
    );
}

/// A talk committed here that the host then refuses does not stay here: it is not a talk, so it
/// is not counted, and a retry is not refused for ever by a copy only this machine holds.
#[test]
fn a_retry_the_host_refuses_leaves_no_local_talk() {
    let case = hosted_talk();
    let host = case.host.to_str().unwrap().to_owned();
    let follow_up = from_now(3600);
    let offline = |what: &str| {
        case.talk(
            "agent",
            &[
                "agent-b",
                what,
                "--kind",
                "delivery",
                "--artifact",
                "dist/report.txt",
                "--follow-up",
                &follow_up,
            ],
        )
    };
    let gone = case.test_box.path().join("unreachable.git");
    case.test_box.git_ok(
        &case.a,
        &["remote", "set-url", "origin", gone.to_str().unwrap()],
    );
    let (code, local) = offline("Publish the offline report");
    assert_eq!(code, 0, "{local}");
    assert_eq!(local["state"], "not_pushed", "{local}");
    let id = local["detail"]["talk"].as_str().unwrap().to_owned();
    let name = talk_ref("agent-b", &id);

    // Meanwhile the same actor, on another machine, fills its backlog to agent-b.
    let other = case.test_box.clone_of(&case.host, "a2");
    case.test_box.git_ok(
        &other,
        &["config", "bay.agentKey", case.keys.agent.to_str().unwrap()],
    );
    case.test_box.git_ok(
        &other,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/bay/registry:refs/bay/registry",
        ],
    );
    for n in 1..=20 {
        let mut cmd = case.test_box.bay(&other);
        cmd.env("BAY_ACTOR", "agent")
            .env("BAY_SESSION", "s-agent-2")
            .args([
                "talk",
                "agent-b",
                &format!("Publish report {n}"),
                "--kind",
                "delivery",
                "--artifact",
                "dist/report.txt",
                "--follow-up",
                &follow_up,
            ]);
        let (code, sent) = support::result_json(&mut cmd);
        assert_eq!(code, 0, "talk {n}: {sent}");
    }

    // Back online, the retry is refused by the host, and the local copy goes with it.
    case.test_box
        .git_ok(&case.a, &["remote", "set-url", "origin", &host]);
    let (code, retry) = offline("Publish the offline report");
    assert_eq!(code, 10, "{retry}");
    assert_eq!(retry["reason"], "TALK_BACKLOG", "{retry}");
    assert_eq!(case.local_tip(&case.a, &name), None, "{retry}");
    assert_eq!(case.host_tip(&name), None);
}
