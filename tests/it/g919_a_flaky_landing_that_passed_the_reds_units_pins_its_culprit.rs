//! G919 (BACKLOG 340, a fleet finding, 2026-11-26: a merge that changed one file of a red
//! check's read set, flaky there on a timing test, unpinned a culprit whose five runs all failed
//! clippy, so the auto-undo never came). A landing that answers a red post check `flaky` is that
//! red's pass when no run there failed a unit the red's failing landings failed: the units the red
//! fails passed there, so the merge above it is the culprit. A failure there of one of those units,
//! or a failure that names no unit, says nothing, and the walk goes on to the pass below.
//!
//! The post check `suite` has two units: `t1` fails a tree with `src/bad.rs`, and each unit
//! fails every run while the box names it in `G919_SUITE`. `[runner] bisect_post` runs a
//! culprit that stands on one record once more (G627), which makes `card` flaky.
//!
//! Guards to mutate: in `src/red.rs`, `passes_units` always `false` (the culprit is not
//! pinned), or with no unit compared (the control pins `bad`). `evidence::failed_units`' `None`
//! for a failure that names no unit has its own unit test.

// Each case runs shell scripts (the adapter), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::{TestBox, result_json};

/// A pre check `tests` that always passes, and a post check `suite` on `src/**` and
/// `notes/**`.
const POLICY: &str = "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nenv = [\"G919_SUITE\"]\nwhen_paths = [\"src/**\", \"notes/**\"]\n";

/// The adapter. `suite` runs units `t1` and `t2`: `t1` fails a tree with `src/bad.rs`, and a
/// unit fails while `G919_SUITE` names it.
const ADAPTER: &str = r#"#!/bin/sh
req=$(cat)
commit=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\1/p')
tree=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\2/p')
case "$req" in
*'"plan":{"id":"suite"'*) ;;
*) printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"selected":1,"completed":1},"checks":[{"id":"t0","verdict":"pass"}]}' "$commit" "$tree"; exit 0 ;;
esac
t1=pass
t2=pass
if [ -f src/bad.rs ] || [ "$G919_SUITE" = t1 ]; then t1=product_failure; fi
if [ "$G919_SUITE" = t2 ]; then t2=product_failure; fi
printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"selected":2,"completed":2},"checks":[{"id":"t1","verdict":"%s"},{"id":"t2","verdict":"%s"}]}' "$commit" "$tree" "$t1" "$t2"
"#;

/// The records of post check `check` on the remote about `tip`.
fn records(test_box: &TestBox, remote: &Path, check: &str, tip: &str) -> Vec<Value> {
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
            out.extend(records.into_iter().filter(|record| {
                record["check"]["id"] == check && record["subject"]["commit"] == tip
            }));
        }
    }
    out
}

struct Line {
    test_box: TestBox,
    repo: PathBuf,
    remote: PathBuf,
    /// `G919_SUITE`, which every bay command that may run `suite` sets.
    suite: std::cell::Cell<&'static str>,
}

impl Line {
    fn new() -> Self {
        let test_box = TestBox::new();
        let repo = test_box.repo_any("repo");
        let remote = test_box.path().join("repo-remote.git");
        let files = vec![
            ("scripts/bay-check".to_owned(), ADAPTER.to_owned()),
            (".bay/policy.toml".to_owned(), POLICY.to_owned()),
            (
                ".bay/config.toml".to_owned(),
                "[runner]\npost = true\nbisect_post = true\n".to_owned(),
            ),
        ];
        test_box.seed_with_check(
            &repo,
            &[
                ("src/a.rs", "fn a() {}\n"),
                ("notes/a.md", "a\n"),
                ("docs/a.md", "a\n"),
            ],
            &files,
        );
        Self {
            test_box,
            repo,
            remote,
            suite: std::cell::Cell::new("steady"),
        }
    }

    /// One runner pass, with `G919_SUITE` as the line holds it.
    fn pass(&self) {
        let (code, pass) = result_json(
            self.test_box
                .bay(&self.repo)
                .env("G919_SUITE", self.suite.get())
                .args(["runner", "run", "--once"]),
        );
        assert_eq!(code, 0, "{pass}");
    }

