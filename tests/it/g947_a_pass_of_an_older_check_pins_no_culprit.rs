//! G947 (a fleet finding, 2026-11-27: a merge added a unit to a post check, its own run passed
//! under the old check, and the next merge, a change to one document, was named the culprit of
//! the new unit's failure and undone). A post run judges a landing with its first parent's check
//! (G91), so the run of a landing that changes the check is a run of the old check. A pass there
//! says nothing of the new check: the landing above it that fails is no culprit, and the landing
//! that changed the check owns the red (`suspect.check_changed`), as one that adds a check does
//! (G584). The run of no change on it (BACKLOG 348) judges it with the new check; a failure there
//! makes the red the check's own.
//!
//! The post check `suite` fails every tree while its adapter file `scripts/suite-mode` says
//! `new`, an environment the new check cannot run in. `grow` makes the mode `new`, and its own
//! run, under the old mode, passes; `next` changes only a source file, and its run fails.
//!
//! Guards to mutate, in `src/red.rs`: `Answers::changed_since` always `None` (`next` is named
//! the culprit), or always `Some` (G919: a flaky landing that passed the red's units is no longer
//! its pass, and the red names no culprit).

// Each case runs shell scripts (the adapter), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, result_json};

/// A pre check `tests` that always passes, and a post check `suite` on `src/**` whose adapter
/// holds its mode file.
fn policy(revision: u32) -> String {
    format!(
        "revision = {revision}\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\", \"scripts/suite-mode\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n"
    )
}

/// The adapter. `suite` fails every tree while the mode file of its own adapter says `new`;
/// every other run passes.
const ADAPTER: &str = r#"#!/bin/sh
req=$(cat)
commit=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\1/p')
tree=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\2/p')
verdict=pass
case "$req" in
*'"plan":{"id":"suite"'*)
  if [ "$(cat "$BAY_ADAPTER_DIR/scripts/suite-mode")" = new ]; then
    verdict=product_failure
  fi
  ;;
esac
printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"selected":1,"completed":1},"checks":[{"id":"t1","verdict":"%s"}]}' "$commit" "$tree" "$verdict"
"#;

/// The verdicts of post check `check` on the remote about `tip`.
fn verdicts(test_box: &TestBox, remote: &Path, check: &str, tip: &str) -> Vec<Value> {
    let refs = test_box.git_bare(
        remote,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/notes/bay/evidence/",
        ],
    );
    let mut out = Vec::new();
    for notes_ref in refs.lines() {
        let blobs = test_box.git_bare(
            remote,
            &["ls-tree", "-r", "--format=%(objectname)", notes_ref],
        );
        for blob in blobs.lines() {
            let text = test_box.git_bare(remote, &["cat-file", "blob", blob]);
            let records: Vec<Value> = serde_json::from_str(&text).unwrap();
            out.extend(
                records
                    .into_iter()
                    .filter(|record| {
                        record["check"]["id"] == check && record["subject"]["commit"] == tip
                    })
                    .map(|record| record["verdict"].clone()),
            );
        }
    }
    out
}

struct Line {
    test_box: TestBox,
    repo: PathBuf,
    remote: PathBuf,
}

impl Line {
    /// A line whose runner runs post checks, and bisects a red when `bisect`.
    fn new(bisect: bool) -> Self {
        let test_box = TestBox::new();
        let repo = test_box.repo_any("repo");
        let remote = test_box.path().join("repo-remote.git");
        let files = vec![
            ("scripts/bay-check".to_owned(), ADAPTER.to_owned()),
            ("scripts/suite-mode".to_owned(), "old\n".to_owned()),
            (".bay/policy.toml".to_owned(), policy(1)),
            (
                ".bay/config.toml".to_owned(),
                format!("[runner]\npost = true\nbisect_post = {bisect}\n"),
            ),
        ];
        test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
        Self {
            test_box,
            repo,
            remote,
        }
    }

    /// Lands change `name` on `paths`, writing `written` in its view, runs passes until `suite`
    /// has a record on the new tip, and returns the change and the tip.
    fn land(&self, name: &str, paths: &[&str], written: &[(&str, &str)]) -> (String, String) {
        let mut args = vec!["start", name, "--paths"];
        args.extend(paths);
        let started = self.test_box.bay_ok(&self.repo, &args);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        for (path, text) in written {
            std::fs::write(view.join(path), text).unwrap();
        }
        let (code, merged) = result_json(self.test_box.bay(&view).args(["merge", "--wait"]));
        assert_eq!(code, 0, "{merged}");
        let tip = self
            .test_box
            .git_bare(&self.remote, &["rev-parse", "main"])
            .trim()
            .to_owned();
        let mut ran = Vec::new();
        for _ in 0..3 {
            if !verdicts(&self.test_box, &self.remote, "suite", &tip).is_empty() {
                return (id, tip);
            }
            ran.push(self.pass());
        }
        panic!("no run of suite on {name}'s tip: {ran:?}");
    }

