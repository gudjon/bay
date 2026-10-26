//! G250: an adapter proves its contract (`INTERFACES` § 3, § 4, § 7).
//!
//! `bay plugin test <kind> -- <command...>` runs a fixed set of contract cases against a
//! connector, through bay's own invocation code, with bay's environment baseline and time
//! limits, and prints one `bay.result/1` whose `detail.cases` names each case with what was sent,
//! what came back, its result and why. A conforming wake, check and notify adapter pass every
//! case and the command exits 0. A broken one (wrong schema, too slow, nonzero exit, stdout
//! noise, a variable bay clears) fails exactly the matching cases, and the command exits
//! nonzero.

// Each case runs shell adapters, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, write_executable};

const WAKE_ANSWER: &str = r#"{"schema":"bay.wake-result/1","result":"skipped","reason":"SESSION_BLOCKED","at":"2026-10-20T00:00:00Z"}"#;

/// A wake adapter that runs `before` (shell), then prints `answer` and exits `code`.
fn wake_script(before: &str, answer: &str, code: u8) -> String {
    format!("#!/bin/sh\ncat >/dev/null\n{before}\nprintf '%s\\n' '{answer}'\nexit {code}\n")
}

/// A check adapter that runs `before` (shell), then answers the request's candidate with one
/// passing unit under `schema`.
fn check_script(before: &str, schema: &str) -> String {
    format!(
        "#!/bin/sh\nreq=$(cat)\n\
         commit=$(printf '%s' \"$req\" | sed -n 's/.*\"candidate\":{{\"commit\":\"\\([0-9a-f]*\\)\",\"tree\":\"\\([0-9a-f]*\\)\"[,}}].*/\\1/p')\n\
         tree=$(printf '%s' \"$req\" | sed -n 's/.*\"candidate\":{{\"commit\":\"\\([0-9a-f]*\\)\",\"tree\":\"\\([0-9a-f]*\\)\"[,}}].*/\\2/p')\n\
         {before}\n\
         printf '{{\"schema\":\"{schema}\",\"exercised\":{{\"commit\":\"%s\",\"tree\":\"%s\"}},\"plan\":{{\"selected\":1,\"completed\":1}},\"checks\":[{{\"id\":\"t1\",\"verdict\":\"pass\"}}]}}' \"$commit\" \"$tree\"\n"
    )
}

/// A directory holding `scripts/<name>` with `script`, the `-C` directory of the kit.
fn adapter_dir(test_box: &TestBox, name: &str, script: &str) -> PathBuf {
    let dir = test_box.path().join(format!("adapters-{name}"));
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    write_executable(&dir.join("scripts").join(name), script);
    dir
}

