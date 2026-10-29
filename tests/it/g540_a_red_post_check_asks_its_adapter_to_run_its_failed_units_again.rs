//! G540 (a fleet finding, 2026-10-29: scripts-fast failed on 3 units and stayed red). A red post
//! check's request names the units that failed in its newest failing record (`rerun`), and a run
//! that does not answer each of them is never green.
//!
//! A red check runs again on each new tip (G492), and its request names the paths changed since
//! its newest pass (G495). The fix landed off its paths, so an adapter that picks its tests by
//! changed paths reached none of the failed units unless that range chose them, and the red
//! stood. Now the request names `rerun`: the adapter runs those units too, and names in
//! `plan.gone` each one it no longer has. A run that reports a requested unit in neither is
//! `unknown`, never a pass that clears the red.
//!
//! E39 (a fleet finding, 2026-10-29): a check whose fix waits for its run (`pending_fix`) is asked
//! the same, so the run that answers a `Fixes:` landing runs the failed units.
//!
//! Guards to mutate: in `src/bisect.rs` `tips`, `crate::red::holding` (`crate::red::red`: the
//! check whose fix waits is not run on the fix, and stays `pending_fix`); the red check's `rerun` (an empty one: the
//! fix answers no requested unit, and the deaf run's pass clears the red); `failed_units` keeping
//! only the failed units (every unit: `rerun` names t2 as well); in `src/check.rs` `judge_units`, the
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

/// `suite`'s repository and its remote: units t1 and t3 fail, and t2 passes.
fn seeded(test_box: &TestBox) -> (PathBuf, PathBuf) {
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
    (repo, remote)
}

/// Runs `bay start` with `start`, edits the view, and waits until the change lands.
fn land(
    test_box: &TestBox,
    (repo, remote): (&Path, &Path),
    start: &[&str],
    edit: &dyn Fn(&Path),
) -> String {
    let mut args = vec!["start"];
    args.extend(start);
    let started = test_box.bay_ok(repo, &args);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    edit(&view);
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    assert!(
        test_box.trunk_change_ids(remote).contains(&id),
        "{} lands: {merged}",
        start[0]
    );
    id
}

#[test]
fn a_red_post_check_asks_its_adapter_to_run_its_failed_units_again() {
    let test_box = TestBox::new();
    let (repo, remote) = seeded(&test_box);
    let submit = |name: &str, paths: &[&str], edit: &dyn Fn(&Path)| {
        let mut start = vec![name, "--paths"];
        start.extend(paths);
        land(&test_box, (&repo, &remote), &start, edit)
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

    // The fix lands off suite's paths: t1 passes now, and t3 is deleted. Suite's range since
    // its last pass still holds `one`'s src/ change (G495), so the adapter runs t1 and t2 for
    // it, and the requested t1, and t3, which it names gone. Main is green.
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
        ["t1", "t2"],
        "the range's units and the requested ones ran: {}",
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

#[test]
fn a_check_whose_fix_waits_for_its_run_asks_for_its_failed_units_again() {
    let test_box = TestBox::new();
    let (repo, remote) = seeded(&test_box);
    let status = || test_box.bay_ok(&repo, &["status"]);
    let one = land(
        &test_box,
        (&repo, &remote),
        &["one", "--paths", "src"],
        &|view| {
            std::fs::write(view.join("src/one.rs"), "x\n").unwrap();
        },
    );
    let ran = test_box.bay_ok(&repo, &["runner", "run", "--once"]);
    assert_eq!(
        status()["detail"]["red"][0]["check"],
        "suite",
        "suite is red: {ran}"
    );

    // The fix names `one` and lands off suite's paths, so suite waits for a run on it. That run
    // is asked for t1 and t3, and for the paths since suite's last pass, which hold `one`'s src/.
    land(
        &test_box,
        (&repo, &remote),
        &["fix", "--paths", "units", "--fixes", &one],
        &|view| {
            std::fs::write(view.join("units/t1"), "pass\n").unwrap();
            std::fs::write(view.join("units/t3"), "pass\n").unwrap();
        },
    );
    let waits = status();
    assert_eq!(
        waits["detail"]["red_pending"][0]["state"], "pending_fix",
        "{waits}"
    );
    let ran = test_box.bay_ok(&repo, &["runner", "run", "--once"]);
    let tip = test_box
        .git_bare(&remote, &["rev-parse", "main"])
        .trim()
        .to_owned();
    let fixed = suite_records(&test_box, &remote, &tip);
    assert_eq!(fixed.len(), 1, "suite ran on the fix: {ran}");
    assert_eq!(
        fixed[0]["post"]["rerun"],
        json!(["t1", "t3"]),
        "{}",
        fixed[0]
    );
    assert_eq!(unit_ids(&fixed[0]), ["t1", "t2", "t3"], "{}", fixed[0]);
    assert_eq!(fixed[0]["verdict"], "pass", "{}", fixed[0]);
    let green = status();
    assert_eq!(green["detail"]["red"], json!([]), "{green}");
    assert!(
        !green["detail"]["red_pending"]
            .to_string()
            .contains("pending_fix"),
        "the pass on the fix ends the wait: {green}"
    );
}
