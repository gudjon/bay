//! G274. A candidate's required checks run in parallel.
//!
//! `[runner] parallel_checks = N` runs up to N of a candidate's checks at once, each in its
//! own work directory; the default, 1, runs them one after another. What a parallel run
//! records and reports is what a serial run does: the records reach the tree in the policy's
//! check order, the same in everything but their timing, and a refusal names the same checks
//! in the same order, however the checks' ends interleave.
//!
//! The guard to mutate is the choice of a parallel run in `check::run_all` (src/check.rs),
//! `if width > 1`: make it `if width > MAX_PARALLEL_CHECKS` and every candidate's checks run
//! serially, so the three 3 s checks below no longer overlap and the land takes 9 s or more.

// Each case runs a check adapter (a shell script), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// The policy's checks, in its order.
const CHECKS: [&str; 3] = ["a", "b", "c"];

/// How long each check sleeps, unless a case says otherwise.
const SLEEP_S: u64 = 3;

/// One `[[check]]` entry on the shared adapter.
fn entry(id: &str) -> String {
    format!(
        "\n[[check]]\nid = \"{id}\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check {id}\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n"
    )
}

/// A trunk whose three checks each log their start, sleep (`sleep` is the shell that does, the
/// check's id in `$1`), log their end, and answer `pass`, or `product_failure` when the
/// candidate holds `fail/<id>`. The log is written through the relay, which runs in the box's
/// root: a check sees nothing of the box, and the adapter names no path of it, so its digest
/// is the same in every box. `config` is the trunk's `.bay/config.toml`. Returns the
/// repository and the log.
fn trunk(test_box: &TestBox, config: &str, sleep: &str) -> (PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    let log = test_box.path().join("checks.log");
    let mark =
        |word: &str| test_box.relay(&format!("echo \"$1 {word} $(date +%s)\" >> checks.log"));
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let before = format!(
        "{start}\n{sleep}\n{end}\n[ -e \"fail/$1\" ] && {{ {failing}; exit 0; }}",
        start = mark("start"),
        end = mark("end"),
    );
    let script = check_files(&before, &passing_result(1), 1, 60)
        .into_iter()
        .find(|(path, _)| path == "scripts/bay-check")
        .unwrap()
        .1;
    let policy: String = std::iter::once("revision = 1\n".to_owned())
        .chain(CHECKS.into_iter().map(entry))
        .collect();
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[
            ("scripts/bay-check".to_owned(), script),
            (".bay/policy.toml".to_owned(), policy),
            (".bay/config.toml".to_owned(), config.to_owned()),
        ],
    );
    (repo, log)
}

/// The same sleep for every check.
fn even() -> String {
    format!("sleep {SLEEP_S}")
}

/// Starts a change on `src/` and `fail/`, writes `files` in its view, and lands it, waiting.
/// Returns the exit code, the result, and how long the land took.
fn land(test_box: &TestBox, repo: &Path, files: &[&str]) -> (i32, Value, Duration) {
    let started = test_box.bay_ok(repo, &["start", "PAR-1", "--paths", "src/", "fail/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/b.rs"), "fn b() {}\n").unwrap();
    for file in files {
        let full = view.join(file);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, "fail\n").unwrap();
    }
    let at = Instant::now();
    let (code, result) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    (code, result, at.elapsed())
}

/// Each check's logged run, (id, start, end) in seconds, in the policy's order.
fn intervals(log: &Path) -> Vec<(String, u64, u64)> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let at = |id: &str, word: &str| -> u64 {
        let found: Vec<u64> = text
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                (fields.next() == Some(id) && fields.next() == Some(word))
                    .then(|| fields.next().unwrap().parse().unwrap())
            })
            .collect();
        assert_eq!(found.len(), 1, "one {word} of {id}: {text}");
        found[0]
    };
    CHECKS
        .iter()
        .map(|id| ((*id).to_owned(), at(id, "start"), at(id, "end")))
        .collect()
}

/// Whether two of the runs overlap: one starts before another ends.
fn overlap(runs: &[(String, u64, u64)]) -> bool {
    runs.iter().enumerate().any(|(i, (_, start, end))| {
        runs.iter()
            .enumerate()
            .any(|(j, (_, other_start, _))| i != j && start <= other_start && other_start < end)
    })
}

