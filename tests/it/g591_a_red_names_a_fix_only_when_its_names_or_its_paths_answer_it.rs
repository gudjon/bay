//! G591 (E68, a fleet finding, 2026-10-30: the fix of one red was named the fix of two more,
//! because their suspect ranges held the merge its `Fixes:` named). A red names a landing its
//! fix only when the landing names the red alone (its check, its owning change or its fix
//! name), or names a landing of its suspect range or one it was seen failing on since, and
//! changed a file on the red's paths or the check. Admission stays as it was: a change that
//! names any merge of the range still lands.
//!
//! G594 (E68 again, 2026-10-30: the fix of one red, naming that red's first failure, was named
//! the fix of a second red that had failed on that merge as well). A merge a red was only seen
//! failing on does not name the red alone: another red's first failure or owner can be one.
//!
//! Guards to mutate, in `src/red.rs`: `fix_of` without its path term (a range name off the
//! red's paths makes `suite` wait for a fix that is not its own), `on_paths` always false (the
//! range name on `tools/` is not `tool`'s fix), `fix_of` without `Red::named_by` (the change
//! that names the check `suite` and changes no file on its paths is not its fix), and
//! `Red::named_by` naming the merges a red was seen failing on (`suite`'s fix is `tool`'s).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::{TestBox, result_json};

