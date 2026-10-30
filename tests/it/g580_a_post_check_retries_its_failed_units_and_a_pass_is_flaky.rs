//! G580 (0.6.6; a fleet finding, 2026-10-30: edge-py-fast, designer-fast and studio-fast each
//! turned main red on one flaky unit overnight, and each red held its paths for one full run).
//! A post check may say `retry_on_failure`: a run on main's tip that fails asks its adapter once
//! more for the units it failed, alone (`retry`, with `rerun` those units). A pass then is
//! `flaky`: it turns no main red, and `bay status` counts it in `flakes`. A retry that fails
//! again opens the red as before, and one that does not report each unit it was asked for is
//! `unknown`, whatever it names in `plan.gone`: the tree is the one that failed them. A flake
//! on a fix of a red check ends no wait: the check stays `pending_fix` until a run passes.
//!
//! Guards to mutate: in `src/check.rs` `retry_of` (the first run's candidate on the retry: the
//! adapter, asked no `retry`, runs every unit, and t2 is in the retry's record); `failed_ids`
//! keeping only failed units (every unit: the retry's record holds t2); `request`'s `retry`
//! field (the adapter runs every unit on the retry); `judge_units`' minimum on a retry (the
//! policy's `expected_minimum` 2: the retry of t1 alone is `unknown`, and the flake turns main
//! red); `plan.gone` answering no unit of a retry (the retry that names t1 gone passes, and the
//! red clears on a flake); in `src/policy.rs` `validate_retry` (a post `retry_on_failure` is
//! refused, and the policy never loads); in `src/status.rs` `flakes` counting post checks (the
//! suite row is missing).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::{TestBox, result_json};

/// A pre check `tests` that always passes, and a post check `suite` on `src/**` that retries
/// once. Its `expected_minimum` is 2, more than a retry of one unit reports.
const POLICY: &str = "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 2\ntimeout_s = 60\nretry_on_failure = 1\nwhen_paths = [\"src/**\"]\n";

/// The adapter. `suite`'s units are the files under `units/`: a unit fails when its file says
/// `fail`, and fails on a first run and passes on a retry when it says `flake`. A change under
/// `src/` reaches every unit; `rerun` adds the units it names. A request with `retry` runs
/// `rerun` alone. On a retry, while `conf/drop` exists the adapter runs t2 instead, and while
/// `conf/gone` exists it runs t2 and names the `rerun` units in `plan.gone`.
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
retry=
case "$req" in *'"retry":'[1-9]*) retry=1 ;; esac
wanted=
case "$changed" in *'"src/'*) wanted=$(ls units) ;; esac
gone=
if [ -n "$retry" ]; then
  wanted=
  if [ -f conf/gone ]; then
    for u in $rerun; do gone="$gone${gone:+,}\"$u\""; done
    wanted=t2
    rerun=
  elif [ -f conf/drop ]; then
    wanted=t2
    rerun=
  fi
fi
units=
n=0
seen=" "
for u in $wanted $rerun; do
  case "$seen" in *" $u "*) continue ;; esac
  seen="$seen$u "
  if [ -f "units/$u" ]; then
    verdict=pass
    case "$(cat "units/$u")" in
    fail) verdict=product_failure ;;
    flake) [ -z "$retry" ] && verdict=product_failure ;;
    esac
    units="$units${units:+,}{\"id\":\"$u\",\"verdict\":\"$verdict\"}"
    n=$((n + 1))
  else
    gone="$gone${gone:+,}\"$u\""
  fi
done
printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"selected":%s,"completed":%s,"gone":[%s]},"checks":[%s]}' "$commit" "$tree" "$n" "$n" "$gone" "$units"
"#;

/// The `suite` records on the remote about `tip`, in the order they were run.
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

/// `suite`'s repository and its remote: t1 flakes and t2 passes.
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
            ("units/t1", "flake\n"),
            ("units/t2", "pass\n"),
        ],
        &files,
    );
    (repo, remote)
}

/// Lands `name` on `src` and `paths` (a fix of `fixes`, when main is red), with `edit` in its
/// view, runs one runner pass, and returns the `suite` records on the new tip and the pass's
/// output.
fn land_and_run(
    test_box: &TestBox,
    (repo, remote): (&Path, &Path),
    (name, paths, fixes): (&str, &[&str], Option<&str>),
    edit: &dyn Fn(&Path),
) -> (Vec<Value>, Value) {
    let mut args = vec!["start", name, "--paths", "src"];
    args.extend(paths);
    if let Some(change) = fixes {
        args.extend(["--fixes", change]);
    }
    let started = test_box.bay_ok(repo, &args);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(format!("src/{name}.rs")), "x\n").unwrap();
    edit(&view);
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    assert!(
        test_box.trunk_change_ids(remote).contains(&id),
        "{name} lands: {merged}"
    );
    let ran = test_box.bay_ok(repo, &["runner", "run", "--once"]);
    let tip = test_box
        .git_bare(remote, &["rev-parse", "main"])
        .trim()
        .to_owned();
    (suite_records(test_box, remote, &tip), ran)
}

