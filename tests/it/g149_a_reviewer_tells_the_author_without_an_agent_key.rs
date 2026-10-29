//! G149: a reviewer tells the author of a landing what it found, with no agent key (`DESIGN`
//! § 11c; the review dossier's correction K10).
//!
//! A reviewer key sends a `delivery --change <id>` talk, and only that kind, about a change it
//! holds a verifying `review` record on: a record of its own, about that change's landing. The
//! sender's bay, the host hook and the recipient's accept judge it by the same rule. The talk
//! reaches the lander's session as any talk does, and the per-pair cap applies.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::talk::{fresh_key, from_now};
use support::{
    AGENT, EXECUTOR, PRODUCER, REGISTRAR, TestBox, key_line, result_json, unchecked_files,
};

const REVIEWER: &str = "review-1";

struct Reviewed {
    test_box: TestBox,
    host: PathBuf,
    a: PathBuf,
    reviewer_box: PathBuf,
    change: String,
    landing: String,
}

/// A bay host whose registry enrols `agent` and reviewer `review-1`; `agent` has landed one
/// change, and the reviewer's box holds its reviewer key.
fn reviewed() -> Reviewed {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let a = test_box.repo_without_remote_any("a");
    test_box.add_origin(&a, &host);
    test_box.seed_with_check(&a, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    support::talk::talk_on(&test_box, &a);
    let keys = test_box.keys();
    let reviewer = fresh_key(&test_box, "reviewer");
    test_box.enroll_files(
        &a,
        &keys,
        &[
            ("agent", key_line(AGENT, &keys.agent)),
            ("runner", key_line(EXECUTOR, &keys.executor)),
            ("producer", key_line(PRODUCER, &keys.producer)),
            ("reviewer", key_line(REVIEWER, &reviewer)),
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
        ("bay.reviewerKey", reviewer.to_str().unwrap()),
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
    Reviewed {
        test_box,
        host,
        a,
        reviewer_box,
        change,
        landing,
    }
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

    fn finding(&self, what: &str, kind: &[&str]) -> (i32, Value) {
        let follow_up = from_now(3600);
        let mut args = vec!["talk", "--to-change", self.change.as_str(), what];
        args.extend(kind);
        args.extend(["--follow-up", follow_up.as_str()]);
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
}

#[test]
fn a_reviewer_with_a_review_of_the_landing_reaches_its_author() {
    let case = reviewed();
    let delivery = ["--kind", "delivery", "--change", case.change.as_str()];

    // No review record of its own on the landing: refused, and nothing is sent.
    let (code, refused) = case.finding("remove the unused function", &delivery);
    assert_ne!(code, 0, "{refused}");
    assert_eq!(refused["reason"], "TALK_REFUSED", "{refused}");
    assert!(refused.to_string().contains("review record"), "{refused}");

    case.review();
    let (code, sent) = case.finding("remove the unused function", &delivery);
    assert_eq!(code, 0, "the host hook took the reviewer's talk: {sent}");
    let talk_ref = sent["detail"]["ref"].as_str().unwrap().to_owned();
    assert_eq!(sent["detail"]["who"], AGENT, "{sent}");
    let tip = case
        .test_box
        .git_bare(&case.host, &["rev-parse", &talk_ref]);

    // The author's next fetching command accepts it: the ack is on the chain.
    let (_, status) = result_json(
        case.test_box
            .bay(&case.a)
            .env("BAY_ACTOR", AGENT)
            .env("BAY_SESSION", "s-agent")
            .arg("status"),
    );
    let acked = case
        .test_box
        .git_bare(&case.host, &["rev-parse", &talk_ref]);
    assert_ne!(acked, tip, "the lander acked the reviewer's talk: {status}");
    let files = case
        .test_box
        .git_bare(&case.host, &["ls-tree", "-r", "--name-only", &acked]);
    assert!(files.contains("-ack.json"), "{files}");
}

#[test]
fn a_reviewer_sends_only_a_delivery_talk_about_a_change() {
    let case = reviewed();
    case.review();
    let decision = from_now(7200);
    let (code, refused) = case.finding(
        "decide the naming",
        &["--kind", "discovery", "--decision", &decision],
    );
    assert_ne!(code, 0, "{refused}");
    assert_eq!(refused["reason"], "TALK_REFUSED", "{refused}");
    assert!(refused.to_string().contains("delivery"), "{refused}");
}