/// A pre check `tests` that always passes, a post check `suite` on `src/**` and a post check
/// `tool` on `tools/**`.
const POLICY: &str = "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n\n[[check]]\nid = \"tool\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"tools/**\"]\n";

/// The adapter: `suite` fails while `src/broken` is there and `src/fixed` is not, and `tool`
/// the same in `tools/`; every other check passes.
const ADAPTER: &str = r#"#!/bin/sh
req=$(cat)
commit=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\1/p')
tree=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\2/p')
dir=
case "$req" in
*'"plan":{"id":"suite"'*) dir=src ;;
*'"plan":{"id":"tool"'*) dir=tools ;;
esac
verdict=pass
[ -n "$dir" ] && [ -f "$dir/broken" ] && [ ! -f "$dir/fixed" ] && verdict=product_failure
printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"selected":1,"completed":1},"checks":[{"id":"t","verdict":"%s"}]}' "$commit" "$tree" "$verdict"
"#;

struct Line {
    test_box: TestBox,
    repo: PathBuf,
}

impl Line {
    /// The code in `src`, the tools in `tools`, and a runner here that runs post checks.
    fn new() -> Self {
        let test_box = TestBox::new();
        let repo = test_box.repo_any("repo");
        let files = vec![
            ("scripts/bay-check".to_owned(), ADAPTER.to_owned()),
            (".bay/policy.toml".to_owned(), POLICY.to_owned()),
            (
                ".bay/config.toml".to_owned(),
                "[runner]\npost = true\nbisect_post = false\n".to_owned(),
            ),
        ];
        test_box.seed_with_check(
            &repo,
            &[("src/a.rs", "fn a() {}\n"), ("tools/t.sh", "true\n")],
            &files,
        );
        Self { test_box, repo }
    }

    /// Starts change `name` on the folders of `files`, naming `fixed` when given, writes each
    /// file, and merges it, waiting; asserts it lands: its id.
    fn lands(&self, name: &str, files: &[&str], fixed: Option<&str>) -> String {
        let mut folders: Vec<String> = files
            .iter()
            .map(|file| format!("{}/", Path::new(file).parent().unwrap().display()))
            .collect();
        folders.sort();
        folders.dedup();
        let mut args = vec!["start", name, "--paths"];
        args.extend(folders.iter().map(String::as_str));
        if let Some(fixed) = fixed {
            args.extend(["--fixes", fixed]);
        }
        let started = self.test_box.bay_ok(&self.repo, &args);
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        for file in files {
            let path = view.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("{name}\n")).unwrap();
        }
        let (code, merged) = result_json(self.test_box.bay(&view).args(["merge", "--wait"]));
        assert_eq!(code, 0, "{merged}");
        assert_eq!(merged["decision"], "merged", "{name} lands: {merged}");
        started["change"].as_str().unwrap().to_owned()
    }

    /// Two runner passes here: a pass runs one post check of an environment on main's tip, so
    /// the second runs the other. Both results.
    fn run_post(&self) -> Value {
        let runs: Vec<Value> = (0..2)
            .map(|_| {
                self.test_box
                    .bay_ok(&self.repo, &["runner", "run", "--once"])
            })
            .collect();
        Value::Array(runs)
    }

    fn status(&self) -> Value {
        self.test_box.bay_ok(&self.repo, &["status"])["detail"].clone()
    }
}

/// The checks of `rows` (`detail.red` or `detail.red_pending`), each with the change its fix
/// names, sorted by check.
fn fixes(rows: &Value) -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["check"].as_str().unwrap().to_owned(),
                row["fix"]["change"].clone(),
            )
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn a_red_names_a_fix_only_when_its_names_or_its_paths_answer_it() {
    let line = Line::new();

    // Both checks pass on `zero`; `one` changes nothing they fail on, and `two` breaks both.
    // Each red's suspect range is `one` and `two`; `two` is the owner and the name to fix.
    line.lands("zero", &["src/zero.rs", "tools/zero.sh"], None);
    let ran = line.run_post();
    assert_eq!(line.status()["red"], json!([]), "both pass on zero: {ran}");
    let one = line.lands("one", &["src/one.rs", "tools/one.sh"], None);
    let two = line.lands("two", &["src/broken", "tools/broken"], None);
    let ran = line.run_post();
    let status = line.status();
    assert_eq!(
        fixes(&status["red"]),
        vec![
            ("suite".to_owned(), Value::Null),
            ("tool".to_owned(), Value::Null)
        ],
        "both red: {ran} {status}"
    );
    for red in status["red"].as_array().unwrap() {
        assert_eq!(red["fixes"], two.as_str(), "{red}");
    }

    // `three` names `one`, a merge both ranges hold, and fixes only the tools: it lands, and it
    // is `tool`'s fix, not `suite`'s.
    let three = line.lands("three", &["tools/fixed"], Some(&one));
    let status = line.status();
    assert_eq!(
        fixes(&status["red"]),
        vec![("suite".to_owned(), Value::Null)],
        "suite still red, with no fix: {status}"
    );
    assert_eq!(
        fixes(&status["red_pending"]),
        vec![("tool".to_owned(), json!(three))],
        "tool waits for the run on its fix: {status}"
    );

    // `four` names the check `suite` and changes no file on its paths: the name is the red's
    // alone, so it is `suite`'s fix.
    let four = line.lands("four", &["docs/four.md"], Some("suite"));
    let status = line.status();
    assert_eq!(status["red"], json!([]), "{status}");
    assert_eq!(
        fixes(&status["red_pending"]),
        vec![
            ("suite".to_owned(), json!(four)),
            ("tool".to_owned(), json!(three))
        ],
        "{status}"
    );
}

#[test]
fn a_fix_that_names_one_reds_first_failure_is_not_the_fix_of_a_red_that_failed_on_it_too() {
    let line = Line::new();
    line.lands("zero", &["src/zero.rs", "tools/zero.sh"], None);
    let ran = line.run_post();
    assert_eq!(line.status()["red"], json!([]), "both pass on zero: {ran}");

    // `one` breaks `tool`, and `two` breaks `suite`; `three` names the check `tool` to land on
    // its paths, and `tool` fails on it again. `two` is `suite`'s first failure and the name
    // to fix it, and a merge `tool` was seen failing on.
    let one = line.lands("one", &["tools/broken"], None);
    let ran = line.run_post();
    assert_eq!(
        fixes(&line.status()["red"]),
        vec![("tool".to_owned(), Value::Null)],
        "{ran}"
    );
    let two = line.lands("two", &["src/broken"], None);
    line.run_post();
    let three = line.lands("three", &["tools/three.sh"], Some("tool"));
    let ran = line.run_post();
    let status = line.status();
    let reds: Vec<&Value> = status["red"].as_array().unwrap().iter().collect();
    let red = |check: &str| {
        *reds
            .iter()
            .find(|red| red["check"] == check)
            .unwrap_or_else(|| panic!("{check} red: {ran} {status}"))
    };
    assert_eq!(red("suite")["fixes"], two.as_str(), "{status}");
    assert_eq!(red("tool")["fixes"], one.as_str(), "{status}");
    let seen: Vec<&Value> = red("tool")["suspect"]["failing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|mark| &mark["change"])
        .collect();
    assert_eq!(seen, vec![&json!(two), &json!(three)], "{status}");

    // `four` names `two` and fixes only the code: it is `suite`'s fix, and `tool` stays red
    // with no fix.
    let four = line.lands("four", &["src/fixed"], Some(&two));
    let status = line.status();
    assert_eq!(
        fixes(&status["red"]),
        vec![("tool".to_owned(), Value::Null)],
        "tool still red, with no fix: {status}"
    );
    assert_eq!(
        fixes(&status["red_pending"]),
        vec![("suite".to_owned(), json!(four))],
        "suite waits for the run on its fix: {status}"
    );
}