/// The records on the landed tree in the remote, in the order they were written, each without
/// what differs from one box to the next: its id (it names the operation), its time, its
/// seconds, its signature and its subject (each box's trunk differs in its config). Returns
/// them with each record's id suffix, its position in the policy.
fn records(test_box: &TestBox, landed: &Value) -> Vec<(String, Value)> {
    let remote = test_box.path().join("repo-remote.git");
    let commit = landed["detail"]["commit"].as_str().unwrap();
    let tree = test_box.git_bare(&remote, &["rev-parse", &format!("{commit}^{{tree}}")]);
    let notes = test_box.git_bare(
        &remote,
        &[
            "notes",
            "--ref=refs/notes/bay/evidence/local",
            "show",
            &tree,
        ],
    );
    let records: Vec<Value> = serde_json::from_str(&notes).unwrap();
    records
        .into_iter()
        .map(|mut record| {
            let id = record["id"].as_str().unwrap().to_owned();
            let suffix = id.rsplit('-').next().unwrap().to_owned();
            let fields = record.as_object_mut().unwrap();
            for key in ["id", "at", "seconds", "signature", "subject"] {
                fields.remove(key);
            }
            (suffix, record)
        })
        .collect()
}

/// The refusal's checks, each as (check, environment, verdict), in its order.
fn refused_checks(refused: &Value) -> Vec<(String, String, String)> {
    refused["detail"]["checks"]
        .as_array()
        .unwrap_or_else(|| panic!("no checks: {refused}"))
        .iter()
        .map(|check| {
            let word = |key: &str| check[key].as_str().unwrap_or_default().to_owned();
            (word("check"), word("environment"), word("verdict"))
        })
        .collect()
}

#[test]
fn three_checks_run_at_once_and_record_what_a_serial_run_records() {
    // The default: one after another.
    let serial_box = TestBox::new();
    let (repo, log) = trunk(&serial_box, "", &even());
    let (code, serial, serial_took) = land(&serial_box, &repo, &[]);
    assert_eq!(code, 0, "{serial}");
    let runs = intervals(&log);
    assert!(
        !overlap(&runs),
        "the default runs checks serially: {runs:?}"
    );
    assert!(
        serial_took >= Duration::from_secs(3 * SLEEP_S),
        "three serial checks took {serial_took:?}"
    );

    // `parallel_checks = 3`: all three at once.
    let parallel_box = TestBox::new();
    let (repo, log) = trunk(&parallel_box, "[runner]\nparallel_checks = 3\n", &even());
    let (code, parallel, parallel_took) = land(&parallel_box, &repo, &[]);
    assert_eq!(code, 0, "{parallel}");
    let runs = intervals(&log);
    assert!(
        overlap(&runs),
        "parallel_checks = 3 overlaps them: {runs:?}"
    );
    // No bound on the parallel land's time: on a loaded box bay's own overhead varies, and the
    // overlap above is what proves the checks ran at once.
    let _ = parallel_took;

    // The records are written in the policy's order, the same as the serial run's.
    let serial = records(&serial_box, &serial);
    let parallel = records(&parallel_box, &parallel);
    let order = |records: &[(String, Value)]| -> Vec<(String, String)> {
        records
            .iter()
            .map(|(suffix, record)| {
                (
                    suffix.clone(),
                    record["check"]["id"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let expected: Vec<(String, String)> = CHECKS
        .iter()
        .enumerate()
        .map(|(n, id)| ((n + 1).to_string(), (*id).to_owned()))
        .collect();
    assert_eq!(order(&serial), expected, "{serial:?}");
    assert_eq!(order(&parallel), expected, "{parallel:?}");
    assert_eq!(parallel, serial, "the same records, but for their timing");
}

#[test]
fn a_parallel_refusal_names_what_a_serial_one_names() {
    // `c` ends first and `b` last, so the checks end out of the policy's order; `b` and `c`
    // fail.
    let sleep = "case \"$1\" in a) sleep 2 ;; b) sleep 3 ;; *) sleep 1 ;; esac";
    let failing = ["fail/b", "fail/c"];
    let serial_box = TestBox::new();
    let (repo, _) = trunk(&serial_box, "", sleep);
    let (code, serial, _) = land(&serial_box, &repo, &failing);
    assert_eq!(code, 10, "{serial}");
    assert_eq!(serial["reason"], "CHECK_FAILED", "{serial}");

    let parallel_box = TestBox::new();
    let (repo, log) = trunk(&parallel_box, "[runner]\nparallel_checks = 3\n", sleep);
    let remote = parallel_box.path().join("repo-remote.git");
    let tip = parallel_box.git_bare(&remote, &["rev-parse", "main"]);
    let (code, parallel, _) = land(&parallel_box, &repo, &failing);
    assert_eq!(code, 10, "{parallel}");
    assert_eq!(parallel["reason"], "CHECK_FAILED", "{parallel}");
    assert!(overlap(&intervals(&log)), "the checks ran at once");
    assert_eq!(
        parallel_box.git_bare(&remote, &["rev-parse", "main"]),
        tip,
        "a failed candidate never lands"
    );
    let named = refused_checks(&parallel);
    assert_eq!(named, refused_checks(&serial), "{parallel}\n{serial}");
    assert_eq!(
        named.first().map(|row| row.0.as_str()),
        Some("b"),
        "{parallel}"
    );
}
