//! G470 (A6, the owner's probe of bay talk, 2026-10-28). A delivery or reliability talk is
//! answered with a backbrief.
//!
//! The talk schema and the host's judge knew a `backbrief` entry (outcome, first step, what is
//! unclear, a status word, evidence, a new `follow_up`), and nothing wrote one: `--answer`
//! refuses a delivery or reliability talk, so such a talk had no reply command at all. Now
//! `bay talk --backbrief <id> <status>` writes it, by the judge's rules: only `who` writes one; the
//! status is ready, blocked, validated or done; a blocked one names its reason code and a new
//! `follow_up`; only a blocked one names a reason; a closed talk takes none. The same backbrief
//! again is `already_backbriefed` and writes nothing; a different one is the next entry (a
//! backbrief reports progress, so a talk takes several). The notice of a delivery or reliability
//! talk names the command, as it names `--answer` for a discovery talk, and the sender hears the
//! status word, never the prose (ruling R13).
//!
//! Under `signing = "all"` the host's judge takes each backbrief written (the bay host here);
//! under `approvals` it is written by name, unsigned (a plain remote).
//!
//! The guards to mutate, in `src/talk/backbrief.rs`: in `checked`, the only-`who` refusal, the
//! status-word refusal, the reason refused unless blocked, the reason-code check and the
//! closed-talk refusal; in `blocked`, the new `follow_up` and the reason a blocked one names; in
//! `give`, the same-backbrief `already_backbriefed` return. Under `all` the host's judge refuses
//! the same writes, so the approvals case is the one each of those mutants fails. In
//! `src/notice.rs` `talk_line`: the `--backbrief` command for a delivery or reliability talk. In
//! `src/talk/replies.rs` `entries`: the status word of a backbrief.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::Value;
use support::talk::{TalkCase, from_now, hosted_talk_seeded, talk_ref};
use support::{TestBox, result_json, unchecked_files};

fn fixture(name: &str, session: &str, cwd: &Path) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code-2.1.302")
        .join(name);
    let mut payload: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    payload["session_id"] = Value::from(session);
    payload["cwd"] = Value::from(cwd.display().to_string());
    payload
}