    /// One runner pass.
    fn pass(&self) -> Value {
        let (code, pass) = result_json(
            self.test_box
                .bay(&self.repo)
                .args(["runner", "run", "--once"]),
        );
        assert_eq!(code, 0, "{pass}");
        pass
    }

    /// `status`'s `detail.red`.
    fn red(&self) -> Value {
        self.test_box.bay_ok(&self.repo, &["status"])["detail"]["red"].clone()
    }

    /// `good` passes the old check; `grow` makes the check new, and its run, under the old
    /// check, passes; `next` fails under the new one. Returns `grow`'s change and tip and
    /// `next`'s tip.
    fn grown(&self) -> (String, String, String) {
        let (_, good) = self.land("good", &["src/"], &[("src/b.rs", "fn b() {}\n")]);
        assert_eq!(
            verdicts(&self.test_box, &self.remote, "suite", &good),
            ["pass"]
        );
        let revised = policy(2);
        let (grow, grow_tip) = self.land(
            "grow",
            &["src/", ".bay/policy.toml", "scripts/suite-mode"],
            &[
                ("src/c.rs", "fn c() {}\n"),
                (".bay/policy.toml", revised.as_str()),
                ("scripts/suite-mode", "new\n"),
            ],
        );
        assert_eq!(
            verdicts(&self.test_box, &self.remote, "suite", &grow_tip),
            ["pass"],
            "grow is judged with the old check"
        );
        let (_, next) = self.land("next", &["src/"], &[("src/d.rs", "fn d() {}\n")]);
        assert!(
            verdicts(&self.test_box, &self.remote, "suite", &next)
                .iter()
                .all(|verdict| verdict == "product_failure"),
            "next is judged with the new check"
        );
        (grow, grow_tip, next)
    }
}

#[test]
fn a_pass_of_the_old_check_names_no_culprit_and_the_change_of_the_check_owns_the_red() {
    // GIVEN a line that does not bisect.
    let line = Line::new(false);

    // WHEN a pass of the old check sits right below the first failure of the new one.
    let (grow, grow_tip, next) = line.grown();

    // THEN the failure is no culprit, and the merge that changed the check owns the red.
    let red = line.red();
    assert_eq!(red[0]["check"], "suite", "{red}");
    assert_eq!(red[0]["culprit"], Value::Null, "{red}");
    assert_eq!(red[0]["suspect"]["last_passed"], Value::Null, "{red}");
    assert_eq!(
        red[0]["suspect"]["first_failed"]["merge"],
        next.as_str(),
        "{red}"
    );
    assert_eq!(
        red[0]["suspect"]["check_changed"]["merge"],
        grow_tip.as_str(),
        "{red}"
    );
    assert_eq!(red[0]["suspect"]["check_added"], Value::Null, "{red}");
    assert_eq!(red[0]["fixes"], grow.as_str(), "{red}");
    let said = red[0]["say"].as_str().unwrap();
    assert!(
        said.contains("changed the check")
            && said.contains("ran the old check")
            && !said.contains("broke it"),
        "{said}"
    );
}

#[test]
fn a_run_of_the_new_check_on_the_change_that_fails_makes_the_red_the_checks_own() {
    // GIVEN a line that bisects, with a pass of the old check right below the first failure of
    // the new one.
    let line = Line::new(true);
    let (_, grow_tip, _) = line.grown();

    // WHEN the runner runs the new check once on the merge that changed it.
    let mut red = line.red();
    for _ in 0..4 {
        if !red[0]["suspect"]["base_failed"].is_null() {
            break;
        }
        line.pass();
        red = line.red();
    }

    // THEN it failed there as well: the red is the check's own, and no merge is its culprit.
    assert_eq!(
        red[0]["suspect"]["base_failed"]["merge"],
        grow_tip.as_str(),
        "{red}"
    );
    assert_eq!(red[0]["culprit"], Value::Null, "{red}");
    let said = red[0]["say"].as_str().unwrap();
    assert!(
        said.contains("the red is the check's own") && !said.contains("broke it"),
        "{said}"
    );
}
