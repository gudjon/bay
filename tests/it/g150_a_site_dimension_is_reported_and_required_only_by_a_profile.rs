//! G150: a site-defined completion dimension, such as `reviewed` (`DESIGN` § 7; the review
//! dossier's correction K11).
//!
//! A `[[dimension]]` table names a record kind and the producers whose records count. For a
//! landing, `bay status` reports its state: `met` (only passes), `rejected` (a non-pass with no
//! answer), `answered` (a non-pass, then a landing whose `Fixes:`, `Reverts:` or `Supersedes:`
//! names the change; never a later pass on the same tree), or `missing`. Records count only
//! about the landing commit itself and, with a registry, only when they verify against the
//! class file their kind names. A profile may require the dimension; the landing verdict never
//! reads it, so a change lands before anyone reviews it.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::talk::fresh_key;
use support::{
    AGENT, EXECUTOR, PRODUCER, REGISTRAR, TestBox, check_files, key_line, passing_result,
    result_json,
};

const REVIEWER: &str = "review-1";
const POLICY: &str = "\n[[dimension]]\nname = \"reviewed\"\nkind = \"review\"\nproducers = [\"review-1\"]\n\n[profile.release]\nrequire = [\"published\", \"checked\", \"reviewed\"]\n";
const SELECT_RELEASE: &str = "[policy]\nprofile = \"release\"\n";

struct Site {
    test_box: TestBox,
    repo: PathBuf,
    reviewer_box: PathBuf,
}

fn site() -> Site {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(POLICY);
        }
    }
    files.push((".bay/config.toml".to_owned(), SELECT_RELEASE.to_owned()));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let keys = test_box.keys();
    let reviewer = fresh_key(&test_box, "reviewer");
    test_box.enroll_files(
        &repo,
        &keys,
        &[
            ("agent", key_line(AGENT, &keys.agent)),
            ("executor", key_line(EXECUTOR, &keys.executor)),
            ("producer", key_line(PRODUCER, &keys.producer)),
            ("reviewer", key_line(REVIEWER, &reviewer)),
            ("registry", key_line(REGISTRAR, &keys.registry)),
        ],
    );
    test_box.use_keys(&repo, &keys);
    let reviewer_box = test_box.clone_of(&remote, "reviewer-box");
    test_box.git_ok(&reviewer_box, &["config", "bay.reviewerName", REVIEWER]);
    test_box.git_ok(
        &reviewer_box,
        &["config", "bay.reviewerKey", reviewer.to_str().unwrap()],
    );
    Site {
        test_box,
        repo,
        reviewer_box,
    }
}

impl Site {
    /// Lands a change writing `src/<file>` (`--fixes <fixes>` when given): (id, landing).
    fn land(&self, file: &str, fixes: Option<&str>) -> (String, String) {
        let mut args = vec!["start", file, "--paths", "src/"];
        if let Some(fixed) = fixes {
            args.extend(["--fixes", fixed]);
        }
        let started = self.test_box.bay_ok(&self.repo, &args);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join("src").join(file), "fn landed() {}\n").unwrap();
        let (code, landed) = result_json(self.test_box.bay(&view).args(["merge", "--wait"]));
        assert_eq!(code, 0, "lands before any review: {landed}");
        (id, landed["detail"]["commit"].as_str().unwrap().to_owned())
    }

    /// A review of `landing`; `round` tells reviews of one landing apart (G75's id rule: one
    /// writer, kind, commit and inputs is one record).
    fn review(&self, landing: &str, verdict: &str, round: &str) {
        let input = format!("round={round}");
        let (code, written) = result_json(self.test_box.bay(&self.reviewer_box).args([
            "evidence",
            "record",
            "--kind",
            "review",
            "--verdict",
            verdict,
            "--commit",
            landing,
            "--input",
            &input,
        ]));
        assert_eq!(code, 0, "{written}");
    }

    fn completion(&self, id: &str) -> serde_json::Value {
        let status = self.test_box.bay_ok(&self.repo, &["status", id]);
        status["detail"]["changes"][0]["completion"].clone()
    }
}

fn reviewed_state(completion: &serde_json::Value) -> &str {
    completion["dimensions"]["reviewed"]["state"]
        .as_str()
        .unwrap_or_default()
}

#[test]
fn a_pass_review_meets_the_dimension_and_completes_the_profile() {
    let site = site();
    let (id, landing) = site.land("one.rs", None);
    let before = site.completion(&id);
    assert_eq!(reviewed_state(&before), "missing", "{before}");
    assert_eq!(before["profile"]["verdict"], "incomplete", "{before}");
    assert_eq!(
        before["profile"]["missing"],
        serde_json::json!(["reviewed"]),
        "{before}"
    );
    assert!(
        before["profile"]["summary"]
            .as_str()
            .unwrap()
            .contains("reviewed missing"),
        "{before}"
    );

    site.review(&landing, "pass", "1");
    let after = site.completion(&id);
    assert_eq!(reviewed_state(&after), "met", "{after}");
    assert_eq!(after["profile"]["verdict"], "complete", "{after}");
}

#[test]
fn a_rejection_is_answered_only_by_a_landing_that_names_the_change() {
    let site = site();
    let (id, landing) = site.land("one.rs", None);
    site.review(&landing, "product_failure", "1");
    let rejected = site.completion(&id);
    assert_eq!(reviewed_state(&rejected), "rejected", "{rejected}");
    assert_eq!(rejected["profile"]["verdict"], "incomplete", "{rejected}");

    // A later pass on the same landing never answers it: never latest wins.
    site.review(&landing, "pass", "2");
    let still = site.completion(&id);
    assert_eq!(reviewed_state(&still), "rejected", "{still}");

    // A landing whose Fixes: names the change answers it.
    site.land("fix.rs", Some(&id));
    let answered = site.completion(&id);
    assert_eq!(reviewed_state(&answered), "answered", "{answered}");
    assert_eq!(answered["profile"]["verdict"], "complete", "{answered}");
}

#[test]
fn a_record_from_a_producer_the_dimension_does_not_name_never_counts() {
    let site = site();
    let (id, landing) = site.land("one.rs", None);
    // The executor's producer writes a pass of kind review: it is no reviewer, so it signs
    // under the wrong class, and the dimension does not name it anyway.
    let (code, _) = result_json(site.test_box.bay(&site.repo).args([
        "evidence",
        "record",
        "--kind",
        "check",
        "--check",
        "review",
        "--verdict",
        "pass",
        "--commit",
        &landing,
    ]));
    assert_eq!(code, 0);
    let completion = site.completion(&id);
    assert_eq!(reviewed_state(&completion), "missing", "{completion}");
    let _ = Path::new("");
}
