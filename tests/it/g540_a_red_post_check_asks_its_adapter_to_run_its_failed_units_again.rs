//! G540 (a fleet finding, 2026-10-29: scripts-fast failed on 3 units and stayed red). A red post
//! check's request names the units that failed in its newest failing record (`rerun`), and a run
//! that does not answer each of them is never green.
//!
//! A red check runs again on each new tip (G492), but its request named only the paths changed
//! since this box last ran it (G439). The fix landed off its paths, so an adapter that picks its
//! tests by changed paths reached no test, emitted no unit and answered `unknown`, and the red
//! stood. Now the request names `rerun`: the adapter runs those units too, and names in
//! `plan.gone` each one it no longer has. A run that reports a requested unit in neither is
//! `unknown`, never a pass that clears the red.
//!
//! Guards to mutate: in `src/bisect.rs` `tips`, the red check's `rerun` (an empty one: the
//! fix reaches no test, and the deaf run's pass clears the red); `failed_units` keeping only the
//! failed units (every unit: the fix runs t2 as well); in `src/check.rs` `judge_units`, the
//! unanswered-rerun refusal (the deaf run's pass clears the red), and `plan.gone` counting as an
//! answer (the fix that deletes t3 stays `unknown`).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::{TestBox, result_json};

/// A pre check `tests` that always passes, and a post check `suite` on `src/**`.
const POLICY: &str = "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

/// The adapter. `suite`'s units are the files under `units/`; a unit fails when its file says
/// `fail`. A change under `src/` reaches every unit; the request's `rerun` adds the units it
/// names, and one the tree no longer has is named in `plan.gone`. While `conf/deaf` exists the
/// adapter ignores `rerun` and runs t2 alone, as one that never read the field would.
const ADAPTER: &str = r#"#!/bin/sh
req=$(cat)
commit=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\1/p')
tree=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\2/p')
case "$req" in
*'"plan":{"id":"suite"'*) ;;
*) printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"selected":1,"completed":1},"checks":[{"id":"t0","verdict":"pass"}]}' "$commit" "$tree"; exit 0 ;;
esac
changed=$(printf '%s' "$req" | sed -n 's/.*"changed_paths":\[\([^]]*\)\].*/\1/p')
rerun=$(printf '%s' "$req" | sed -n 's/.*"rerun":\[\([^]]*\)\].*/\1/p' | tr -d '"' | tr ',' ' ')
wanted=
case "$changed" in *'"src/'*) wanted=$(ls units) ;; esac
if [ -f conf/deaf ]; then wanted=t2; rerun=; fi
units=
gone=
n=0
seen=" "
for u in $wanted $rerun; do
  case "$seen" in *" $u "*) continue ;; esac
  seen="$seen$u "
  if [ -f "units/$u" ]; then
    verdict=pass
    [ "$(cat "units/$u")" = fail ] && verdict=product_failure
    units="$units${units:+,}{\"id\":\"$u\",\"verdict\":\"$verdict\"}"
    n=$((n + 1))
  else
    gone="$gone${gone:+,}\"$u\""
  fi
done
printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"selected":%s,"completed":%s,"gone":[%s]},"checks":[%s]}' "$commit" "$tree" "$n" "$n" "$gone" "$units"
"#;

/// The `suite` records on the remote about `tip`.
fn suite_records(test_box: &TestBox, remote: &Path, tip: &str) -> Vec<Value> {
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
                record["check"]["id"] == "suite" && record["subject"]["commit"] == tip
            }));
        }
    }
    out
}

fn unit_ids(record: &Value) -> Vec<String> {
    record["units"]
        .as_array()
        .unwrap()
        .iter()
        .map(|unit| unit["id"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn a_red_post_check_asks_its_adapter_to_run_its_failed_units_again() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
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
        &[
            ("src/a.rs", "fn a() {}\n"),
            ("conf/readme", "conf\n"),
            ("units/t1", "fail\n"),
            ("units/t2", "pass\n"),
            ("units/t3", "fail\n"),
        ],
        &files,
    );
    let submit = |name: &str, paths: &[&str], edit: &dyn Fn(&Path)| -> String {
        let mut args = vec!["start", name, "--paths"];
        args.extend(paths);
        let started = test_box.bay_ok(&repo, &args);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        edit(&view);
        let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
        assert_eq!(code, 0, "{merged}");
        assert!(
            test_box.trunk_change_ids(&remote).contains(&id),
            "{name} lands: {merged}"
        );
        id
    };
    let tip = || {
        test_box
            .git_bare(&remote, &["rev-parse", "main"])
            .trim()
            .to_owned()
    };
    let red = || test_box.bay_ok(&repo, &["status"])["detail"]["red"].clone();

    // `one` lands on src/: every unit runs, and t1 and t3 fail. `suite` is red.
    submit("one", &["src"], &|view| {
        std::fs::write(view.join("src/one.rs"), "x\n").unwrap();
    });
    let ran = test_box.bay_ok(&repo, &["runner", "run", "--once"]);
    assert_eq!(red()[0]["check"], "suite", "suite is red: {ran}");

    // An adapter deaf to `rerun` runs t2 alone after a landing off suite's paths, and passes:
    // it answered neither failed unit, so the run is no pass and the red stands.
    submit("deaf", &["conf"], &|view| {
        std::fs::write(view.join("conf/deaf"), "x\n").unwrap();
    });
    let ran = test_box.bay_ok(&repo, &["runner", "run", "--once"]);
    let deaf = suite_records(&test_box, &remote, &tip());
    assert_eq!(deaf.len(), 1, "suite ran on the new tip: {ran}");
    assert_eq!(deaf[0]["post"]["rerun"], json!(["t1", "t3"]), "{}", deaf[0]);
    assert_eq!(unit_ids(&deaf[0]), ["t2"], "{}", deaf[0]);
    assert_eq!(deaf[0]["verdict"], "unknown", "{}", deaf[0]);
    let why = deaf[0]["why"].as_str().unwrap_or_default();
    assert!(
        why.contains("t1") && why.contains("t3"),
        "the run names what it missed: {why}"
    );
    assert_eq!(
        red()[0]["check"],
        "suite",
        "a run that answered no failed unit clears nothing"
    );

    // The fix lands off suite's paths: t1 passes now, and t3 is deleted. The adapter runs
    // exactly the requested units: t1, and t3, which it names gone. Main is green.
    submit("fix", &["units", "conf"], &|view| {
        std::fs::write(view.join("units/t1"), "pass\n").unwrap();
        std::fs::remove_file(view.join("units/t3")).unwrap();
        std::fs::remove_file(view.join("conf/deaf")).unwrap();
    });
    let ran = test_box.bay_ok(&repo, &["runner", "run", "--once"]);
    let fixed = suite_records(&test_box, &remote, &tip());
    assert_eq!(fixed.len(), 1, "suite ran on the fix: {ran}");
    assert_eq!(
        fixed[0]["post"]["rerun"],
        json!(["t1", "t3"]),
        "{}",
        fixed[0]
    );
    assert_eq!(
        unit_ids(&fixed[0]),
        ["t1"],
        "only the requested units ran: {}",
        fixed[0]
    );
    assert_eq!(
        fixed[0]["gone"],
        json!(["t3"]),
        "the record keeps the gone unit: {}",
        fixed[0]
    );
    assert_eq!(fixed[0]["verdict"], "pass", "{}", fixed[0]);
    assert_eq!(
        red(),
        json!([]),
        "the fix off the red's paths clears it: {ran}"
    );
}
