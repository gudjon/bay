//! G48. A talk refuses a bad ask, and the words stay distinct (`DESIGN` § 11c; ruling R9 and
//! BACKLOG 9b in `plans/wave2.md`).
//!
//! Built so far: a missing `what` or `who` is refused (a missing kind or `follow_up` is filled
//! in since 0.6.0, G460); a `follow_up` not after
//! `sent` is refused; a bundled ask (two target fields, or a list in one) is refused by
//! structure alone; a kind other than `delivery`, `discovery` or `reliability`, and a target
//! field another kind owns, are refused; a body over `BAY_TALK_MAX_BYTES` is refused naming that
//! bay limit; CRLF folds to LF and any other control character, a bare CR among them, is
//! refused; a `decides` that is not a registry actor is refused. Each is `TALK_REFUSED`, and
//! nothing is committed. The host refuses the same shapes when a commit skips the sender's bay.
//! `owns`, `promises`, `needs` and `promise_evidence` are fields on the talk; a talk names at
//! most one of `target`, `forecast` and `commitment`, and `done` or `ready` labels neither a
//! target nor a forecast. The hook-silence clause comes with the talk rows of `bay status`.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::{Value, json};
use support::talk::{TalkCase, from_now, hosted_talk, talk_ref};

fn refused(case: &TalkCase, args: &[&str]) -> Value {
    let (code, result) = case.talk("agent", args);
    assert_eq!(code, 10, "{args:?}: {result}");
    assert_eq!(result["decision"], "blocked", "{result}");
    assert_eq!(result["reason"], "TALK_REFUSED", "{args:?}: {result}");
    assert_eq!(result["allowed"], json!(["read", "repair"]), "{result}");
    result
}

fn nothing_committed(case: &TalkCase) {
    let refs = case.test_box.git_ok(
        &case.a,
        &["for-each-ref", "--format=%(refname)", "refs/bay/talk/"],
    );
    assert_eq!(refs, "");
}

