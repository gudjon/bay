//! G445 (N-21, owner ruling 2026-10-28): under `[policy] signing = "approvals"` a talk is
//! accepted unsigned, attributed to its actor (`BAY_ACTOR`) as a merge is, and acked unsigned by
//! its recipient. No key registry is needed, and none is fetched. The rules of who writes what stay,
//! judged by actor name: only the sender or `decides` supersedes, only `who` or `decides` answers.
//! Every talk row says whether a key signed its sender (`signed`), so a reader can tell a
//! claimed name from a signed one. An answer by `who` counts by its name; one written around bay
//! by a third actor does not. A fetching command (`bay status`) fetches talks at most once per
//! poll interval, on the live hook's state: inside it the outcome is `not_due`.
//!
//! Under `signing = "all"` nothing changes: a talk is signed by a registry actor's key, and an
//! unsigned one is refused at accept and never acked. A clone that never fetched the registry, or
//! fetched it before the sender was enrolled, fetches it before it judges (F1): a talk is never
//! dropped for want of a registry the remote holds.
//!
//! Every remote here is a plain Git remote, as a forge is: no bay hook judges a push, so each
//! rule is the sender's or the recipient's bay.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/talk.rs` `Trust::current`: `Signing::Approvals => Ok(Some(Self::Names))` to
//!   `Ok(None)` (every talk under approvals is refused again).
//! - `src/talk/send.rs` `open`: the `Trust::Names` arm back to a registry requirement.
//! - `src/talk/accept.rs` `receive`: the `Trust::Names` arm to `return None` (nothing is acked).
//! - `src/talk/accept.rs` `verify`: drop the signature check under `Trust::Keys` (the unsigned
//!   talk under `all` is acked).
//! - `src/talk/accept.rs` `receive`: drop the registry fetch (a talk to a clone that never fetched
//!   the registry is not acked).
//! - `src/talk/accept.rs` `accept`: drop the refetch on an unknown sender (a talk from a sender
//!   enrolled after the last fetch is refused).
//! - `src/talk.rs` `verified_answer`: `Trust::Names if party` to `Trust::Names` (the answer by
//!   name is not read), or drop `if party` (the third actor's answer counts).
//! - `src/talk/poll.rs` `command`: drop the interval check (the second status fetches again).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::talk::{canonical, fresh_key, from_now, talk_ref};
use support::{TestBox, TestKeys, result_json, unchecked_files};

const APPROVALS_ONLY: &str = "\n[policy]\nsigning = \"approvals\"\n";
const REGISTRY: &str = "refs/bay/registry";

/// Three clones of one plain Git remote: `agent` on `a`, `agent-b` on `b`, `agent-c` on `c`.
struct Case {
    test_box: TestBox,
    remote: PathBuf,
    a: PathBuf,
    b: PathBuf,
    c: PathBuf,
}

impl Case {
    fn clone_of(&self, actor: &str) -> &Path {
        match actor {
            "agent" => &self.a,
            "agent-b" => &self.b,
            "agent-c" => &self.c,
            other => panic!("no clone for {other}"),
        }
    }

    fn bay(&self, actor: &str, args: &[&str]) -> (i32, Value) {
        result_json(
            self.test_box
                .bay(self.clone_of(actor))
                .env("BAY_ACTOR", actor)
                .env("BAY_SESSION", format!("s-{actor}"))
                .env_remove("BAY_TEST_TALK_COMMAND_UNBOUNDED")
                .args(args),
        )
    }

