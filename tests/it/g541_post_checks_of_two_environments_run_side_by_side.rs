//! G541 (#123, a fleet finding, 2026-10-29). The post checks of different environments run side
//! by side: a slow post check in one environment never holds back a post check of another.
//!
//! A runner did one piece of post work a pass, on the pass's own thread. A post check that ran
//! for an hour in one environment held the post checks of every other environment, and every
//! pass of that runner, for that hour. Now a pass starts at most one piece in each environment
//! the box serves and no running piece holds, each on its own thread, and collects the verdict
//! of each piece when it ends.
//!
//! Here `slow` (environment `env-a`, first in the policy) waits until the test lets it go, and
//! `fast` (`env-b`) records its verdict while `slow` still runs.
//!
//! Guards to mutate: in `src/bisect.rs` `tips`, stop after the first piece (`fast` waits for a
//! later pass, behind `slow`); in `src/executor.rs` `post_pass`, run each piece on the pass's own
//! thread (`fast` starts only once `slow` ends).

#![cfg(unix)]

use crate::support;

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::{TestBox, result_json};

/// A pre check `tests`, and two post checks on `src/**`: `slow` in `env-a`, then `fast` in
/// `env-b`.
const POLICY: &str = "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n\n[[check]]\nid = \"slow\"\nenvironment = \"env-a\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 150\nwhen_paths = [\"src/**\"]\n\n[[check]]\nid = \"fast\"\nenvironment = \"env-b\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

const HEAD: &str = r#"#!/bin/sh
req=$(cat)
commit=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\1/p')
tree=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\2/p')
"#;

const TAIL: &str = r#"printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"selected":1,"completed":1},"checks":[{"id":"t1","verdict":"pass"}]}' "$commit" "$tree"
"#;

/// The adapter: every check passes with one unit, and `slow` first runs `hold` as the test,
/// outside the sandbox.
fn adapter(hold: &str) -> String {
    [
        HEAD,
        "case \"$req\" in\n*'\"plan\":{\"id\":\"slow\"'*)\n",
        hold,
        ";;\nesac\n",
        TAIL,
    ]
    .concat()
}

/// The records of check `id` about `tip` on the remote.
fn records(test_box: &TestBox, remote: &Path, id: &str, tip: &str) -> Vec<Value> {
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
                record["check"]["id"] == id && record["subject"]["commit"] == tip
            }));
        }
    }
    out
}

/// A child killed when the test ends, so a failed assertion never leaves it running.
struct Reaped(std::process::Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Lets `slow` go when it is dropped, so a failed assertion never leaves it waiting.
struct Release(PathBuf);

impl Drop for Release {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, "");
    }
}

fn eventually(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < within, "never: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_post_check_of_one_environment_records_while_one_of_another_runs() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let started = test_box.path().join("slow-started");
    let go = test_box.path().join("slow-go");
    let hold = test_box.relay(&format!(
        "touch '{}'; until [ -e '{}' ]; do sleep 0.05; done",
        started.display(),
        go.display()
    ));
    let files = vec![
        ("scripts/bay-check".to_owned(), adapter(&hold)),
        (".bay/policy.toml".to_owned(), POLICY.to_owned()),
        (
            ".bay/config.toml".to_owned(),
            "[runner]\npost = true\nbisect_post = false\n".to_owned(),
        ),
    ];
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let one = test_box.bay_ok(&repo, &["start", "one", "--paths", "src"]);
    let view = PathBuf::from(one["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/one.rs"), "x\n").unwrap();
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    let tip = test_box
        .git_bare(&remote, &["rev-parse", "main"])
        .trim()
        .to_owned();

    // One pass of the runner: `slow` starts and waits; `fast` records meanwhile.
    let mut runner = Reaped(
        test_box
            .bay_process(&repo)
            .args(["--json", "runner", "run", "--once"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let release = Release(go);
    eventually(
        "fast records its verdict while slow runs",
        Duration::from_secs(60),
        || !records(&test_box, &remote, "fast", &tip).is_empty(),
    );
    assert!(started.exists(), "slow runs");
    assert_eq!(
        records(&test_box, &remote, "slow", &tip),
        Vec::<Value>::new(),
        "slow still runs"
    );

    // `slow` goes; `--once` waits for it, and its result names both verdicts.
    drop(release);
    eventually("the runner's pass ends", Duration::from_secs(120), || {
        runner.0.try_wait().unwrap().is_some()
    });
    let mut out = String::new();
    runner
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    let result: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        records(&test_box, &remote, "slow", &tip).len(),
        1,
        "{result}"
    );
    assert_eq!(
        records(&test_box, &remote, "fast", &tip).len(),
        1,
        "{result}"
    );
    let rows: Vec<&Value> = result["detail"]["ran"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["post"].as_array())
        .flatten()
        .collect();
    let mut named: Vec<(&str, &str)> = rows
        .iter()
        .map(|row| {
            (
                row["check"].as_str().unwrap_or_default(),
                row["verdict"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    named.sort_unstable();
    assert_eq!(named, [("fast", "pass"), ("slow", "pass")], "{result}");
}