/// A prompt's hook as `actor` on `clone`, its talk poll made due first: what it printed.
fn prompt(test_box: &TestBox, clone: &Path, actor: &str) -> String {
    let _ = std::fs::remove_file(clone.join(".git/bay/talk-poll.json"));
    let payload = fixture("02-UserPromptSubmit.json", &format!("s-{actor}"), clone);
    let mut child = test_box
        .bay_process(clone)
        .env("BAY_ACTOR", actor)
        .args(["hook", "turn.start"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    format!(
        "{}{}",
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap()
    )
}

/// The entry at `path` on the host's tip of `name`.
fn host_entry(case: &TalkCase, name: &str, path: &str) -> Value {
    let tip = case.host_tip(name).unwrap();
    serde_json::from_str(
        &case
            .test_box
            .git_bare(&case.host, &["show", &format!("{tip}:{path}")]),
    )
    .unwrap()
}

/// The talk host, with talk turned on in main's policy.
fn talk_on() -> TalkCase {
    hosted_talk_seeded(|test_box, a| {
        test_box.seed_trunk(
            a,
            &[
                ("src/a.rs", "fn a() {}\n"),
                (
                    ".bay/policy.toml",
                    "revision = 1\nunchecked = true\n\n[talk]\nenabled = true\n",
                ),
            ],
        );
    })
}

fn why(result: &Value) -> &str {
    result["detail"]["why"].as_str().unwrap_or_default()
}

#[test]
fn the_recipient_of_a_delivery_talk_replies_with_a_backbrief() {
    let case = talk_on();
    let follow_up = from_now(3600);
    let (code, sent) = case.talk(
        "agent",
        &[
            "agent-b",
            "Tag the release",
            "--kind",
            "delivery",
            "--change",
            "abcd2345",
            "--follow-up",
            &follow_up,
        ],
    );
    assert_eq!(code, 0, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();
    let name = talk_ref("agent-b", &id);

    // The recipient's notice names the one reply command of a delivery talk, and no prose.
    let told = prompt(&case.test_box, &case.b, "agent-b");
    assert!(
        told.contains(&format!(
            "; reply with `bay talk --backbrief {id} <status>`"
        )),
        "{told}"
    );
    assert!(!told.contains("--answer"), "{told}");
    assert!(
        !told.contains("release"),
        "the prose never reaches the notice"
    );
    let acked = case.host_tip(&name).unwrap();

    // Only `who` replies: neither a third agent nor the sender writes a backbrief.
    for actor in ["agent-c", "agent"] {
        let (code, refused) = case.talk(actor, &["--backbrief", &id, "ready"]);
        assert_eq!(code, 10, "{actor}: {refused}");
        assert_eq!(refused["reason"], "TALK_REFUSED", "{refused}");
        assert!(why(&refused).contains("only agent-b"), "{refused}");
    }
    refused_rules(&|args| case.talk("agent-b", args), &id, &follow_up);
    assert_eq!(case.host_tip(&name).as_deref(), Some(acked.as_str()));

    // `who` reports ready, with its evidence; the host's judge takes it.
    let (code, ready) = case.talk(
        "agent-b",
        &["--backbrief", &id, "ready", "--evidence", "abcd2345"],
    );
    assert_eq!(code, 0, "{ready}");
    assert_eq!(ready["state"], "backbriefed", "{ready}");
    assert_eq!(ready["detail"]["pushed"], true, "{ready}");
    let entry = host_entry(&case, &name, "entries/0002-backbrief.json");
    assert_eq!(entry["status"], "ready", "{entry}");
    assert_eq!(entry["evidence"], "abcd2345", "{entry}");
    assert_eq!(entry["actor"]["name"], "agent-b", "{entry}");
    assert!(entry["reason"].is_null(), "{entry}");
    let replied = case.host_tip(&name).unwrap();
    assert_ne!(replied, acked);

    // The same backbrief again writes nothing.
    let (code, again) = case.talk(
        "agent-b",
        &["--backbrief", &id, "ready", "--evidence", "abcd2345"],
    );
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["state"], "already_backbriefed", "{again}");
    assert_eq!(case.host_tip(&name).as_deref(), Some(replied.as_str()));

    // The sender hears the status word, never the prose.
    let heard = prompt(&case.test_box, &case.a, "agent");
    assert!(
        heard.contains(&format!("bay: your talk {id} to agent-b reports ready:")),
        "{heard}"
    );
    assert!(!heard.contains("abcd2345"), "{heard}");

    blocked_rules(&case, &id, &replied);

    // A different status is the next entry, with its own outcome.
    let (code, done) = case.talk(
        "agent-b",
        &["--backbrief", &id, "done", "--outcome", "Tagged v1.2"],
    );
    assert_eq!(code, 0, "{done}");
    assert_eq!(done["state"], "backbriefed", "{done}");
    let entry = host_entry(&case, &name, "entries/0004-backbrief.json");
    assert_eq!(entry["status"], "done", "{entry}");
    assert_eq!(entry["outcome"], "Tagged v1.2", "{entry}");

    // A delivery talk still takes no answer.
    let (code, answer) = case.talk("agent-b", &["--answer", &id, "--value", "done"]);
    assert_eq!(code, 10, "{answer}");
}

/// Each rule of a backbrief, as `reply` (`who`'s `bay talk`) meets it on talk `id`, which owes
/// its reply by `follow_up`: refused, saying why. A status is a status word; only a blocked one
/// names a reason, and a blocked one names a new `follow_up` and a reason code.
fn refused_rules(reply: &dyn Fn(&[&str]) -> (i32, Value), id: &str, follow_up: &str) {
    let later = from_now(7200);
    for (args, says) in [
        (vec!["nearly"], "ready, blocked, validated or done"),
        (
            vec!["ready", "--reason", "CHECK_FAILED"],
            "only a blocked backbrief",
        ),
        (vec!["blocked"], "new `follow_up`"),
        (vec!["blocked", "--follow-up", follow_up], "new `follow_up`"),
        (
            vec!["blocked", "--follow-up", later.as_str()],
            "reason code",
        ),
        (
            vec![
                "blocked",
                "--follow-up",
                later.as_str(),
                "--reason",
                "STUCK",
            ],
            "is not a reason code",
        ),
    ] {
        let mut all = vec!["--backbrief", id];
        all.extend(args);
        let (code, refused) = reply(&all);
        assert_eq!(code, 10, "{all:?}: {refused}");
        assert_eq!(refused["reason"], "TALK_REFUSED", "{refused}");
        assert!(why(&refused).contains(says), "{all:?}: {refused}");
    }
}

/// A blocked backbrief with a new `follow_up` and a reason code is the next entry (the talk
/// stands at `replied`).
fn blocked_rules(case: &TalkCase, id: &str, replied: &str) {
    let name = talk_ref("agent-b", id);
    assert_eq!(case.host_tip(&name).as_deref(), Some(replied));
    let later = from_now(7200);
    let (code, blocked) = case.talk(
        "agent-b",
        &[
            "--backbrief",
            id,
            "blocked",
            "--follow-up",
            &later,
            "--reason",
            "CHECK_FAILED",
        ],
    );
    assert_eq!(code, 0, "{blocked}");
    assert_eq!(blocked["state"], "backbriefed", "{blocked}");
    let entry = host_entry(case, &name, "entries/0003-backbrief.json");
    assert_eq!(entry["status"], "blocked", "{entry}");
    assert_eq!(entry["reason"], "CHECK_FAILED", "{entry}");
    assert_eq!(entry["follow_up"], later.as_str(), "{entry}");
}

/// A plain remote under `signing = "approvals"`, seeded on `a`; clones `b` and `c`.
struct Plain {
    test_box: TestBox,
    remote: PathBuf,
    a: PathBuf,
    b: PathBuf,
    c: PathBuf,
}

impl Plain {
    fn new() -> Self {
        let test_box = TestBox::new();
        let remote = test_box.bare_any("remote.git");
        let a = test_box.repo_without_remote_any("a");
        test_box.add_origin(&a, &remote);
        let mut files = unchecked_files();
        files[0]
            .1
            .push_str("\n[policy]\nsigning = \"approvals\"\n\n[talk]\nenabled = true\n");
        test_box.seed_with_check(&a, &[("src/a.rs", "fn a() {}\n")], &files);
        let b = test_box.clone_of(&remote, "b");
        let c = test_box.clone_of(&remote, "c");
        Self {
            test_box,
            remote,
            a,
            b,
            c,
        }
    }

    fn bay(&self, actor: &str, args: &[&str]) -> (i32, Value) {
        let dir = match actor {
            "agent" => &self.a,
            "agent-b" => &self.b,
            _ => &self.c,
        };
        result_json(
            self.test_box
                .bay(dir)
                .env("BAY_ACTOR", actor)
                .env("BAY_SESSION", format!("s-{actor}"))
                .args(args),
        )
    }

    fn tip(&self, name: &str) -> Option<String> {
        let out = self
            .test_box
            .git(self.test_box.path())
            .args([
                &format!("--git-dir={}", self.remote.display()),
                "rev-parse",
                "--verify",
                "-q",
                name,
            ])
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8(out.stdout).unwrap().trim().to_owned())
    }
}

#[test]
fn under_approvals_a_reliability_talk_takes_a_backbrief_by_name_and_a_closed_talk_none() {
    let case = Plain::new();
    let follow_up = from_now(3600);
    let (code, sent) = case.bay(
        "agent",
        &[
            "talk",
            "agent-b",
            "Run the soak",
            "--kind",
            "reliability",
            "--check",
            "soak",
            "--follow-up",
            &follow_up,
        ],
    );
    assert_eq!(code, 0, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();
    let name = talk_ref("agent-b", &id);
    let told = prompt(&case.test_box, &case.b, "agent-b");
    assert!(
        told.contains(&format!("reply with `bay talk --backbrief {id} <status>`")),
        "{told}"
    );
    let acked = case.tip(&name).unwrap();

    // A third actor's name is not `who`'s; with no host to judge, bay keeps every rule itself.
    let (code, refused) = case.bay("agent-c", &["talk", "--backbrief", &id, "done"]);
    assert_eq!(code, 10, "{refused}");
    let reply = |args: &[&str]| {
        let mut all = vec!["talk"];
        all.extend_from_slice(args);
        case.bay("agent-b", &all)
    };
    refused_rules(&reply, &id, &follow_up);
    assert_eq!(case.tip(&name).as_deref(), Some(acked.as_str()));

    // `who` reports validated, by name, unsigned.
    let (code, validated) = case.bay(
        "agent-b",
        &[
            "talk",
            "--backbrief",
            &id,
            "validated",
            "--evidence",
            "soak-run-7",
        ],
    );
    assert_eq!(code, 0, "{validated}");
    assert_eq!(validated["state"], "backbriefed", "{validated}");
    let tip = case.tip(&name).unwrap();
    let entry: Value = serde_json::from_str(&case.test_box.git_bare(
        &case.remote,
        &["show", &format!("{tip}:entries/0002-backbrief.json")],
    ))
    .unwrap();
    assert_eq!(entry["status"], "validated", "{entry}");
    assert_eq!(entry["actor"]["name"], "agent-b", "{entry}");
    let commit = case
        .test_box
        .git_bare(&case.remote, &["cat-file", "commit", &tip]);
    assert!(!commit.contains("gpgsig"), "{commit}");

    // A closed talk takes no backbrief: a discovery talk answered is closed.
    let (code, question) = case.bay(
        "agent",
        &[
            "talk",
            "agent-b",
            "Pick a colour",
            "--kind",
            "discovery",
            "--decision",
            "colour",
            "--follow-up",
            &follow_up,
        ],
    );
    assert_eq!(code, 0, "{question}");
    let question = question["detail"]["talk"].as_str().unwrap().to_owned();
    let (code, answered) = case.bay(
        "agent-b",
        &["talk", "--answer", &question, "--value", "blue"],
    );
    assert_eq!(code, 0, "{answered}");
    let closed = case.tip(&talk_ref("agent-b", &question));
    let (code, refused) = case.bay("agent-b", &["talk", "--backbrief", &question, "done"]);
    assert_eq!(code, 10, "{refused}");
    assert!(why(&refused).contains("is closed"), "{refused}");
    assert_eq!(case.tip(&talk_ref("agent-b", &question)), closed);
}