    fn remote_tip(&self, name: &str) -> Option<String> {
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

    fn holds(&self, dir: &Path, name: &str) -> bool {
        self.test_box
            .git(dir)
            .args(["rev-parse", "--verify", "-q", name])
            .output()
            .unwrap()
            .status
            .success()
    }

    /// Whether commit `rev` on the remote carries a signature.
    fn signed(&self, rev: &str) -> bool {
        self.test_box
            .git_bare(&self.remote, &["cat-file", "commit", rev])
            .lines()
            .take_while(|line| !line.is_empty())
            .any(|line| line.starts_with("gpgsig"))
    }

    /// The answer `b`'s bay reads on talk `name` under approvals, after a fetch of it.
    fn answer(&self, name: &str) -> Option<String> {
        self.test_box.git_ok(
            &self.b,
            &["fetch", "-q", "origin", &format!("+{name}:{name}")],
        );
        let git = bay::git::Git::new(&self.b);
        let tip = git.run(&["rev-parse", name]).unwrap();
        let talk = bay::talk::Talk::read(&git, name, &tip).unwrap();
        bay::talk::verified_answer(&git, &bay::talk::Trust::Names, &talk)
    }

    fn entry(&self, rev: &str, path: &str) -> Value {
        serde_json::from_str(
            &self
                .test_box
                .git_bare(&self.remote, &["show", &format!("{rev}:{path}")]),
        )
        .unwrap()
    }
}

/// The plain remote, seeded on `a` with an unchecked policy (plus `more`), and clones `b` and `c`.
fn plain(more: &str) -> Case {
    let test_box = TestBox::new();
    let remote = test_box.bare_any("remote.git");
    let a = test_box.repo_without_remote_any("a");
    test_box.add_origin(&a, &remote);
    let mut files = unchecked_files();
    files[0].1.push_str(more);
    test_box.seed_with_check(&a, &[("src/a.rs", "fn a() {}\n")], &files);
    let b = test_box.clone_of(&remote, "b");
    let c = test_box.clone_of(&remote, "c");
    Case {
        test_box,
        remote,
        a,
        b,
        c,
    }
}

/// `signing = "approvals"` and no key registry anywhere: no machine holds a key.
fn approvals() -> Case {
    plain(APPROVALS_ONLY)
}

/// `signing = "all"` with a registry enrolling `agent` and `agent-b` (and `agent-c` when
/// `with_c`); `b` and `c` set their agent keys and never fetch the registry.
fn signed(with_c: bool) -> (Case, TestKeys, PathBuf, PathBuf) {
    let case = plain("");
    let keys = case.test_box.keys();
    let b_key = fresh_key(&case.test_box, "agent-b");
    let c_key = fresh_key(&case.test_box, "agent-c");
    let mut agents: Vec<(&str, &Path)> = vec![("agent-b", &b_key)];
    if with_c {
        agents.push(("agent-c", &c_key));
    }
    case.test_box
        .enroll_with(&case.a, &keys, &keys.executor, &agents);
    case.test_box.use_keys(&case.a, &keys);
    case.test_box.git_ok(
        &case.b,
        &["config", "bay.agentKey", b_key.to_str().unwrap()],
    );
    case.test_box.git_ok(
        &case.c,
        &["config", "bay.agentKey", c_key.to_str().unwrap()],
    );
    (case, keys, b_key, c_key)
}

fn delivery(case: &Case, from: &str, what: &str) -> (i32, Value) {
    static FOLLOW_UP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let follow_up = FOLLOW_UP.get_or_init(|| from_now(3600));
    case.bay(
        from,
        &[
            "talk",
            "agent-b",
            what,
            "--kind",
            "delivery",
            "--change",
            "abcd2345",
            "--follow-up",
            follow_up,
        ],
    )
}

fn discovery(case: &Case, from: &str, what: &str, more: &[&str]) -> (i32, Value) {
    static FOLLOW_UP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let follow_up = FOLLOW_UP.get_or_init(|| from_now(3600));
    let mut args = vec![
        "talk",
        "agent-b",
        what,
        "--kind",
        "discovery",
        "--decision",
        "wire-format",
        "--follow-up",
        follow_up,
    ];
    args.extend(more);
    case.bay(from, &args)
}

fn talk_rows(status: &Value) -> Vec<Value> {
    status["detail"]["talks"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[test]
fn under_approvals_a_talk_is_sent_unsigned_and_acked_by_name() {
    let case = approvals();
    let (code, sent) = delivery(&case, "agent", "Review the parser change");
    assert_eq!(code, 0, "{sent}");
    assert_eq!(sent["detail"]["pushed"], true, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();
    let name = talk_ref("agent-b", &id);
    let root = case.remote_tip(&name).expect("the talk is on the remote");
    // No key signed it: the sender is its actor name, as a merge under approvals is.
    assert!(!case.signed(&root), "{root} is signed");
    let body = case.entry(&root, "talk.json");
    assert_eq!(body["from"]["name"], "agent", "{body}");

    // B's fetching command accepts and acks it, unsigned, with no registry anywhere.
    let (code, status) = case.bay("agent-b", &["status"]);
    assert_eq!(code, 0, "{status}");
    assert_eq!(
        status["detail"]["talk_accept"]["acked"],
        json!([id.as_str()]),
        "{status}"
    );
    let acked = case.remote_tip(&name).unwrap();
    assert_ne!(acked, root);
    assert!(!case.signed(&acked), "{acked} is signed");
    let ack = case.entry(&acked, "entries/0001-ack.json");
    assert_eq!(ack["ack"], true, "{ack}");
    assert_eq!(ack["actor"]["name"], "agent-b", "{ack}");
    assert_eq!(ack["actor"]["session"], "s-agent-b", "{ack}");
    assert!(!case.holds(&case.b, REGISTRY), "b fetched a registry");

    // The row says the sender's name is a claim: no key signed it.
    let rows = talk_rows(&status);
    let row = rows
        .iter()
        .find(|row| row["talk"] == id.as_str())
        .unwrap_or_else(|| panic!("no row for {id}: {status}"));
    assert_eq!(row["signed"], false, "{row}");

    // The sender sees the ack after its own fetch; a second accept writes nothing.
    let (code, _) = case.bay("agent", &["status"]);
    assert_eq!(code, 0);
    // Inside the poll interval a status fetches no talks, and says so.
    let (code, again) = case.bay("agent-b", &["status"]);
    assert_eq!(code, 0, "{again}");
    assert_eq!(
        again["detail"]["talk_accept"]["state"], "not_due",
        "{again}"
    );
    assert_eq!(
        again["detail"]["talk_accept"]["acked"],
        json!([]),
        "{again}"
    );
    assert_eq!(case.remote_tip(&name).as_deref(), Some(acked.as_str()));
}

#[test]
fn under_approvals_only_who_or_decides_answers_and_only_the_sender_supersedes() {
    let case = approvals();
    let (code, first) = discovery(&case, "agent", "Pick the wire format: json or cbor", &[]);
    assert_eq!(code, 0, "{first}");
    let id = first["detail"]["talk"].as_str().unwrap().to_owned();
    let name = talk_ref("agent-b", &id);
    let (code, _) = case.bay("agent-b", &["status"]);
    assert_eq!(code, 0);

    // A third actor neither supersedes the talk nor answers it.
    let (code, other) = discovery(&case, "agent-c", "Pick cbor", &["--supersedes", &id]);
    assert_eq!(code, 10, "{other}");
    assert_eq!(other["reason"], "TALK_REFUSED", "{other}");
    let before = case.remote_tip(&name);
    let (code, answer) = case.bay("agent-c", &["talk", "--answer", &id, "--value", "cbor"]);
    assert_ne!(code, 0, "{answer}");
    assert_eq!(case.remote_tip(&name), before, "agent-c wrote on {name}");

    // `who` answers, by name, unsigned, and the answer counts by that name.
    let (code, answer) = case.bay("agent-b", &["talk", "--answer", &id, "--value", "json"]);
    assert_eq!(code, 0, "{answer}");
    let answered = case.remote_tip(&name).unwrap();
    assert!(!case.signed(&answered), "{answered} is signed");
    assert_eq!(case.answer(&name).as_deref(), Some("json"));

    // The sender supersedes its own talk.
    let (code, second) = discovery(&case, "agent", "Pick the log format", &[]);
    assert_eq!(code, 0, "{second}");
    let second = second["detail"]["talk"].as_str().unwrap().to_owned();
    let (code, later) = discovery(
        &case,
        "agent",
        "Pick json lines",
        &["--supersedes", &second],
    );
    assert_eq!(code, 0, "{later}");

    // A third actor's answer, written around bay, does not count.
    let later = talk_ref("agent-b", later["detail"]["talk"].as_str().unwrap());
    case.test_box.git_ok(
        &case.c,
        &["fetch", "-q", "origin", &format!("+{later}:{later}")],
    );
    let git = bay::git::Git::new(&case.c);
    let tip = git.run(&["rev-parse", &later]).unwrap();
    let entry = json!({"schema": "bay.talk/1", "entry": "answer",
        "talk": later.rsplit('/').next().unwrap(),
        "actor": {"name": "agent-c", "session": "s-agent-c"}, "value": "cbor",
        "at": bay::clock::now_rfc3339()});
    let forged = bay::talk::entry_commit_as(&git, &tip, &entry, None, "agent-c").unwrap();
    case.test_box.git_ok(
        &case.c,
        &["push", "-q", "origin", &format!("{forged}:{later}")],
    );
    assert_eq!(case.answer(&later), None);
}

#[test]
fn under_all_an_unsigned_talk_is_refused_and_never_acked() {
    let (case, _keys, _b_key, _c_key) = signed(false);
    // The sender's bay writes the body; a raw commit with no signature carries it to the remote.
    let (code, dry) = case.bay(
        "agent",
        &[
            "talk",
            "agent-b",
            "Ship it",
            "--kind",
            "delivery",
            "--change",
            "abcd2345",
            "--follow-up",
            &from_now(3600),
            "--dry-run",
        ],
    );
    assert_eq!(code, 0, "{dry}");
    let body = dry["detail"]["body"].clone();
    let id = body["id"].as_str().unwrap().to_owned();
    let sent = bay::clock::unix_of(body["sent"].as_str().unwrap()).unwrap();
    let git = bay::git::Git::new(&case.a);
    let blob = git
        .run_with(
            &["hash-object", "-w", "--stdin"],
            &[],
            Some(canonical(&body).as_bytes()),
        )
        .unwrap();
    let tree = git
        .run_with(
            &["mktree"],
            &[],
            Some(format!("100644 blob {blob}\ttalk.json\n").as_bytes()),
        )
        .unwrap();
    let date = format!("{sent} +0000");
    let commit = git
        .run_with(
            &["commit-tree", &tree, "-m", "unsigned"],
            &[
                ("GIT_AUTHOR_NAME", "agent"),
                ("GIT_AUTHOR_EMAIL", "agent@bay.invalid"),
                ("GIT_COMMITTER_NAME", "agent"),
                ("GIT_COMMITTER_EMAIL", "agent@bay.invalid"),
                ("GIT_AUTHOR_DATE", &date),
                ("GIT_COMMITTER_DATE", &date),
            ],
            None,
        )
        .unwrap();
    let name = talk_ref("agent-b", &id);
    case.test_box.git_ok(
        &case.a,
        &["push", "-q", "origin", &format!("{commit}:{name}")],
    );

    let (code, status) = case.bay("agent-b", &["status"]);
    assert_eq!(code, 0, "{status}");
    let accept = &status["detail"]["talk_accept"];
    assert_eq!(accept["acked"], json!([]), "{status}");
    let refused = accept["refused"].as_array().unwrap();
    assert!(
        refused
            .iter()
            .any(|row| row["talk"] == id.as_str()
                && row["why"].as_str().unwrap().contains("not signed")),
        "{status}"
    );
    assert_eq!(case.remote_tip(&name).as_deref(), Some(commit.as_str()));
}

#[test]
fn under_all_a_clone_that_never_fetched_the_registry_fetches_it_before_it_judges() {
    let (case, _keys, _b_key, _c_key) = signed(false);
    assert!(!case.holds(&case.b, REGISTRY));
    let (code, sent) = delivery(&case, "agent", "Review the parser change");
    assert_eq!(code, 0, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();
    let name = talk_ref("agent-b", &id);

    let (code, status) = case.bay("agent-b", &["status"]);
    assert_eq!(code, 0, "{status}");
    assert_eq!(
        status["detail"]["talk_accept"]["acked"],
        json!([id.as_str()]),
        "{status}"
    );
    let acked = case.remote_tip(&name).unwrap();
    assert!(case.signed(&acked), "{acked} is not signed");
    let row = talk_rows(&status)
        .into_iter()
        .find(|row| row["talk"] == id.as_str())
        .unwrap_or_else(|| panic!("no row for {id}: {status}"));
    assert_eq!(row["signed"], true, "{row}");
}

#[test]
fn under_all_a_sender_enrolled_after_the_last_fetch_is_judged_on_the_new_registry() {
    let (case, keys, b_key, c_key) = signed(false);
    // B holds the registry from before agent-c was enrolled.
    case.test_box.git_ok(
        &case.b,
        &["fetch", "-q", "origin", &format!("+{REGISTRY}:{REGISTRY}")],
    );
    case.test_box.enroll_with(
        &case.a,
        &keys,
        &keys.executor,
        &[("agent-b", &b_key), ("agent-c", &c_key)],
    );
    let (code, sent) = delivery(&case, "agent-c", "Review the lexer change");
    assert_eq!(code, 0, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();

    let (code, status) = case.bay("agent-b", &["status"]);
    assert_eq!(code, 0, "{status}");
    assert_eq!(
        status["detail"]["talk_accept"]["acked"],
        json!([id.as_str()]),
        "{status}"
    );
}