    /// Lands change `name` on `paths`, writing `written` in its view, runs passes until `suite`
    /// has a record on the new tip, and returns it.
    fn land(&self, name: &str, paths: &[&str], written: &[(&str, &str)]) -> String {
        let mut args = vec!["start", name, "--paths"];
        args.extend(paths);
        let started = self.test_box.bay_ok(&self.repo, &args);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        for (path, text) in written {
            std::fs::write(view.join(path), text).unwrap();
        }
        let (code, merged) = result_json(
            self.test_box
                .bay(&view)
                .env("G919_SUITE", self.suite.get())
                .args(["merge", "--wait"]),
        );
        assert_eq!(code, 0, "{merged}");
        assert!(
            self.test_box.trunk_change_ids(&self.remote).contains(&id),
            "{name} lands: {merged}"
        );
        let tip = self
            .test_box
            .git_bare(&self.remote, &["rev-parse", "main"])
            .trim()
            .to_owned();
        for _ in 0..3 {
            if !records(&self.test_box, &self.remote, "suite", &tip).is_empty() {
                return tip;
            }
            self.pass();
        }
        panic!("no run of suite on {name}'s tip");
    }

    /// Runs up to `passes` runner passes, until `suite` has `count` records on `tip`, and
    /// returns their verdicts, sorted.
    fn until(&self, tip: &str, count: usize, passes: usize) -> Vec<Value> {
        for _ in 0..passes {
            if records(&self.test_box, &self.remote, "suite", tip).len() >= count {
                break;
            }
            self.pass();
        }
        let mut verdicts: Vec<Value> = records(&self.test_box, &self.remote, "suite", tip)
            .iter()
            .map(|record| record["verdict"].clone())
            .collect();
        verdicts.sort_by_key(ToString::to_string);
        verdicts
    }

    /// `status`'s `detail.red`.
    fn red(&self) -> Value {
        self.test_box.bay_ok(&self.repo, &["status"])["detail"]["red"].clone()
    }

    /// `good`, which passes `suite`, then `card` on notes/, whose first run fails as
    /// `G919_SUITE=failing` makes it and whose second run passes: `card` is flaky.
    fn flaky_card(&self, failing: &'static str) -> (String, String) {
        let good = self.land("good", &["src/"], &[("src/b.rs", "fn b() {}\n")]);
        assert_eq!(self.until(&good, 1, 0), vec![json!("pass")]);
        self.suite.set(failing);
        let card = self.land("card", &["notes/"], &[("notes/card.md", "c\n")]);
        self.suite.set("steady");
        assert_eq!(
            self.until(&card, 2, 4),
            vec![json!("pass"), json!("product_failure")],
            "card is flaky"
        );
        assert_eq!(self.red(), json!([]), "a flaky landing is no red");
        (good, card)
    }
}

#[test]
fn a_flaky_landing_that_passed_the_reds_units_pins_its_culprit() {
    // GIVEN `card`, flaky on unit t2 alone, over `good`.
    let line = Line::new();
    let (_good, card) = line.flaky_card("t2");

    // WHEN `bad` adds src/bad.rs, so every run of it fails t1.
    let bad = line.land("bad", &["src/"], &[("src/bad.rs", "x\n")]);

    // THEN no run at `card` failed t1, so `card` is the red's pass and `bad` its culprit.
    let red = line.red();
    assert_eq!(red[0]["check"], "suite", "{red}");
    assert_eq!(red[0]["culprit"]["merge"], bad.as_str(), "{red}");
    assert_eq!(
        red[0]["suspect"]["last_passed"]["merge"],
        card.as_str(),
        "{red}"
    );
    assert_eq!(red[0]["suspect"]["passed_units"], json!(["t1"]), "{red}");
    let said = red[0]["say"].as_str().unwrap();
    assert!(
        said.contains("broke it") && said.contains("flaky") && said.contains("t1"),
        "{said}"
    );
}

#[test]
fn a_flaky_landing_that_failed_a_reds_unit_pins_nothing() {
    // GIVEN `card`, flaky on unit t1, the unit `bad` breaks.
    let line = Line::new();
    let (good, _card) = line.flaky_card("t1");

    // WHEN `bad` adds src/bad.rs, so every run of it fails t1.
    let bad = line.land("bad", &["src/"], &[("src/bad.rs", "x\n")]);

    // THEN `card` says nothing of t1: the range runs down to `good`, and nothing is pinned.
    let red = line.red();
    assert_eq!(red[0]["merge"], bad.as_str(), "{red}");
    assert_eq!(red[0]["culprit"], Value::Null, "{red}");
    assert_eq!(
        red[0]["suspect"]["last_passed"]["merge"],
        good.as_str(),
        "{red}"
    );
    assert_eq!(red[0]["suspect"]["passed_units"], Value::Null, "{red}");
}