#[test]
fn a_post_check_retries_its_failed_units_and_a_pass_is_flaky() {
    let test_box = TestBox::new();
    let (repo, remote) = seeded(&test_box);
    let status = || test_box.bay_ok(&repo, &["status"]);

    // `one` lands on src/: every unit runs and t1 fails. The retry asks for t1 alone, and t1
    // passes: the check is flaky on the tip, and main stays green.
    let (one, ran) = land_and_run(&test_box, (&repo, &remote), ("one", &[], None), &|_| {});
    assert_eq!(one.len(), 2, "suite ran, then retried: {ran}");
    assert_eq!(unit_ids(&one[0]), ["t1", "t2"], "{}", one[0]);
    assert_eq!(one[0]["verdict"], "product_failure", "{}", one[0]);
    assert_eq!(
        unit_ids(&one[1]),
        ["t1"],
        "the retry ran t1 alone: {}",
        one[1]
    );
    assert_eq!(one[1]["verdict"], "pass", "{}", one[1]);
    let green = status();
    assert_eq!(
        green["detail"]["red"],
        json!([]),
        "a flake turns no red: {green}"
    );
    let flakes = green["detail"]["flakes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let suite = flakes
        .iter()
        .find(|row| row["check"] == "suite")
        .unwrap_or_else(|| panic!("status counts the post check's flakes: {green}"));
    assert_eq!(suite["flaky"], 1, "{suite}");
    assert_eq!(suite["retry_on_failure"], 1, "{suite}");

    // `two` adds t3, which fails. The retry asks for t1 and t3: t1 passes and t3 fails again,
    // so the run is a failure and suite is red.
    let (two, ran) = land_and_run(
        &test_box,
        (&repo, &remote),
        ("two", &["units"], None),
        &|view| {
            std::fs::write(view.join("units/t3"), "fail\n").unwrap();
        },
    );
    assert_eq!(two.len(), 2, "suite ran, then retried: {ran}");
    assert_eq!(unit_ids(&two[1]), ["t1", "t3"], "{}", two[1]);
    assert_eq!(two[1]["verdict"], "product_failure", "{}", two[1]);
    let red = status();
    assert_eq!(red["detail"]["red"][0]["check"], "suite", "{red}");
}

#[test]
fn a_retry_that_does_not_report_its_failed_units_is_unknown() {
    let test_box = TestBox::new();
    let (repo, remote) = seeded(&test_box);
    let red = || test_box.bay_ok(&repo, &["status"])["detail"]["red"].clone();
    let names_t1 = |record: &Value| {
        let why = record["why"].as_str().unwrap_or_default();
        assert!(
            why.contains("t1"),
            "the retry names what it missed: {record}"
        );
    };

    // The retry runs t2 instead of t1: it answered no failed unit, so the failure stands.
    let (drop, ran) = land_and_run(
        &test_box,
        (&repo, &remote),
        ("drop", &["conf"], None),
        &|view| {
            std::fs::write(view.join("conf/drop"), "x\n").unwrap();
        },
    );
    assert_eq!(drop.len(), 2, "suite ran, then retried: {ran}");
    assert_eq!(unit_ids(&drop[1]), ["t2"], "{}", drop[1]);
    assert_eq!(drop[1]["verdict"], "unknown", "{}", drop[1]);
    names_t1(&drop[1]);
    assert_eq!(red()[0]["check"], "suite", "the failure stands");

    // The red holds src/, so each next landing is a fix of the red's newest failure.
    let owner = || red()[0]["change"].as_str().unwrap().to_owned();

    // The retry names t1 gone on the tree that just failed it: no answer either.
    let fixes = owner();
    let (gone, ran) = land_and_run(
        &test_box,
        (&repo, &remote),
        ("gone", &["conf"], Some(&fixes)),
        &|view| {
            std::fs::remove_file(view.join("conf/drop")).unwrap();
            std::fs::write(view.join("conf/gone"), "x\n").unwrap();
        },
    );
    assert_eq!(gone.len(), 2, "suite ran, then retried: {ran}");
    assert_eq!(gone[1]["verdict"], "unknown", "{}", gone[1]);
    names_t1(&gone[1]);
    assert_eq!(
        red()[0]["check"],
        "suite",
        "a retry's plan.gone answers nothing"
    );

    // A flake on the fix decides nothing: the check waits for a run that passes, and holds its
    // paths until then (G568, E35).
    let fixes = owner();
    let (flake, ran) = land_and_run(
        &test_box,
        (&repo, &remote),
        ("flake", &["conf"], Some(&fixes)),
        &|view| {
            std::fs::remove_file(view.join("conf/gone")).unwrap();
        },
    );
    assert_eq!(flake.len(), 2, "suite ran, then retried: {ran}");
    assert_eq!(flake[1]["verdict"], "pass", "{}", flake[1]);
    let waits = test_box.bay_ok(&repo, &["status"]);
    let pending = &waits["detail"]["red_pending"][0];
    assert_eq!(pending["check"], "suite", "a flake ends no wait: {waits}");
    assert_eq!(pending["state"], "pending_fix", "{waits}");
}
