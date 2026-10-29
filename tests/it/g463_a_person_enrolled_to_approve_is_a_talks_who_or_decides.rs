//! G463 (bay 0.6.0, T14; the owner's probe, 2026-10-28: the help said "a person's name works
//! too", and under `signing = "all"` only an agent could be `who` or `decides`). Under `all` a
//! person the registry enrols as an approver (`bay enrol approver`) may be a talk's `who` or
//! `decides`: the host takes the talk, the person answers it with their approver key (as they
//! approve), the host takes that answer, and the sender's bay counts it. A name the registry
//! enrols in no class is still refused. The help names the name grammar and who each signing
//! mode accepts.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/talk.rs` `check_actors`: drop the `KeyClass::Approver` arm (the talk to `owner` is
//!   refused at the sender and at the host).
//! - `src/talk/send.rs` `answerer`: the approver key only on a talk that asks a person (the
//!   person's answer to `owner`'s talk is signed by no key it holds, and refused).
//! - `src/talk/judge.rs` `append`: the approver classes only on a talk that asks a person (the
//!   host refuses the person's answer).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::plain_talk::TALK_ON;
use support::talk::{TalkCase, fresh_key, from_now, hosted_talk_seeded, talk_ref};
use support::{AGENT, EXECUTOR, PRODUCER, REGISTRAR, UNCHECKED_POLICY, key_line, result_json};

/// The talk host, talk on, with `owner` enrolled as an approver; `owner`'s approver key set on
/// `c` beside `agent-c`'s agent key.
fn hosted_with_owner() -> TalkCase {
    let case = hosted_talk_seeded(|test_box, a| {
        test_box.seed_with_check(
            a,
            &[("src/a.rs", "fn a() {}\n")],
            &[(
                ".bay/policy.toml".to_owned(),
                format!("{UNCHECKED_POLICY}{TALK_ON}"),
            )],
        );
    });
    let owner = fresh_key(&case.test_box, "owner");
    let agents = format!(
        "{}{}{}",
        key_line(AGENT, &case.keys.agent),
        key_line("agent-b", &case.b_key),
        key_line("agent-c", &case.c_key)
    );
    let files = [
        ("agent", agents),
        ("runner", key_line(EXECUTOR, &case.keys.executor)),
        ("producer", key_line(PRODUCER, &case.keys.producer)),
        ("approver", key_line("owner", &owner)),
        ("registry", key_line(REGISTRAR, &case.keys.registry)),
    ];
    case.test_box.enroll_files(&case.a, &case.keys, &files);
    for (key, value) in [
        ("bay.approverName", "owner"),
        ("bay.approverKey", owner.to_str().unwrap()),
    ] {
        case.test_box.git_ok(&case.c, &["config", key, value]);
    }
    for clone in [&case.b, &case.c] {
        case.test_box.git_ok(
            clone,
            &[
                "fetch",
                "-q",
                "origin",
                "+refs/bay/registry:refs/bay/registry",
            ],
        );
    }
    case
}

/// `bay talk <args>` on `c` as the person `owner`.
fn as_owner(case: &TalkCase, args: &[&str]) -> (i32, Value) {
    result_json(
        case.test_box
            .bay(&case.c)
            .env("BAY_ACTOR", "owner")
            .env("BAY_SESSION", "s-owner")
            .arg("talk")
            .args(args),
    )
}

/// The answer the sender's bay counts on talk `id` to `who`, after a fetch of it.
fn counted(case: &TalkCase, who: &str, id: &str) -> Option<String> {
    let name = talk_ref(who, id);
    case.test_box.git_ok(
        &case.a,
        &["fetch", "-q", "origin", &format!("+{name}:{name}")],
    );
    let git = bay::git::Git::new(&case.a);
    let tip = git.run(&["rev-parse", &name]).unwrap();
    let talk = bay::talk::Talk::read(&git, &name, &tip).unwrap();
    let registry = bay::registry::Registry::current(&git).unwrap().unwrap();
    bay::talk::verified_answer(&git, &bay::talk::Trust::Keys(registry), &talk)
}

#[test]
fn an_enrolled_approver_is_who_or_decides_and_answers_with_the_approver_key() {
    let case = hosted_with_owner();
    let follow_up = from_now(3600);
    let discovery = |who: &str, what: &str, decision: &str, more: &[&str]| {
        let mut args = vec![
            who,
            what,
            "--kind",
            "discovery",
            "--decision",
            decision,
            "--follow-up",
            follow_up.as_str(),
        ];
        args.extend(more);
        case.talk("agent", &args)
    };
    let (code, to_owner) = discovery("owner", "Approve the field name", "field", &[]);
    assert_eq!(code, 0, "{to_owner}");
    assert_eq!(to_owner["state"], "sent", "{to_owner}");
    let to_owner = to_owner["detail"]["talk"].as_str().unwrap().to_owned();
    assert!(case.host_tip(&talk_ref("owner", &to_owner)).is_some());

    let (code, decided) = discovery(
        "agent-b",
        "Pick the field name",
        "name",
        &["--decides", "owner"],
    );
    assert_eq!(code, 0, "{decided}");
    assert_eq!(decided["state"], "sent", "{decided}");
    let decided = decided["detail"]["talk"].as_str().unwrap().to_owned();

    // The person answers each with their approver key, and the host takes it.
    let (code, answered) = as_owner(&case, &["--answer", &to_owner, "--value", "yes"]);
    assert_eq!(code, 0, "{answered}");
    assert_eq!(answered["state"], "answered", "{answered}");
    assert_eq!(answered["detail"]["pushed"], true, "{answered}");
    assert_eq!(counted(&case, "owner", &to_owner).as_deref(), Some("yes"));
    let (code, answered) = as_owner(&case, &["--answer", &decided, "--value", "name"]);
    assert_eq!(code, 0, "{answered}");
    assert_eq!(answered["detail"]["pushed"], true, "{answered}");
    assert_eq!(counted(&case, "agent-b", &decided).as_deref(), Some("name"));

    // A name the registry enrols in no class is refused, as before.
    let (code, nobody) = discovery("nobody", "Approve it", "it", &[]);
    assert_eq!(code, 10, "{nobody}");
    assert_eq!(nobody["reason"], "TALK_REFUSED", "{nobody}");
    let why = nobody["detail"]["why"].as_str().unwrap();
    assert!(why.contains("nobody"), "{nobody}");
}

#[test]
fn the_help_names_the_name_grammar_and_who_each_mode_accepts() {
    let case = hosted_with_owner();
    let out = case
        .test_box
        .bay(&case.a)
        .args(["talk", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    for words in ["lower-case letters, digits and -", "approvals", "approver"] {
        assert!(help.contains(words), "the help lacks {words:?}:\n{help}");
    }
}