/// Runs `bay plugin test <kind> <flags> -- scripts/<name>` from `dir`, with `env` added.
fn kit(
    test_box: &TestBox,
    dir: &Path,
    kind: &str,
    flags: &[&str],
    name: &str,
    env: &[(&str, &str)],
) -> (i32, Value) {
    let mut cmd = test_box.bay(dir);
    cmd.args(["plugin", "test", kind, "--json"])
        .args(flags)
        .arg("--")
        .arg(format!("scripts/{name}"));
    for (key, value) in env {
        cmd.env(key, value);
    }
    let out = cmd.output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let result: Value = serde_json::from_str(&text).unwrap_or_else(|error| {
        panic!(
            "not JSON ({error}): {text}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.code().unwrap_or(-1), result)
}

/// Each case's result by name; every case names what was sent, what came back and why.
fn cases(result: &Value) -> BTreeMap<String, String> {
    assert_eq!(result["schema"], "bay.result/1", "{result}");
    let rows = result["detail"]["cases"]
        .as_array()
        .unwrap_or_else(|| panic!("{result}"));
    let mut by_name = BTreeMap::new();
    for row in rows {
        let name = row["name"].as_str().unwrap_or_default().to_owned();
        assert!(!name.is_empty(), "{row}");
        for field in ["sent", "came_back", "result", "why"] {
            assert!(
                row.get(field).is_some(),
                "case {name} has no {field}: {row}"
            );
        }
        assert!(
            !row["why"].as_str().unwrap_or_default().is_empty(),
            "case {name} says no why: {row}"
        );
        let verdict = row["result"].as_str().unwrap_or_default().to_owned();
        assert!(
            ["pass", "fail", "not_judged"].contains(&verdict.as_str()),
            "{row}"
        );
        assert!(by_name.insert(name, verdict).is_none(), "{result}");
    }
    by_name
}

/// The names of the cases whose result is `word`.
fn named(by_name: &BTreeMap<String, String>, word: &str) -> Vec<String> {
    by_name
        .iter()
        .filter(|(_, verdict)| verdict.as_str() == word)
        .map(|(name, _)| name.clone())
        .collect()
}

const WAKE_CASES: [&str; 9] = [
    "answers",
    "env_baseline",
    "exit_code",
    "one_document",
    "outcome",
    "reason",
    "recorded",
    "schema",
    "within_limit",
];

const CHECK_CASES: [&str; 7] = [
    "answers",
    "env_baseline",
    "exercised",
    "one_document",
    "recorded",
    "schema",
    "within_timeout",
];

/// A wake adapter that breaks one rule fails exactly `failing`; every other case passes.
fn wake_fails_exactly(script: &str, failing: &[&str]) {
    let test_box = TestBox::new();
    let dir = adapter_dir(&test_box, "bay-wake", script);
    let (code, result) = kit(&test_box, &dir, "wake", &[], "bay-wake", &[]);
    assert_ne!(code, 0, "{result}");
    assert_eq!(result["decision"], "failed", "{result}");
    let by_name = cases(&result);
    assert_eq!(named(&by_name, "fail"), failing, "{result}");
    assert_eq!(by_name.len(), WAKE_CASES.len(), "{result}");
    assert!(named(&by_name, "not_judged").is_empty(), "{result}");
    assert_eq!(
        result["detail"]["bay_records"],
        serde_json::json!({"result": "uncertain", "reason": "WAKE_ADAPTER_FAILED"}),
        "{result}"
    );
}

#[test]
fn a_conforming_wake_adapter_passes_every_case() {
    let test_box = TestBox::new();
    let dir = adapter_dir(&test_box, "bay-wake", &wake_script("", WAKE_ANSWER, 0));
    let (code, result) = kit(&test_box, &dir, "wake", &[], "bay-wake", &[]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["decision"], "ok", "{result}");
    let by_name = cases(&result);
    assert_eq!(named(&by_name, "pass"), WAKE_CASES, "{result}");
    assert_eq!(result["detail"]["limit_s"], 5, "{result}");
    // What bay would write on the ack, as G58 records it.
    assert_eq!(
        result["detail"]["bay_records"],
        serde_json::json!({"result": "skipped", "reason": "SESSION_BLOCKED"}),
        "{result}"
    );
    let sent = &result["detail"]["cases"][0]["sent"];
    assert_eq!(sent["schema"], "bay.wake/1", "{result}");
    assert_eq!(sent["op"], "wake", "{result}");
    assert_eq!(sent["cause"], "talk", "{result}");
    // G307: the kit also sends the request an executor sends for an outcome.
    let outcome = result["detail"]["cases"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["name"] == "outcome"))
        .map_or_else(|| panic!("{result}"), |row| row["sent"].clone());
    assert_eq!(outcome["cause"], "outcome", "{result}");
    assert_eq!(outcome["reason"], "CHECK_FAILED", "{result}");
    assert_eq!(outcome["decision"], "blocked", "{result}");
}

#[test]
fn a_wake_adapter_with_the_wrong_schema_fails_the_schema() {
    let answer = WAKE_ANSWER.replace("bay.wake-result/1", "bay.wake-result/2");
    wake_fails_exactly(
        &wake_script("", &answer, 0),
        &["outcome", "recorded", "schema"],
    );
}

#[test]
fn a_wake_adapter_that_exits_nonzero_fails_the_exit_code() {
    wake_fails_exactly(
        &wake_script("", WAKE_ANSWER, 3),
        &["exit_code", "outcome", "recorded"],
    );
}

/// The answer is waited for past bay's 5 s, so the kit judges it, and shows that bay records
/// `uncertain` for it.
#[test]
fn a_wake_adapter_that_answers_after_the_limit_fails_the_limit() {
    wake_fails_exactly(
        &wake_script("sleep 6", WAKE_ANSWER, 0),
        &["outcome", "recorded", "within_limit"],
    );
}

#[test]
fn a_wake_adapter_that_prints_noise_on_stdout_fails_one_document() {
    wake_fails_exactly(
        &wake_script("echo waking the session", WAKE_ANSWER, 0),
        &["one_document", "outcome", "recorded"],
    );
}

/// An adapter that answers only with a variable of its author's shell: bay clears it.
#[test]
fn a_wake_adapter_that_relies_on_a_cleared_variable_fails_the_env_baseline() {
    let test_box = TestBox::new();
    let script = wake_script("[ -n \"$WAKE_TOKEN\" ] || exit 4", WAKE_ANSWER, 0);
    let dir = adapter_dir(&test_box, "bay-wake", &script);
    let (code, result) = kit(
        &test_box,
        &dir,
        "wake",
        &[],
        "bay-wake",
        &[("WAKE_TOKEN", "t0")],
    );
    assert_ne!(code, 0, "{result}");
    let by_name = cases(&result);
    assert_eq!(by_name["env_baseline"], "fail", "{result}");
    assert_eq!(by_name["exit_code"], "fail", "{result}");
    assert_eq!(by_name["within_limit"], "pass", "{result}");
}

#[test]
fn a_conforming_check_adapter_passes_every_case() {
    let test_box = TestBox::new();
    let dir = adapter_dir(
        &test_box,
        "bay-check",
        &check_script("", "bay.check.result/1"),
    );
    let (code, result) = kit(
        &test_box,
        &dir,
        "check",
        &["--timeout-s", "60"],
        "bay-check",
        &[],
    );
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["decision"], "ok", "{result}");
    let by_name = cases(&result);
    assert_eq!(named(&by_name, "pass"), CHECK_CASES, "{result}");
    assert_eq!(
        result["detail"]["bay_records"]["verdict"], "pass",
        "{result}"
    );
    assert_eq!(result["detail"]["limit_s"], 60, "{result}");
    let sent = &result["detail"]["cases"][0]["sent"];
    assert_eq!(sent["schema"], "bay.check.request/1", "{result}");
    assert_eq!(
        sent["changed_paths"],
        serde_json::json!(["src/example.txt"])
    );
    assert_eq!(sent["timeout_s"], 60, "{result}");
}

#[test]
fn a_check_adapter_with_the_wrong_schema_fails_the_schema() {
    let test_box = TestBox::new();
    let dir = adapter_dir(
        &test_box,
        "bay-check",
        &check_script("", "bay.check.result/2"),
    );
    let (code, result) = kit(
        &test_box,
        &dir,
        "check",
        &["--timeout-s", "60"],
        "bay-check",
        &[],
    );
    assert_ne!(code, 0, "{result}");
    let by_name = cases(&result);
    assert_eq!(named(&by_name, "fail"), ["recorded", "schema"], "{result}");
    assert_eq!(by_name.len(), CHECK_CASES.len(), "{result}");
    assert!(named(&by_name, "not_judged").is_empty(), "{result}");
    assert_eq!(
        result["detail"]["bay_records"]["verdict"], "unknown",
        "{result}"
    );
}

/// A check past its `timeout_s` is killed and recorded `unknown`, as bay records it; what it
/// would have said is not judged.
#[test]
fn a_check_adapter_past_its_timeout_fails_the_timeout() {
    let test_box = TestBox::new();
    let dir = adapter_dir(
        &test_box,
        "bay-check",
        &check_script("sleep 4", "bay.check.result/1"),
    );
    let (code, result) = kit(
        &test_box,
        &dir,
        "check",
        &["--timeout-s", "1"],
        "bay-check",
        &[],
    );
    assert_ne!(code, 0, "{result}");
    let by_name = cases(&result);
    assert_eq!(
        named(&by_name, "fail"),
        ["recorded", "within_timeout"],
        "{result}"
    );
    assert!(named(&by_name, "pass").is_empty(), "{result}");
    assert_eq!(
        result["detail"]["bay_records"]["verdict"], "unknown",
        "{result}"
    );
    assert!(
        result["detail"]["bay_records"]["why"]
            .as_str()
            .is_some_and(|why| why.contains("timed out")),
        "{result}"
    );
}

#[test]
fn a_conforming_notify_adapter_passes_every_case() {
    let test_box = TestBox::new();
    let dir = adapter_dir(&test_box, "bay-notify", "#!/bin/sh\ncat >/dev/null\n");
    let (code, result) = kit(&test_box, &dir, "notify", &[], "bay-notify", &[]);
    assert_eq!(code, 0, "{result}");
    let by_name = cases(&result);
    assert_eq!(
        named(&by_name, "pass"),
        [
            "another_kind",
            "env_baseline",
            "exit_code",
            "recorded",
            "repeat_id",
            "within_limit"
        ],
        "{result}"
    );
    assert_eq!(result["detail"]["bay_records"], "delivered", "{result}");
    assert_eq!(
        result["detail"]["cases"][0]["sent"]["schema"], "bay.event/1",
        "{result}"
    );
}

#[test]
fn a_notify_adapter_that_exits_nonzero_is_recorded_lost() {
    let test_box = TestBox::new();
    let dir = adapter_dir(
        &test_box,
        "bay-notify",
        "#!/bin/sh\ncat >/dev/null\nexit 1\n",
    );
    let (code, result) = kit(&test_box, &dir, "notify", &[], "bay-notify", &[]);
    assert_ne!(code, 0, "{result}");
    let by_name = cases(&result);
    assert_eq!(
        named(&by_name, "fail"),
        ["another_kind", "exit_code", "recorded", "repeat_id"],
        "{result}"
    );
    assert_eq!(result["detail"]["bay_records"], "lost", "{result}");
}

/// The kit writes nothing in the directory it runs from: no ref, no config, no file.
#[test]
fn the_kit_changes_nothing_where_it_runs() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    std::fs::create_dir_all(repo.join("scripts")).unwrap();
    write_executable(
        &repo.join("scripts/bay-check"),
        &check_script("", "bay.check.result/1"),
    );
    let refs =
        |repo: &Path| test_box.git_ok(repo, &["for-each-ref", "--format=%(refname) %(objectname)"]);
    let status = |repo: &Path| test_box.git_ok(repo, &["status", "--porcelain", "--ignored"]);
    let (refs_before, status_before) = (refs(&repo), status(&repo));
    let (code, result) = kit(
        &test_box,
        &repo,
        "check",
        &["--timeout-s", "60"],
        "bay-check",
        &[],
    );
    assert_eq!(code, 0, "{result}");
    assert_eq!(refs(&repo), refs_before);
    assert_eq!(status(&repo), status_before);
}
