//! G149b: a reviewer holds one open talk per change, and a later finding supersedes it (`DESIGN`
//! § 11c; the review dossier's correction K10 (d)).
//!
//! A reviewer's later `delivery --change <id>` talk about a change on which it holds an open talk
//! names that talk in `supersedes`, so the author's accept closes the earlier one as `superseded`
//! and the author's session lists one open talk from the reviewer about the change. The same
//! finding again is that finding, not a third talk. A second open talk that skips the sender's
//! bay is `TALK_REFUSED` naming the open one, at the host hook and, where no hook judged it, at
//! the author's accept.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::talk::{fresh_key, from_now, talk_ref};
use support::{
    AGENT, EXECUTOR, PRODUCER, REGISTRAR, TestBox, key_line, result_json, unchecked_files,
};

const REVIEWER: &str = "reviewer-a";

struct Reviewed {
    test_box: TestBox,
    a: PathBuf,
    reviewer_box: PathBuf,
    reviewer_key: PathBuf,
    change: String,
    landing: String,
}

/// A bay host whose registry enrols `agent` and reviewer `reviewer-a`; `agent` has landed one
/// change, and the reviewer's box holds its reviewer key and a review record of that landing.
fn reviewed() -> Reviewed {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let a = test_box.repo_without_remote_any("a");
    test_box.add_origin(&a, &host);
    test_box.seed_with_check(&a, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    support::talk::talk_on(&test_box, &a);
    let keys = test_box.keys();
    let reviewer_key = fresh_key(&test_box, "reviewer");
    test_box.enroll_files(
        &a,
        &keys,
        &[
            ("agent", key_line(AGENT, &keys.agent)),
            ("runner", key_line(EXECUTOR, &keys.executor)),
            ("producer", key_line(PRODUCER, &keys.producer)),
            ("reviewer", key_line(REVIEWER, &reviewer_key)),
            ("registry", key_line(REGISTRAR, &keys.registry)),
        ],
    );
    test_box.use_keys(&a, &keys);
    let as_agent = |dir: &Path| {
        let mut cmd = test_box.bay(dir);
        cmd.env("BAY_ACTOR", AGENT).env("BAY_SESSION", "s-agent");
        cmd
    };
    let (code, started) = result_json(as_agent(&a).args(["start", "CARD-1", "--paths", "src/"]));
    assert_eq!(code, 0, "{started}");
    let change = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    let (code, landed) = result_json(as_agent(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let landing = landed["detail"]["commit"].as_str().unwrap().to_owned();
    let reviewer_box = test_box.clone_of(&host, "reviewer-box");
    for (key, value) in [
        ("bay.reviewerName", REVIEWER),
        ("bay.reviewerKey", reviewer_key.to_str().unwrap()),
    ] {
        test_box.git_ok(&reviewer_box, &["config", key, value]);
    }
    test_box.git_ok(
        &reviewer_box,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/bay/registry:refs/bay/registry",
        ],
    );
    let case = Reviewed {
        test_box,
        a,
        reviewer_box,
        reviewer_key,
        change,
        landing,
    };
    case.review();
    case
}

impl Reviewed {
    fn as_reviewer(&self, args: &[&str]) -> (i32, Value) {
        result_json(
            self.test_box
                .bay(&self.reviewer_box)
                .env("BAY_ACTOR", REVIEWER)
                .env("BAY_SESSION", "s-review")
                .args(args),
        )
    }

    /// The reviewer's finding `what` about the change, owed by `follow_up`.
    fn finding(&self, what: &str, follow_up: &str, more: &[&str]) -> (i32, Value) {
        let mut args = vec![
            "talk",
            "--to-change",
            self.change.as_str(),
            what,
            "--kind",
            "delivery",
            "--change",
            self.change.as_str(),
            "--follow-up",
            follow_up,
        ];
        args.extend(more);
        self.as_reviewer(&args)
    }

    fn review(&self) {
        let (code, written) = self.as_reviewer(&[
            "evidence",
            "record",
            "--kind",
            "review",
            "--verdict",
            "product_failure",
            "--commit",
            &self.landing,
        ]);
        assert_eq!(code, 0, "{written}");
    }

    /// `bay status` in the author's session.
    fn author_status(&self) -> Value {
        let (code, status) = result_json(
            self.test_box
                .bay(&self.a)
                .env("BAY_ACTOR", AGENT)
                .env("BAY_SESSION", "s-agent")
                .arg("status"),
        );
        assert_eq!(code, 0, "{status}");
        status
    }

    /// A finding's body that skips the sender's bay: `what`, with no `supersedes`, so it stands
    /// beside the reviewer's open talk about the change.
    fn beside(&self, what: &str, follow_up: &str) -> Value {
        let (code, dry) = self.finding(what, follow_up, &["--dry-run"]);
        assert_eq!(code, 0, "{dry}");
        let mut body = dry["detail"]["body"].clone();
        assert!(body["supersedes"].is_string(), "{dry}");
        body.as_object_mut().unwrap().remove("supersedes");
        body["id"] = json!(bay::talk::talk_id(&body).unwrap());
        body
    }

    /// The root commit of `body` in `dir`, signed by the reviewer's key, and its talk ref.
    fn signed(&self, dir: &Path, body: &Value) -> (String, String) {
        let git = bay::git::Git::new(dir);
        let commit = bay::talk::root_commit(&git, body, &self.reviewer_key, REVIEWER).unwrap();
        (commit, talk_ref(AGENT, body["id"].as_str().unwrap()))
    }
}

fn ids(list: &Value) -> Vec<String> {
    let mut ids: Vec<String> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_owned())
        .collect();
    ids.sort_unstable();
    ids
}

/// The open talks the author's session lists from the reviewer.
fn from_reviewer(status: &Value) -> Vec<String> {
    status["detail"]["talks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["from"] == REVIEWER)
        .map(|row| row["talk"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn a_later_finding_supersedes_the_open_one_and_the_author_sees_one() {
    let case = reviewed();
    let follow_up = from_now(3600);
    let (code, sent) = case.finding("remove the unused function", &follow_up, &[]);
    assert_eq!(code, 0, "{sent}");
    let first = sent["detail"]["talk"].as_str().unwrap().to_owned();
    let status = case.author_status();
    assert_eq!(
        status["detail"]["talk_accept"]["acked"],
        json!([first.as_str()]),
        "{status}"
    );

    // The later finding names the open one in `supersedes`, unasked.
    let later = "rename the new function";
    let (code, dry) = case.finding(later, &follow_up, &["--dry-run"]);
    assert_eq!(code, 0, "{dry}");
    assert_eq!(dry["detail"]["body"]["supersedes"], first.as_str(), "{dry}");
    let (code, sent) = case.finding(later, &follow_up, &[]);
    assert_eq!(code, 0, "{sent}");
    let second = sent["detail"]["talk"].as_str().unwrap().to_owned();
    assert_ne!(second, first, "{sent}");

    // The same finding again is that finding, not a third talk.
    let (code, again) = case.finding(later, &follow_up, &[]);
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["state"], "already_sent", "{again}");
    assert_eq!(again["detail"]["talk"], second.as_str(), "{again}");

    // A `--supersedes` that leaves the open finding standing is refused, naming it.
    let (code, refused) = case.finding(
        "split the new function",
        &follow_up,
        &["--supersedes", &first],
    );
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "TALK_REFUSED", "{refused}");
    assert_eq!(refused["detail"]["oldest"], second.as_str(), "{refused}");

    // The host hook refuses a second open finding that skips the sender's bay.
    let body = case.beside("inline the new function", &follow_up);
    let (commit, name) = case.signed(&case.reviewer_box, &body);
    let out = case
        .test_box
        .git(&case.reviewer_box)
        .args(["push", "origin", &format!("{commit}:{name}")])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "the host took it: {stderr}");
    assert!(stderr.contains("TALK_REFUSED"), "{stderr}");
    assert!(stderr.contains(&second), "{stderr}");

    // The author's accept acks the later finding and closes the earlier one as superseded, and
    // the author's session lists one open talk from the reviewer: the later finding.
    let status = case.author_status();
    let accept = &status["detail"]["talk_accept"];
    assert_eq!(accept["acked"], json!([second.as_str()]), "{status}");
    assert_eq!(
        accept["closed"],
        json!([{"talk": first, "state": "superseded", "by": second}]),
        "{status}"
    );
    assert_eq!(from_reviewer(&status), vec![second], "{status}");
}

#[test]
fn the_authors_accept_closes_a_superseded_finding_and_refuses_a_second_open_one() {
    let case = reviewed();
    let follow_up = from_now(3600);
    let (code, sent) = case.finding("remove the unused function", &follow_up, &[]);
    assert_eq!(code, 0, "{sent}");
    let first = sent["detail"]["talk"].as_str().unwrap().to_owned();
    // The later finding arrives before the author accepted the first.
    let (code, sent) = case.finding("rename the new function", &follow_up, &[]);
    assert_eq!(code, 0, "{sent}");
    let second = sent["detail"]["talk"].as_str().unwrap().to_owned();

    let status = case.author_status();
    let accept = &status["detail"]["talk_accept"];
    let mut both = vec![first.clone(), second.clone()];
    both.sort_unstable();
    assert_eq!(ids(&accept["acked"]), both, "{status}");
    assert_eq!(accept["refused"], json!([]), "{status}");
    assert_eq!(
        accept["closed"],
        json!([{"talk": first, "state": "superseded", "by": second}]),
        "{status}"
    );
    assert_eq!(from_reviewer(&status), vec![second.clone()], "{status}");

    // A second open finding on the author's box, as a remote without bay's hook would deliver
    // it: the accept refuses it, naming the open one, and writes no ack.
    let body = case.beside("inline the new function", &follow_up);
    let (commit, name) = case.signed(&case.a, &body);
    case.test_box
        .git_ok(&case.a, &["update-ref", &name, &commit, ""]);
    let status = case.author_status();
    let accept = &status["detail"]["talk_accept"];
    let third = body["id"].as_str().unwrap();
    assert!(
        !ids(&accept["acked"]).iter().any(|id| id == third),
        "{status}"
    );
    let refused = accept["refused"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["talk"] == third)
        .unwrap_or_else(|| panic!("{third} is not refused: {status}"));
    let why = refused["why"].as_str().unwrap();
    assert!(why.contains("TALK_REFUSED"), "{refused}");
    assert!(why.contains(&second), "{refused}");
    let files = case
        .test_box
        .git_ok(&case.a, &["ls-tree", "-r", "--name-only", &name]);
    assert!(!files.contains("-ack.json"), "{files}");
}
