//! G46. A talk crosses a firewall by the remote, and arrival is an ack (`DESIGN` § 11c; rulings
//! R9, T6, T7 and T10 in `plans/wave2.md`).
//!
//! Three clones of one host stand for three machines: none opens a connection to another, each
//! pushes and fetches the host. Built so far: a push is not delivery and writes no ack; a fetch
//! without accept is not an ack; the recipient's fetching command (`bay status`) accepts and
//! writes the ack on the same ref, signed by the recipient's key; the sender sees it only after
//! its own fetch; the same id with the same body is a no-op and a second ack is the same ack; a
//! different body under the same id is refused; an ack by a third actor is refused at the host;
//! an unknown actor and an empty action are refused. The host takes a new talk only in its one
//! shape and a later commit only as the next entry, keeping every earlier file, from that entry's
//! one writer, by that entry's rules (backbriefs among them), at most 64 per talk; nothing deletes
//! a talk. A push is judged by the entries it adds, one signature check each, and a forged one
//! among them is refused. Accept reports one outcome shape, and one talk it cannot ack never stops the others.
//! Later parts of G46 (the `SessionStart`
//! notice, the draft left alone) come with the hook notices.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::{Value, json};
use support::talk::{TalkCase, canonical, contents, forge, from_now, hosted_talk, talk_ref};

fn send(case: &TalkCase, what: &str, follow_up: &str, more: &[&str]) -> (i32, Value) {
    let mut args = vec![
        "agent-b",
        what,
        "--kind",
        "delivery",
        "--change",
        "abcd2345",
        "--follow-up",
        follow_up,
    ];
    args.extend(more);
    case.talk("agent", &args)
}

