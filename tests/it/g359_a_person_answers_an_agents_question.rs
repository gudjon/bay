//! G359 (owner ruling 2026-10-25, dossier 63 § 6 item 3: ruling T1 changed): a person answers an
//! agent's question.
//!
//! A question an agent asks with its question tool is a talk to itself (G358), and until now only
//! the talk's `who` or `decides`, both agents, could answer a discovery talk. On a talk that asks
//! a person (`decision: question:…` or `plan:…`), an answer signed by a person is taken as well:
//! an approver or registry key enrolled for the name the answer gives, as an approval is. The
//! name is bound to the key that signed, so the record of who decided is true. `bay talk answer`
//! signs as the machine's approver (`bay.approverName`, `bay.approverKey`) on such a talk.
//!
//! The answer entry is a public format: a page that signs with a person's passkey builds the same
//! bytes. This test pins them: `entries/<NNNN>-answer.json`, the canonical entry, committed on the
//! talk's tip with the message `answer <talk id>`, author and committer `<name>
//! <<name>@bay.invalid>` at the entry's `at`, signed (SSH, namespace `git`; the header is
//! `gpgsig` in a SHA-1 repository and `gpgsig-sha256` in a SHA-256 one, as Git writes it).
//!
//! Guard to mutate: `src/talk/judge.rs` `Chain::append`: sign answers with agent keys only (the
//! host refuses the person's answer).

use crate::support;

use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;

use serde_json::{Value, json};
use support::talk::{fresh_key, hosted_talk};

/// `bay hook tool.ask` of the recorded question as `agent-b`, session `s-q`, on its clone.
fn asks(case: &support::talk::TalkCase) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code-2.1.310/01-PreToolUse-AskUserQuestion.json");
    let mut payload: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    payload["session_id"] = json!("s-q");
    payload["cwd"] = json!(case.b);
    let mut child = case
        .test_box
        .bay_process(&case.b)
        .env("BAY_ACTOR", "agent-b")
        .args(["hook", "tool.ask"])
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
    assert_eq!(child.wait_with_output().unwrap().status.code(), Some(0));
    payload
}

/// The one question talk the host holds for `agent-b`, and its id.
fn question(case: &support::talk::TalkCase) -> (String, String) {
    let name = case
        .test_box
        .git_bare(
            &case.host,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/bay/talk/agent-b/",
            ],
        )
        .lines()
        .next()
        .unwrap_or_else(|| panic!("no question talk"))
        .to_owned();
    let id = name.rsplit('/').next().unwrap().to_owned();
    (name, id)
}

#[test]
fn a_person_with_an_approver_key_answers_and_the_host_takes_it() {
    let case = hosted_talk();
    let test_box = &case.test_box;
    // Alice is a person: an approver, enrolled by the registry key on `a`.
    let alice = fresh_key(test_box, "alice");
    let public = alice.with_extension("pub");
    test_box.git_ok(
        &case.a,
        &[
            "config",
            "bay.registryKey",
            case.keys.registry.to_str().unwrap(),
        ],
    );
    test_box.bay_ok(
        &case.a,
        &["enrol", "approver", "alice", public.to_str().unwrap()],
    );
    asks(&case);
    let (name, id) = question(&case);

    // From another clone, Alice answers as herself.
    let c = &case.c;
    test_box.git_ok(c, &["config", "bay.approverName", "alice"]);
    test_box.git_ok(c, &["config", "bay.approverKey", alice.to_str().unwrap()]);
    test_box.git_ok(
        c,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/bay/registry:refs/bay/registry",
        ],
    );
    let (code, answered) = support::result_json(
        test_box
            .bay(c)
            .args(["talk", "--answer", &id, "--value", "Blue"]),
    );
    assert_eq!(code, 0, "{answered}");

    // The host holds it: the answer entry, signed by Alice, under her name.
    let files = test_box.git_bare(&case.host, &["ls-tree", "-r", "--name-only", &name]);
    let path = files
        .lines()
        .find(|path| path.ends_with("-answer.json"))
        .unwrap_or_else(|| panic!("the host holds no answer: {files}"));
    assert!(
        path.starts_with("entries/") && path.len() == "entries/0002-answer.json".len(),
        "{path}"
    );
    let text = test_box.git_bare(&case.host, &["show", &format!("{name}:{path}")]);
    let entry: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(entry["actor"]["name"], "alice", "{entry}");
    // The public format: canonical JSON, keys sorted, these fields and no other.
    let at = entry["at"].as_str().unwrap();
    let session = entry["actor"]["session"].as_str().unwrap();
    assert_eq!(
        text,
        format!(
            "{{\"actor\":{{\"name\":\"alice\",\"session\":\"{session}\"}},\"at\":\"{at}\",\"entry\":\"answer\",\"schema\":\"bay.talk/1\",\"talk\":\"{id}\",\"value\":\"Blue\"}}"
        )
    );
    let commit = test_box.git_bare(&case.host, &["cat-file", "commit", &name]);
    assert!(
        commit.contains("\nauthor alice <alice@bay.invalid> ")
            && commit.contains("\ncommitter alice <alice@bay.invalid> ")
            // `gpgsig` in a SHA-1 repository, `gpgsig-sha256` in a SHA-256 one.
            && (commit.contains("\ngpgsig -----BEGIN SSH SIGNATURE-----")
                || commit.contains("\ngpgsig-sha256 -----BEGIN SSH SIGNATURE-----"))
            && commit.ends_with(&format!("answer {id}")),
        "{commit}"
    );
}

#[test]
fn an_agent_that_is_not_the_talks_party_still_cannot_answer() {
    let case = hosted_talk();
    asks(&case);
    let (_, id) = question(&case);
    // agent-c holds an agent key and no approver key: neither `who`, `decides` nor a person.
    let (code, refused) = case.talk("agent-c", &["--answer", &id, "--value", "Blue"]);
    assert_ne!(code, 0, "{refused}");
}