#[test]
fn missing_fields_and_a_follow_up_not_after_sent_are_refused() {
    let case = hosted_talk();
    let later = from_now(3600);
    let base = ["--kind", "delivery", "--change", "abcd2345"];
    let with = |head: &[&'static str], tail: &[&str]| -> Vec<String> {
        head.iter()
            .chain(base.iter())
            .chain(tail.iter())
            .map(|arg| (*arg).to_owned())
            .collect()
    };
    for (args, field) in [
        (with(&["agent-b"], &["--follow-up", &later]), "what"),
        (with(&[], &["--follow-up", &later]), "who"),
    ] {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = refused(&case, &args);
        assert!(
            result["detail"]["why"].as_str().unwrap().contains(field),
            "{field}: {result}"
        );
    }
    for past in [from_now(0), "2020-01-01T00:00:00Z".to_owned()] {
        let args = with(&["agent-b", "Ship it"], &["--follow-up", &past]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = refused(&case, &args);
        assert!(
            result["detail"]["why"].as_str().unwrap().contains("after"),
            "{result}"
        );
    }
    let args = with(&["agent-b", "Ship it"], &["--follow-up", "tomorrow"]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    refused(&case, &args);
    nothing_committed(&case);
}

#[test]
fn a_bundled_ask_and_a_bad_kind_are_refused_by_structure() {
    let case = hosted_talk();
    let later = from_now(3600);
    let two_targets = refused(
        &case,
        &[
            "agent-b",
            "Ship it",
            "--kind",
            "delivery",
            "--change",
            "abcd2345",
            "--artifact",
            "dist/a.tar",
            "--follow-up",
            &later,
        ],
    );
    assert!(
        two_targets["detail"]["why"]
            .as_str()
            .unwrap()
            .contains("one target"),
        "{two_targets}"
    );
    let a_list = refused(
        &case,
        &[
            "agent-b",
            "Ship it",
            "--kind",
            "delivery",
            "--change",
            "abcd2345",
            "--change",
            "wxyz6723",
            "--follow-up",
            &later,
        ],
    );
    assert!(
        a_list["detail"]["why"].as_str().unwrap().contains("list"),
        "{a_list}"
    );
    refused(
        &case,
        &[
            "agent-b",
            "Ship it",
            "--kind",
            "chat",
            "--change",
            "abcd2345",
            "--follow-up",
            &later,
        ],
    );
    // A target field another kind owns.
    refused(
        &case,
        &[
            "agent-b",
            "Pick one",
            "--kind",
            "discovery",
            "--change",
            "abcd2345",
            "--follow-up",
            &later,
        ],
    );
    refused(
        &case,
        &[
            "agent-b",
            "Ship it",
            "--kind",
            "delivery",
            "--follow-up",
            &later,
        ],
    );
    nothing_committed(&case);
}

#[test]
fn size_and_control_characters_are_bay_limits() {
    let case = hosted_talk();
    let later = from_now(3600);
    let talk = |what: &str| -> (i32, Value) {
        case.talk(
            "agent",
            &[
                "agent-b",
                what,
                "--kind",
                "delivery",
                "--change",
                "abcd2345",
                "--follow-up",
                &later,
                "--dry-run",
            ],
        )
    };
    let oversize = "a".repeat(9000);
    let (code, result) = talk(&oversize);
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "TALK_REFUSED");
    assert_eq!(result["detail"]["limit"], "BAY_TALK_MAX_BYTES", "{result}");
    assert_eq!(result["detail"]["max_bytes"], 8192, "{result}");
    assert!(
        result["detail"]["why"]
            .as_str()
            .unwrap()
            .contains("BAY_TALK_MAX_BYTES (8192"),
        "{result}"
    );

    // CRLF folds to LF; newline and tab stay.
    let (code, folded) = talk("Ship it\r\nthen\ttell me");
    assert_eq!(code, 0, "{folded}");
    assert_eq!(folded["detail"]["dry_run"], true);
    assert_eq!(folded["detail"]["body"]["what"], "Ship it\nthen\ttell me");

    // A bare CR, or any other control character, is refused.
    for bad in ["Ship it\rnow", "Ship it\u{1b}[2J", "Ship\u{7}it"] {
        let (code, result) = talk(bad);
        assert_eq!(code, 10, "{bad:?}: {result}");
        assert_eq!(result["reason"], "TALK_REFUSED", "{result}");
        assert!(
            result["detail"]["why"]
                .as_str()
                .unwrap()
                .contains("control character"),
            "{result}"
        );
    }
    let (code, result) = case.talk(
        "agent",
        &[
            "agent-b",
            "Ship it",
            "--kind",
            "delivery",
            "--change",
            "abcd2345",
            "--follow-up",
            &later,
            "--context",
            "see\rhere",
        ],
    );
    assert_eq!(code, 10, "{result}");
    nothing_committed(&case);
}

#[test]
fn decides_must_be_a_registry_actor() {
    let case = hosted_talk();
    let later = from_now(3600);
    let result = refused(
        &case,
        &[
            "agent-b",
            "Pick a format",
            "--kind",
            "discovery",
            "--decision",
            "wire-format",
            "--follow-up",
            &later,
            "--decides",
            "nobody",
        ],
    );
    assert!(result["detail"]["why"].as_str().unwrap().contains("nobody"));
    let (code, ok) = case.talk(
        "agent",
        &[
            "agent-b",
            "Pick a format",
            "--kind",
            "discovery",
            "--decision",
            "wire-format",
            "--follow-up",
            &later,
            "--decides",
            "agent-c",
            "--dry-run",
        ],
    );
    assert_eq!(code, 0, "{ok}");
    assert_eq!(ok["detail"]["body"]["decides"], "agent-c");
    nothing_committed(&case);
}

#[test]
fn the_host_refuses_a_bad_ask_that_skips_the_senders_bay() {
    let case = hosted_talk();
    let later = from_now(3600);
    let (code, dry) = case.talk(
        "agent",
        &[
            "agent-b",
            "Ship it",
            "--kind",
            "delivery",
            "--change",
            "abcd2345",
            "--follow-up",
            &later,
            "--dry-run",
        ],
    );
    assert_eq!(code, 0, "{dry}");
    let good = dry["detail"]["body"].clone();
    let git = bay::git::Git::new(&case.a);
    let push = |body: &Value| {
        let mut body = body.clone();
        body["id"] = json!(bay::talk::talk_id(&body).unwrap());
        let name = talk_ref("agent-b", body["id"].as_str().unwrap());
        let commit = bay::talk::root_commit(&git, &body, &case.keys.agent, "agent").unwrap();
        case.refused(&case.a, &format!("{commit}:{name}"))
    };
    let mut two = good.clone();
    two["artifact"] = json!("dist/a.tar");
    assert!(push(&two).contains("TALK_REFUSED"));
    let mut list = good.clone();
    list["change"] = json!(["abcd2345", "wxyz6723"]);
    assert!(push(&list).contains("TALK_REFUSED"));
    let mut cr = good.clone();
    cr["what"] = json!("Ship\rit");
    assert!(push(&cr).contains("TALK_REFUSED"));
    let mut big = good.clone();
    big["context"] = json!("a".repeat(9000));
    assert!(push(&big).contains("BAY_TALK_MAX_BYTES"));
    let mut decides = good;
    decides["decides"] = json!("nobody");
    assert!(push(&decides).contains("TALK_REFUSED"));
}

/// `owns`, `promises`, `needs` and `promise_evidence` are fields on the same talk; `target`,
/// `forecast` and `commitment` are three words, of which a talk names at most one, and `done`
/// or `ready` labels neither a target nor a forecast (`DESIGN` § 11c). The host takes what the
/// sender's bay takes.
#[test]
fn the_talk_words_are_fields_and_stay_distinct() {
    let case = hosted_talk();
    let later = from_now(3600);
    let base = |more: &[&str]| -> Vec<String> {
        [
            "agent-b",
            "Ship the parser",
            "--kind",
            "delivery",
            "--change",
            "abcd2345",
            "--follow-up",
            later.as_str(),
        ]
        .iter()
        .chain(more.iter())
        .map(|arg| (*arg).to_owned())
        .collect()
    };
    for (flags, says) in [
        (
            vec!["--target", "parser lands", "--forecast", "lands friday"],
            "at most one of `target`, `forecast` and `commitment`",
        ),
        (
            vec!["--forecast", "lands friday", "--commitment", "I land it"],
            "at most one of `target`, `forecast` and `commitment`",
        ),
        (
            vec!["--target", "done"],
            "cannot label a target or a forecast",
        ),
        (
            vec!["--forecast", " Ready "],
            "cannot label a target or a forecast",
        ),
    ] {
        let args = base(&flags);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = refused(&case, &args);
        assert!(
            result["detail"]["why"].as_str().unwrap().contains(says),
            "{flags:?}: {result}"
        );
    }
    nothing_committed(&case);

    let args = base(&[
        "--owns",
        "src/parser.rs",
        "--promises",
        "a parser that takes the new grammar",
        "--needs",
        "the grammar decision",
        "--promise-evidence",
        "tests/parser.rs",
        "--target",
        "the parser lands this week",
    ]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, sent) = case.talk("agent", &args);
    assert_eq!(code, 0, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap();
    let tip = case.host_tip(&talk_ref("agent-b", id)).unwrap();
    let body: Value = serde_json::from_str(
        &case
            .test_box
            .git_bare(&case.host, &["show", &format!("{tip}:talk.json")]),
    )
    .unwrap();
    assert_eq!(body["owns"], "src/parser.rs", "{body}");
    assert_eq!(body["promise_evidence"], "tests/parser.rs", "{body}");
    assert_eq!(body["target"], "the parser lands this week", "{body}");

    // The host refuses the same shapes when a commit skips the sender's bay.
    let git = bay::git::Git::new(&case.a);
    let mut both = body.clone();
    both["forecast"] = json!("lands friday");
    both["what"] = json!("Ship the parser again");
    both["id"] = json!(bay::talk::talk_id(&both).unwrap());
    let name = talk_ref("agent-b", both["id"].as_str().unwrap());
    let commit = bay::talk::root_commit(&git, &both, &case.keys.agent, "agent").unwrap();
    let stderr = case.refused(&case.a, &format!("{commit}:{name}"));
    assert!(
        stderr.contains("TALK_REFUSED: a talk names at most one of"),
        "{stderr}"
    );
}