#[test]
fn arrival_is_the_recipients_accept_and_the_sender_sees_it_after_a_fetch() {
    let case = hosted_talk();
    let follow_up = from_now(3600);
    let (code, sent) = send(&case, "Review the parser change", &follow_up, &[]);
    assert_eq!(code, 0, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();
    // T7: 80 bits of the content digest in base32.
    assert_eq!(id.len(), 16, "{sent}");
    let name = talk_ref("agent-b", &id);
    assert_eq!(sent["detail"]["ref"], name.as_str(), "{sent}");
    assert_eq!(sent["detail"]["pushed"], true, "{sent}");

    // A push is not delivery: the host holds the talk and no ack.
    let tip = case.host_tip(&name).expect("the talk is on the host");
    assert_eq!(case.files(None, &tip), ["talk.json"]);

    // A fetch that has not been accepted is not an ack either.
    case.test_box.git_ok(
        &case.b,
        &[
            "fetch",
            "-q",
            "origin",
            "refs/bay/talk/agent-b/*:refs/bay/talk/agent-b/*",
        ],
    );
    assert_eq!(case.host_tip(&name).as_deref(), Some(tip.as_str()));

    // B's status fetches, accepts and acks, signed by B's key.
    let (code, status) = case.status("agent-b");
    assert_eq!(code, 0, "{status}");
    assert_eq!(
        status["detail"]["talk_accept"]["acked"],
        json!([id.as_str()]),
        "{status}"
    );
    let acked = case.host_tip(&name).unwrap();
    assert_ne!(acked, tip);
    assert_eq!(
        case.files(None, &acked),
        ["entries/0001-ack.json", "talk.json"]
    );
    let ack: Value = serde_json::from_str(&case.test_box.git_bare(
        &case.host,
        &["show", &format!("{acked}:entries/0001-ack.json")],
    ))
    .unwrap();
    assert_eq!(ack["ack"], true, "{ack}");
    assert_eq!(ack["actor"]["name"], "agent-b", "{ack}");
    assert_eq!(ack["actor"]["session"], "s-agent-b", "{ack}");
    assert!(ack["at"].is_string(), "{ack}");
    // The ack is arrival, not work: it carries no essay and no status word.
    assert!(
        ack.get("state").is_none() && ack.get("done").is_none(),
        "{ack}"
    );

    // The sender sees the ack only after its own fetch.
    assert_eq!(
        case.local_tip(&case.a, &name).as_deref(),
        Some(tip.as_str())
    );
    let (code, _) = case.status("agent");
    assert_eq!(code, 0);
    assert_eq!(
        case.local_tip(&case.a, &name).as_deref(),
        Some(acked.as_str())
    );

    // A second accept of the same id is the same ack.
    let (code, again) = case.status("agent-b");
    assert_eq!(code, 0, "{again}");
    assert_eq!(
        again["detail"]["talk_accept"]["acked"],
        json!([]),
        "{again}"
    );
    assert_eq!(case.host_tip(&name).as_deref(), Some(acked.as_str()));
}

#[test]
fn one_id_is_one_talk() {
    let case = hosted_talk();
    let follow_up = from_now(3600);
    let (code, first) = send(&case, "Ship the parser", &follow_up, &[]);
    assert_eq!(code, 0, "{first}");
    let id = first["detail"]["talk"].as_str().unwrap().to_owned();
    let name = talk_ref("agent-b", &id);
    let tip = case.host_tip(&name).unwrap();

    // The same ask again (a retry after a lost reply) is the same talk: a no-op.
    let (code, retry) = send(&case, "Ship the parser", &follow_up, &[]);
    assert_eq!(code, 0, "{retry}");
    assert_eq!(retry["detail"]["talk"], id.as_str(), "{retry}");
    assert_eq!(retry["state"], "already_sent", "{retry}");
    assert_eq!(case.host_tip(&name).as_deref(), Some(tip.as_str()));

    // A different body under the same id is refused, by the sender's bay ...
    let (code, other) = send(
        &case,
        "Ship the parser",
        &follow_up,
        &["--context", "other"],
    );
    assert_eq!(code, 10, "{other}");
    assert_eq!(other["reason"], "TALK_BODY_CONFLICT", "{other}");
    assert_eq!(case.host_tip(&name).as_deref(), Some(tip.as_str()));

    // ... and by the host, which never lets a chain's body change.
    let git = bay::git::Git::new(&case.a);
    let mut body: Value = serde_json::from_str(
        &case
            .test_box
            .git_ok(&case.a, &["show", &format!("{tip}:talk.json")]),
    )
    .unwrap();
    body["context"] = json!("rewritten");
    let rewritten = bay::talk::root_commit(&git, &body, &case.keys.agent, "agent").unwrap();
    let stderr = case.refused(&case.a, &format!("+{rewritten}:{name}"));
    assert!(
        stderr.contains(&format!("{name}: a talk only appends")),
        "{stderr}"
    );

    // Different asks are different ids.
    let (code, second) = send(&case, "Ship the lexer", &follow_up, &[]);
    assert_eq!(code, 0, "{second}");
    assert_ne!(second["detail"]["talk"], id.as_str(), "{second}");
}

#[test]
fn an_ack_by_any_other_actor_is_refused_at_the_host() {
    let case = hosted_talk();
    let (code, sent) = send(&case, "Check the release notes", &from_now(3600), &[]);
    assert_eq!(code, 0, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();
    let name = talk_ref("agent-b", &id);
    case.test_box.git_ok(
        &case.c,
        &["fetch", "-q", "origin", &format!("+{name}:{name}")],
    );
    let tip = case.local_tip(&case.c, &name).unwrap();
    let git = bay::git::Git::new(&case.c);
    let ack = |actor: &str| {
        json!({"schema": "bay.talk/1", "entry": "ack", "talk": id, "ack": true,
            "actor": {"name": actor, "session": "s-c"}, "at": from_now(0)})
    };
    // agent-c acking a talk to agent-b, as itself or claiming to be agent-b.
    for (claimed, says) in [
        ("agent-c", "only agent-b, who owes the reply, acks talk"),
        (
            "agent-b",
            "is not signed by the agent key of agent-b, the actor its ack entry names",
        ),
    ] {
        let forged =
            bay::talk::entry_commit(&git, &tip, &ack(claimed), &case.c_key, "agent-c").unwrap();
        let stderr = case.refused(&case.c, &format!("{forged}:{name}"));
        assert!(stderr.contains(says), "{claimed}: {stderr}");
    }
    assert_eq!(case.host_tip(&name).as_deref(), Some(tip.as_str()));

    // agent-b's own ack is accepted.
    let (code, status) = case.status("agent-b");
    assert_eq!(code, 0, "{status}");
    assert_eq!(
        status["detail"]["talk_accept"]["acked"],
        json!([id.as_str()])
    );
}

#[test]
fn an_unknown_actor_and_an_empty_action_are_refused_before_a_commit() {
    let case = hosted_talk();
    let follow_up = from_now(3600);
    let (code, unknown) = case.talk(
        "agent",
        &[
            "nobody",
            "Ship it",
            "--kind",
            "delivery",
            "--change",
            "abcd2345",
            "--follow-up",
            &follow_up,
        ],
    );
    assert_eq!(code, 10, "{unknown}");
    assert_eq!(unknown["reason"], "TALK_INVALID", "{unknown}");
    let (code, empty) = send(&case, "  \n\t ", &follow_up, &[]);
    assert_eq!(code, 10, "{empty}");
    assert_eq!(empty["reason"], "TALK_INVALID", "{empty}");
    let refs = case.test_box.git_ok(
        &case.a,
        &["for-each-ref", "--format=%(refname)", "refs/bay/talk/"],
    );
    assert_eq!(refs, "", "nothing was committed");
}

/// Ruling T12: a recipient named like a verb is still a name.
#[test]
fn a_recipient_named_like_a_verb_is_an_actor() {
    let case = hosted_talk();
    let (code, result) = case.talk(
        "agent",
        &[
            "start",
            "Ship it",
            "--kind",
            "delivery",
            "--change",
            "abcd2345",
            "--follow-up",
            &from_now(3600),
        ],
    );
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "TALK_INVALID", "{result}");
    assert!(
        result["detail"]["why"]
            .as_str()
            .unwrap()
            .contains("\"start\" is not a registry actor"),
        "{result}"
    );
}

/// `bay watch` fetches too, and so accepts and acks on the recipient's machine.
#[test]
fn watch_on_the_recipients_machine_accepts() {
    let case = hosted_talk();
    let (code, sent) = send(&case, "Read the changelog", &from_now(3600), &[]);
    assert_eq!(code, 0, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();
    let (code, watched) = support::result_json(case.bay_as("agent-b").arg("watch"));
    assert_eq!(code, 0, "{watched}");
    assert_eq!(
        watched["detail"]["talk_accept"]["acked"],
        json!([id.as_str()])
    );
    let tip = case.host_tip(&talk_ref("agent-b", &id)).unwrap();
    assert_eq!(
        case.files(None, &tip),
        ["entries/0001-ack.json", "talk.json"]
    );
}

/// An entry by `actor` on talk `id`, at `at`, with the kind's own fields.
fn entry(kind: &str, id: &str, actor: &str, at: u64, more: &Value) -> Value {
    let mut entry = json!({"schema": "bay.talk/1", "entry": kind, "talk": id,
        "actor": {"name": actor, "session": format!("s-{actor}")},
        "at": bay::clock::rfc3339_of(at)});
    for (key, value) in more.as_object().unwrap() {
        entry[key] = value.clone();
    }
    entry
}

/// A backbrief's three names (`DESIGN` § 11c), and `more`.
fn backbrief(more: &Value) -> Value {
    let mut fields = json!({"outcome": "The release is tagged", "first_step": "Run the tag script",
        "unclear": "Which branch"});
    for (key, value) in more.as_object().unwrap() {
        fields[key] = value.clone();
    }
    fields
}

/// The files of `rev` in `dir`, less `drop`, plus `add`.
fn tree_with(
    dir: &std::path::Path,
    rev: &str,
    drop: &str,
    add: &[(&str, String)],
) -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = contents(dir, rev)
        .into_iter()
        .filter(|(path, _)| path != drop)
        .collect();
    files.extend(
        add.iter()
            .map(|(path, text)| ((*path).to_owned(), text.clone())),
    );
    files
}

fn borrowed(files: &[(String, String)]) -> Vec<(&str, &str)> {
    files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect()
}

/// A new talk is one parentless, canonical commit of `talk.json` alone, at `sent`, with a `sent`
/// no further ahead of the host's clock than its skew; anything else is refused at the host.
#[test]
fn the_host_takes_a_new_talk_only_in_its_one_shape() {
    let case = hosted_talk();
    let follow_up = from_now(3600);
    let (code, sent) = send(&case, "Tag the release", &follow_up, &[]);
    assert_eq!(code, 0, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();
    let name = talk_ref("agent-b", &id);
    let root = case.host_tip(&name).unwrap();

    // Nothing deletes a talk.
    let stderr = case.refused(&case.a, &format!(":{name}"));
    assert!(stderr.contains("a talk is never deleted"), "{stderr}");
    assert_eq!(case.host_tip(&name).as_deref(), Some(root.as_str()));

    let (code, dry) = send(&case, "Tag the next release", &follow_up, &["--dry-run"]);
    assert_eq!(code, 0, "{dry}");
    let body = dry["detail"]["body"].clone();
    let new_name = talk_ref("agent-b", body["id"].as_str().unwrap());
    let sent_at = bay::clock::unix_of(body["sent"].as_str().unwrap()).unwrap();
    let text = canonical(&body);
    let key = &case.keys.agent;
    let refused = |parent: Option<&str>, files: &[(&str, &str)], at: u64, name: &str| {
        let commit = forge(&case.a, key, "agent", parent, files, at);
        case.refused(&case.a, &format!("{commit}:{name}"))
    };
    let stderr = refused(Some(&root), &[("talk.json", &text)], sent_at, &new_name);
    assert!(
        stderr.contains("a talk starts as one parentless commit"),
        "{stderr}"
    );
    let pretty = serde_json::to_string_pretty(&body).unwrap();
    let stderr = refused(None, &[("talk.json", &pretty)], sent_at, &new_name);
    assert!(
        stderr.contains("talk.json is not in RFC 8785 canonical form"),
        "{stderr}"
    );
    let stderr = refused(None, &[("talk.json", &text)], sent_at + 5, &new_name);
    assert!(
        stderr.contains("`sent` is not the talk's commit time"),
        "{stderr}"
    );
    let stderr = refused(
        None,
        &[("talk.json", &text), ("notes.txt", "more")],
        sent_at,
        &new_name,
    );
    assert!(
        stderr.contains("a new talk holds talk.json alone"),
        "{stderr}"
    );

    // A `sent` hours ahead of the host's clock would postpone `stale` and `lost`: refused.
    let mut ahead = body.clone();
    ahead["sent"] = json!(from_now(3 * 3600));
    ahead["follow_up"] = json!(from_now(4 * 3600));
    ahead["id"] = json!(bay::talk::talk_id(&ahead).unwrap());
    let ahead_at = bay::clock::unix_of(ahead["sent"].as_str().unwrap()).unwrap();
    let ahead_name = talk_ref("agent-b", ahead["id"].as_str().unwrap());
    let stderr = refused(
        None,
        &[("talk.json", &canonical(&ahead))],
        ahead_at,
        &ahead_name,
    );
    assert!(stderr.contains("ahead of this host's clock"), "{stderr}");
    assert_eq!(case.host_tip(&new_name), None);
    assert_eq!(case.host_tip(&ahead_name), None);
}

/// The sender's close of talk `id` at `tip` is refused at the host: only `who` closes.
fn only_who_closes(case: &TalkCase, id: &str, tip: &str) {
    let name = talk_ref("agent-b", id);
    let a_git = bay::git::Git::new(&case.a);
    case.test_box.git_ok(
        &case.a,
        &["fetch", "-q", "origin", &format!("+{name}:{name}")],
    );
    let close = entry(
        "close",
        id,
        "agent",
        bay::live::unix_now(),
        &json!({"state": "superseded", "by": id}),
    );
    let by_sender =
        bay::talk::entry_commit(&a_git, tip, &close, &case.keys.agent, "agent").unwrap();
    let stderr = case.refused(&case.a, &format!("{by_sender}:{name}"));
    assert!(stderr.contains("only agent-b closes"), "{stderr}");
}

/// A backbrief's own rules at the host, then a good one from `who`: its commit.
fn backbrief_rules(case: &TalkCase, id: &str, acked: &str, follow_up: &str) -> String {
    let name = talk_ref("agent-b", id);
    let b = &case.b;
    let b_git = bay::git::Git::new(b);
    let now = bay::live::unix_now();
    let on = |tip: &str, kind: &str, more: &Value| {
        let entry = entry(kind, id, "agent-b", now, more);
        bay::talk::entry_commit(&b_git, tip, &entry, &case.b_key, "agent-b").unwrap()
    };
    let refused = |commit: &str| case.refused(b, &format!("{commit}:{name}"));
    // A backbrief's own rules.
    for (more, says) in [
        (
            json!({"owes": "nobody"}),
            "`owes` \"nobody\" is not a registry actor",
        ),
        (
            json!({"follow_up": bay::clock::rfc3339_of(now - 7200)}),
            "is not after",
        ),
        (json!({"status": "nearly"}), "is not a status word"),
        (
            json!({"status": "blocked"}),
            "a blocked backbrief states its new `follow_up`",
        ),
        (
            json!({"status": "blocked", "follow_up": follow_up}),
            "a blocked backbrief states its new `follow_up`",
        ),
        (
            json!({"status": "blocked", "follow_up": from_now(7200)}),
            "a blocked backbrief names its reason code",
        ),
        (
            json!({"status": "blocked", "follow_up": from_now(7200), "reason": "STUCK"}),
            "is not a reason code",
        ),
        (
            json!({"reason": "CHECK_FAILED"}),
            "only a blocked backbrief names a reason",
        ),
        (json!({"outcome": " "}), "a backbrief names `outcome`"),
    ] {
        let stderr = refused(&on(acked, "backbrief", &backbrief(&more)));
        assert!(stderr.contains(says), "{more}: {stderr}");
    }
    assert_eq!(case.host_tip(&name).as_deref(), Some(acked));

    // A good backbrief from `who` is taken; it may hand `owes` to another registry actor.
    let good = on(
        acked,
        "backbrief",
        &backbrief(&json!({"status": "blocked", "follow_up": from_now(7200),
            "reason": "CHECK_FAILED", "owes": "agent-c"})),
    );
    case.test_box
        .git_ok(b, &["push", "-q", "origin", &format!("{good}:{name}")]);
    good
}

/// Each later commit adds the next entry and keeps every earlier file; each entry kind has its
/// one writer and its own rules, at the host.
#[test]
fn the_host_judges_each_entry_and_keeps_every_earlier_one() {
    let case = hosted_talk();
    let follow_up = from_now(3600);
    let (code, sent) = send(&case, "Tag the release", &follow_up, &[]);
    assert_eq!(code, 0, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();
    let name = talk_ref("agent-b", &id);
    let (code, status) = case.status("agent-b");
    assert_eq!(code, 0, "{status}");
    let acked = case.host_tip(&name).unwrap();
    let b = &case.b;
    let b_git = bay::git::Git::new(b);
    let now = bay::live::unix_now();
    let on = |tip: &str, kind: &str, more: &Value| {
        let entry = entry(kind, &id, "agent-b", now, more);
        bay::talk::entry_commit(&b_git, tip, &entry, &case.b_key, "agent-b").unwrap()
    };
    let refused = |commit: &str| case.refused(b, &format!("{commit}:{name}"));

    // A second ack is the same ack: the host takes no second ack entry.
    let stderr = refused(&on(&acked, "ack", &json!({"ack": true})));
    assert!(stderr.contains("is already acked"), "{stderr}");

    // An append keeps every earlier file: an entry in place of the ack is refused.
    let brief = canonical(&entry(
        "backbrief",
        &id,
        "agent-b",
        now,
        &backbrief(&json!({})),
    ));
    let swapped = tree_with(
        b,
        &acked,
        "entries/0001-ack.json",
        &[("entries/0002-backbrief.json", brief.clone())],
    );
    let forged = forge(
        b,
        &case.b_key,
        "agent-b",
        Some(&acked),
        &borrowed(&swapped),
        now,
    );
    let stderr = refused(&forged);
    assert!(
        stderr.contains("an entry keeps every earlier file"),
        "{stderr}"
    );

    // Only a discovery talk takes an answer, even from `who`.
    let stderr = refused(&on(&acked, "answer", &json!({"value": "yes"})));
    assert!(
        stderr.contains("only a discovery talk takes an answer"),
        "{stderr}"
    );

    // An entry is committed at its `at`.
    let late = tree_with(b, &acked, "", &[("entries/0002-backbrief.json", brief)]);
    let forged = forge(
        b,
        &case.b_key,
        "agent-b",
        Some(&acked),
        &borrowed(&late),
        now + 90,
    );
    let stderr = refused(&forged);
    assert!(stderr.contains("is not its commit time"), "{stderr}");

    let good = backbrief_rules(&case, &id, &acked, &follow_up);

    // Only `who` closes, and a close names a talk that supersedes this one.
    only_who_closes(&case, &id, &good);
    let (code, other) = send(&case, "Tag the other release", &follow_up, &[]);
    assert_eq!(code, 0, "{other}");
    let other = other["detail"]["talk"].as_str().unwrap().to_owned();
    let stderr = refused(&on(
        &good,
        "close",
        &json!({"state": "superseded", "by": other}),
    ));
    assert!(
        stderr.contains(&format!("talk {other} does not supersede {id}")),
        "{stderr}"
    );

    // Once closed, a talk takes nothing more.
    let (code, successor) = send(
        &case,
        "Tag the release from the new branch",
        &follow_up,
        &["--supersedes", &id],
    );
    assert_eq!(code, 0, "{successor}");
    let (code, status) = case.status("agent-b");
    assert_eq!(code, 0, "{status}");
    let closed = case.host_tip(&name).unwrap();
    assert!(
        case.files(None, &closed)
            .contains(&"entries/0003-close.json".to_owned()),
        "{status}"
    );
    let stderr = refused(&on(&closed, "backbrief", &backbrief(&json!({}))));
    assert!(
        stderr.contains("is closed; it takes no more entries"),
        "{stderr}"
    );
}

/// One chain takes at most 64 entries, so the hook's work on one talk is bounded.
#[test]
fn a_talk_takes_at_most_sixty_four_entries() {
    let case = hosted_talk();
    let (code, sent) = send(&case, "Tag the release", &from_now(3600), &[]);
    assert_eq!(code, 0, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();
    let name = talk_ref("agent-b", &id);
    let (code, status) = case.status("agent-b");
    assert_eq!(code, 0, "{status}");
    let b_git = bay::git::Git::new(&case.b);
    let mut tip = case.host_tip(&name).unwrap();
    let now = bay::live::unix_now();
    // The ack is entry 1; 63 backbriefs make 64.
    let brief = entry("backbrief", &id, "agent-b", now, &backbrief(&json!({})));
    for _ in 0..63 {
        tip = bay::talk::entry_commit(&b_git, &tip, &brief, &case.b_key, "agent-b").unwrap();
    }
    case.test_box
        .git_ok(&case.b, &["push", "-q", "origin", &format!("{tip}:{name}")]);
    let more = bay::talk::entry_commit(&b_git, &tip, &brief, &case.b_key, "agent-b").unwrap();
    let stderr = case.refused(&case.b, &format!("{more}:{name}"));
    assert!(
        stderr.contains("holds 64 entries, the most one takes"),
        "{stderr}"
    );
}

/// An `ssh-keygen` on the box's `PATH` that logs each signature check (`-Y verify`, `-Y
/// find-principals`) the host's hook runs, then runs the real one. The log's path.
fn log_signature_checks(case: &TalkCase) -> std::path::PathBuf {
    let out = std::process::Command::new("sh")
        .args(["-c", "command -v ssh-keygen"])
        .output()
        .unwrap();
    let real = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    let log = case.test_box.path().join("signature-checks.log");
    let script = format!(
        "#!/bin/sh\ncase \"$2\" in verify|find-principals) echo \"$2\" >> '{}' ;; esac\nexec '{real}' \"$@\"\n",
        log.display()
    );
    let path = case.test_box.fakebin.join("ssh-keygen");
    std::fs::write(&path, script).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    log
}

/// The signature checks logged since the last call, and the log emptied.
fn signature_checks(log: &std::path::Path) -> usize {
    let count = std::fs::read_to_string(log).map_or(0, |text| text.lines().count());
    let _ = std::fs::remove_file(log);
    count
}

/// The host judges a push by the entries it adds: the entries its own ref already holds were
/// judged when they were pushed, and each new one is one signature check, so a push to a long
/// talk stays inside the hook's bound on a loaded host.
#[test]
fn a_push_is_judged_by_the_entries_it_adds() {
    let case = hosted_talk();
    let (code, sent) = send(&case, "Tag the release", &from_now(3600), &[]);
    assert_eq!(code, 0, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();
    let name = talk_ref("agent-b", &id);
    let (code, status) = case.status("agent-b");
    assert_eq!(code, 0, "{status}");
    let log = log_signature_checks(&case);
    let b_git = bay::git::Git::new(&case.b);
    let mut tip = case.host_tip(&name).unwrap();
    let now = bay::live::unix_now();
    let brief = entry("backbrief", &id, "agent-b", now, &backbrief(&json!({})));
    // The ack and 59 backbriefs: one signature check per new entry.
    for _ in 0..59 {
        tip = bay::talk::entry_commit(&b_git, &tip, &brief, &case.b_key, "agent-b").unwrap();
    }
    case.test_box
        .git_ok(&case.b, &["push", "-q", "origin", &format!("{tip}:{name}")]);
    let checks = signature_checks(&log);
    assert!(
        checks <= 59,
        "59 new entries took {checks} signature checks"
    );

    // One more entry on 60 is one signature check, not one per entry the talk holds.
    let one = bay::talk::entry_commit(&b_git, &tip, &brief, &case.b_key, "agent-b").unwrap();
    case.test_box
        .git_ok(&case.b, &["push", "-q", "origin", &format!("{one}:{name}")]);
    assert_eq!(case.host_tip(&name).as_deref(), Some(one.as_str()));
    let checks = signature_checks(&log);
    assert!(checks <= 1, "1 new entry took {checks} signature checks");
}

/// A forged entry among a push's new ones is refused, wherever it stands: one signed by another
/// actor's key, and one that carries a good signature from another commit.
#[test]
fn a_forged_entry_among_new_ones_is_refused() {
    let case = hosted_talk();
    let (code, sent) = send(&case, "Tag the release", &from_now(3600), &[]);
    assert_eq!(code, 0, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();
    let name = talk_ref("agent-b", &id);
    let (code, status) = case.status("agent-b");
    assert_eq!(code, 0, "{status}");
    let acked = case.host_tip(&name).unwrap();
    let b_git = bay::git::Git::new(&case.b);
    let now = bay::live::unix_now();
    let brief = entry("backbrief", &id, "agent-b", now, &backbrief(&json!({})));
    let good = bay::talk::entry_commit(&b_git, &acked, &brief, &case.b_key, "agent-b").unwrap();

    // The second of three new entries names agent-b and is signed by agent-c's key.
    let forged = bay::talk::entry_commit(&b_git, &good, &brief, &case.c_key, "agent-b").unwrap();
    let last = bay::talk::entry_commit(&b_git, &forged, &brief, &case.b_key, "agent-b").unwrap();
    let stderr = case.refused(&case.b, &format!("{last}:{name}"));
    assert!(
        stderr.contains(&format!(
            "{forged} is not signed by the agent key of agent-b"
        )),
        "{stderr}"
    );

    // A good signature moved onto another entry: the same commit but for its tree.
    let other = entry(
        "backbrief",
        &id,
        "agent-b",
        now,
        &backbrief(&json!({"unclear": "Which tag"})),
    );
    let other = bay::talk::entry_commit(&b_git, &good, &other, &case.b_key, "agent-b").unwrap();
    let signed = bay::talk::entry_commit(&b_git, &good, &brief, &case.b_key, "agent-b").unwrap();
    let raw = case
        .test_box
        .git_ok(&case.b, &["cat-file", "commit", &signed]);
    let tree = |commit: &str| {
        case.test_box
            .git_ok(&case.b, &["rev-parse", &format!("{commit}^{{tree}}")])
    };
    let moved = raw.replacen(&tree(&signed), &tree(&other), 1);
    assert_ne!(moved, raw);
    let out = case
        .test_box
        .git(&case.b)
        .args(["hash-object", "-t", "commit", "-w", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(format!("{moved}\n").as_bytes())?;
            child.wait_with_output()
        })
        .unwrap();
    let moved = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    let stderr = case.refused(&case.b, &format!("{moved}:{name}"));
    assert!(
        stderr.contains(&format!(
            "{moved} is not signed by the agent key of agent-b"
        )),
        "{stderr}"
    );
    assert_eq!(case.host_tip(&name).as_deref(), Some(acked.as_str()));

    // The good entries alone are taken.
    case.test_box.git_ok(
        &case.b,
        &["push", "-q", "origin", &format!("{good}:{name}")],
    );
    assert_eq!(case.host_tip(&name).as_deref(), Some(good.as_str()));
}

/// `detail.talk_accept` has one shape on every path, and says which path it took.
#[test]
fn the_accept_outcome_has_one_shape() {
    let case = hosted_talk();
    let (code, sent) = send(&case, "Read the changelog", &from_now(3600), &[]);
    assert_eq!(code, 0, "{sent}");
    let (code, status) = case.status("agent-b");
    assert_eq!(code, 0, "{status}");
    let accept = &status["detail"]["talk_accept"];
    assert_eq!(accept["state"], "accepted", "{status}");
    for field in ["acked", "closed", "refused", "failed"] {
        assert!(accept[field].is_array(), "{field}: {status}");
    }
    assert_eq!(accept["pushed"], true, "{status}");

    // A machine whose agent key is not enrolled for the actor accepts nothing, and says so.
    case.test_box
        .git_ok(&case.c, &["config", "--unset", "bay.agentKey"]);
    let (code, status) = case.status("agent-c");
    assert_eq!(code, 0, "{status}");
    let accept = &status["detail"]["talk_accept"];
    assert_eq!(accept["state"], "skipped", "{status}");
    assert!(accept["why"].is_string(), "{status}");
    for field in ["acked", "closed", "refused", "failed"] {
        assert_eq!(accept[field], json!([]), "{field}: {status}");
    }
    assert_eq!(accept["pushed"], false, "{status}");
}

/// A talk whose ack cannot be written here (its ref is locked) does not stop the other acks.
#[test]
fn one_talk_that_cannot_be_acked_does_not_stop_the_others() {
    if support::formats().1 != "files" {
        return; // a ref lock file is the files backend's
    }
    let case = hosted_talk();
    let follow_up = from_now(3600);
    let (code, first) = send(&case, "Read the changelog", &follow_up, &[]);
    assert_eq!(code, 0, "{first}");
    let (code, second) = send(&case, "Read the release notes", &follow_up, &[]);
    assert_eq!(code, 0, "{second}");
    let first = first["detail"]["talk"].as_str().unwrap().to_owned();
    let second = second["detail"]["talk"].as_str().unwrap().to_owned();
    let name = talk_ref("agent-b", &first);
    case.test_box.git_ok(
        &case.b,
        &[
            "fetch",
            "-q",
            "origin",
            "refs/bay/talk/agent-b/*:refs/bay/talk/agent-b/*",
        ],
    );
    let lock = case.b.join(".git").join(format!("{name}.lock"));
    std::fs::write(&lock, "").unwrap();
    let (code, status) = case.status("agent-b");
    assert_eq!(code, 0, "{status}");
    let accept = &status["detail"]["talk_accept"];
    assert_eq!(accept["acked"], json!([second.as_str()]), "{status}");
    assert_eq!(accept["failed"][0]["talk"], first.as_str(), "{status}");
    std::fs::remove_file(&lock).unwrap();
    let (code, status) = case.status("agent-b");
    assert_eq!(code, 0, "{status}");
    assert_eq!(
        status["detail"]["talk_accept"]["acked"],
        json!([first.as_str()]),
        "{status}"
    );
}
