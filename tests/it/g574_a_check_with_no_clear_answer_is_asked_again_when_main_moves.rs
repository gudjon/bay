//! G574 (a fleet finding, 2026-10-29: a pre check ran on its author's machine and printed no
//! result, so the change was blocked `CHECK_UNKNOWN`; a runner asked again only a block that
//! waits for an approval, and the change waited for its author while its result said to wait).
//! A runner pass asks a `CHECK_UNKNOWN` block again, as it asks an approval (G284): once main
//! moved it is re-armed and checked on the new main, up to `UNKNOWN_RETRIES` candidates; then it
//! waits for its author.
//!
//! The pre check gives no result for a tree with `unknown/` while `conf/state` says `away`.
//!
//! Guards to mutate, in `src/publish.rs` `asked_again`: drop the `CheckUnknown` arm (the change
//! waits after main moves), or the bound (a fourth candidate is checked).

// Each case runs shell scripts (the adapter), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use serde_json::Value;
use support::{TestBox, result_json};

/// A pre check `tests` on every path.
const POLICY: &str = "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n";

/// The adapter: no result for a tree with `unknown/` while `conf/state` is `away`; else a pass.
const ADAPTER: &str = r#"#!/bin/sh
req=$(cat)
commit=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\1/p')
tree=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\2/p')
if [ -d unknown ] && [ "$(cat conf/state)" = away ]; then
  exit 0
fi
printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"selected":1,"completed":1},"checks":[{"id":"t1","verdict":"pass"}]}' "$commit" "$tree"
"#;

struct Line {
    test_box: TestBox,
    repo: PathBuf,
    remote: PathBuf,
}

impl Line {
    fn new() -> Self {
        let test_box = TestBox::new();
        let repo = test_box.repo_any("repo");
        let remote = test_box.path().join("repo-remote.git");
        let files = vec![
            ("scripts/bay-check".to_owned(), ADAPTER.to_owned()),
            (".bay/policy.toml".to_owned(), POLICY.to_owned()),
        ];
        test_box.seed_with_check(
            &repo,
            &[("src/a.rs", "fn a() {}\n"), ("conf/state", "away\n")],
            &files,
        );
        Self {
            test_box,
            repo,
            remote,
        }
    }

    /// Opens change `name` on `dir/`, writes `dir/<name>` with `text`, and merges it with
    /// `--wait`, run here as its author runs it: the result.
    fn merge(&self, name: &str, dir: &str, text: &str) -> (String, Value) {
        let started = self
            .test_box
            .bay_ok(&self.repo, &["start", name, "--paths", &format!("{dir}/")]);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        let path = view.join(format!("{dir}/{name}"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        let (_, merged) = result_json(self.test_box.bay(&view).args(["merge", "--wait"]));
        (id, merged)
    }

    /// Lands change `name` on `dir/`, writing `text`.
    fn land(&self, name: &str, dir: &str, text: &str) {
        let (id, merged) = self.merge(name, dir, text);
        assert!(self.on_main(&id), "{name} lands: {merged}");
    }

    fn on_main(&self, id: &str) -> bool {
        self.test_box
            .trunk_change_ids(&self.remote)
            .contains(&id.to_owned())
    }

    /// One runner pass.
    fn pass(&self) -> Value {
        let (code, ran) = result_json(
            self.test_box
                .bay(&self.repo)
                .args(["runner", "run", "--once"]),
        );
        assert_eq!(code, 0, "{ran}");
        ran
    }

    /// The last operation of change `id`: its outcome, and how many of its candidates the
    /// check gave no clear answer.
    fn op(&self, id: &str) -> (Value, usize) {
        let text = self.test_box.git_ok(
            &self.repo,
            &[
                "cat-file",
                "blob",
                &format!("refs/bay/changes/{id}:change.json"),
            ],
        );
        let record: Value = serde_json::from_str(&text).unwrap();
        let op = record["operations"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        let unknown = op["attempts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|attempt| attempt["verdict"] == "unknown")
            .count();
        (op["outcome"].clone(), unknown)
    }
}

/// `stuck`, on `unknown/`, merged by its author while the check gives no result: blocked
/// `CHECK_UNKNOWN` on one candidate.
fn stuck(line: &Line) -> String {
    let (id, merged) = line.merge("stuck", "unknown", "x\n");
    assert_eq!(merged["reason"], "CHECK_UNKNOWN", "{merged}");
    assert_eq!(line.op(&id), (Value::from("CHECK_UNKNOWN"), 1));
    id
}

#[test]
fn a_check_with_no_clear_answer_is_asked_again_once_main_moves_and_lands() {
    // GIVEN a change blocked `CHECK_UNKNOWN`, and a pass while main stands still.
    let line = Line::new();
    let id = stuck(&line);
    let ran = line.pass();
    assert_eq!(
        line.op(&id),
        (Value::from("CHECK_UNKNOWN"), 1),
        "an unchanged block runs nothing: {ran}"
    );

    // WHEN main moves to a state the check answers.
    line.land("state", "conf", "pass\n");

    // THEN a pass asks the change again on the new main, and it lands.
    let mut ran = Vec::new();
    for _ in 0..3 {
        if line.on_main(&id) {
            break;
        }
        ran.push(line.pass());
    }
    assert!(
        line.on_main(&id),
        "the change waited after main moved: {ran:?}"
    );
}

#[test]
fn a_check_with_no_clear_answer_is_asked_again_at_most_three_times() {
    // GIVEN a change blocked `CHECK_UNKNOWN`, while the check stays without an answer.
    let line = Line::new();
    let id = stuck(&line);

    // WHEN main moves four times, two passes after each.
    let mut ran = Vec::new();
    for name in ["d1", "d2", "d3", "d4"] {
        line.land(name, "docs", "x\n");
        ran.push(line.pass());
        ran.push(line.pass());
    }

    // THEN it was checked on three candidates in all, and waits for its author.
    assert_eq!(line.op(&id), (Value::from("CHECK_UNKNOWN"), 3), "{ran:?}");
}
