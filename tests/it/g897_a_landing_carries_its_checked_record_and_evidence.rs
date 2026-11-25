//! G897 (a fleet measurement, 2026-11-25: a single landing made three pushes of about 5 s each
//! through a forwarding remote, its claim, its persist and its publishing push, and the persist
//! carried only the checked record and its evidence): a single landing's checked record and its
//! evidence ride its publishing push. The host's hook judges trunk on the evidence notes that push
//! carries. A refused landing push leaves the candidate checked on the remote, with its evidence,
//! before anything reads the chain again, and the landing goes on as before.
//!
//! Guards to mutate: in `src/hook.rs` `evidence_on`, read the stored producer refs alone (the
//! landing push is refused, and main moves on a second push); in `src/publish.rs`
//! `publish_checked`, take a refused landing's flush as done (`Ok(true)`: a producer collision
//! then lands trunk with no evidence, G29); in `persist_attempt`, never ride (the evidence goes in
//! a push before the landing push). `refetch` flushes too, a second guard no path reaches today:
//! every path from the persist to the landing push flushes first (`save`, `publish_checked`).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;

use support::queued::{Check, Held, host, ran_for};
use support::{PRODUCER, result_json, write_executable};

const CONFIG: &str = "[runner]\nlease_s = 450\nheartbeat_s = 120\n";

/// The pushes a pass traced in `trace`, each as its arguments joined.
fn pushes(trace: &Path) -> Vec<String> {
    std::fs::read_to_string(trace)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let call: Value = serde_json::from_str(line).unwrap();
            let args: Vec<&str> = call["args"]
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .collect();
            args.contains(&"push").then(|| args.join(" "))
        })
        .collect()
}

/// The places in `pushes` of the pushes that move main.
fn landings(pushes: &[String]) -> Vec<usize> {
    pushes
        .iter()
        .enumerate()
        .filter(|(_, push)| push.contains(":refs/heads/main"))
        .map(|(at, _)| at)
        .collect()
}

/// The evidence notes ref of [`PRODUCER`].
fn notes() -> String {
    format!("refs/notes/bay/evidence/{PRODUCER}")
}

/// Whether the host holds a passing record of [`PRODUCER`] on the tree of main.
fn passes_on_main(held: &Held) -> bool {
    let text = held
        .test_box
        .git_bare(&held.remote, &["notes", "--ref", &notes(), "show", "main^{tree}"]);
    let records: Vec<Value> = serde_json::from_str(&text).unwrap();
    records.iter().any(|record| record["verdict"] == "pass")
}

/// A host with one change in line, and an executor's clone: the host, the clone and the change.
fn one_in_line() -> (Held, PathBuf, String) {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let exec = held.executor("exec", PRODUCER);
    let id = held.queue("one", "src");
    (held, exec, id)
}

#[test]
fn a_landing_pushes_its_checked_record_and_evidence_with_trunk() {
    // GIVEN one change in line
    let (held, exec, id) = one_in_line();

    // WHEN a pass lands it
    let trace = held.test_box.path().join("trace.jsonl");
    let (code, ran) = result_json(
        held.test_box
            .bay(&exec)
            .env("BAY_TEST_GIT_TRACE", &trace)
            .args(["runner", "run", "--once"]),
    );
    assert_eq!(code, 0, "{ran}");
    assert_eq!(ran_for(&ran, &id)["decision"], "merged", "{ran}");

    // THEN one push moves main, carrying the chain and the evidence notes, and no push before it
    // carries the notes
    let pushes = pushes(&trace);
    let landings = landings(&pushes);
    assert_eq!(landings.len(), 1, "{pushes:#?}");
    let landing = landings[0];
    assert!(
        pushes[landing].contains(&format!(":refs/bay/changes/{id}")),
        "{pushes:#?}"
    );
    assert!(pushes[landing].contains(&notes()), "{pushes:#?}");
    assert!(
        pushes[..landing]
            .iter()
            .all(|push| !push.contains("refs/notes/bay/evidence/")),
        "{pushes:#?}"
    );

    // AND the host holds the passing evidence on main's tree
    assert!(passes_on_main(&held), "{ran}");
}

#[test]
fn a_refused_landing_push_leaves_its_candidate_checked_with_its_evidence() {
    // GIVEN one change in line, and a host that refuses the first push that moves main
    let (held, exec, id) = one_in_line();
    let refused = held.test_box.path().join("refused");
    write_executable(
        &held.remote.join("hooks/pre-receive"),
        &format!(
            "#!/bin/sh\nwhile read old new name; do\n  if [ \"$name\" = refs/heads/main ] && [ ! -e '{refused}' ]; then\n    : > '{refused}'\n    echo 'the test refuses this push once' >&2\n    exit 1\n  fi\ndone\nexit 0\n",
            refused = refused.display()
        ),
    );

    // WHEN a pass runs
    let trace = held.test_box.path().join("trace.jsonl");
    let (code, ran) = result_json(
        held.test_box
            .bay(&exec)
            .env("BAY_TEST_GIT_TRACE", &trace)
            .args(["runner", "run", "--once"]),
    );
    assert_eq!(code, 0, "{ran}");
    assert!(refused.exists(), "the host refused nothing: {ran}");

    // THEN the change lands in the pass, on its one check
    assert_eq!(ran_for(&ran, &id)["decision"], "merged", "{ran}");
    let runs = std::fs::read_to_string(&held.runs).unwrap();
    assert_eq!(runs.lines().count(), 1, "{runs}");

    // AND the push after the refused one carries the chain and its evidence, before main moves
    let pushes = pushes(&trace);
    let landings = landings(&pushes);
    assert_eq!(landings.len(), 2, "{pushes:#?}");
    let after = &pushes[landings[0] + 1];
    assert!(
        after.contains(&format!(":refs/bay/changes/{id}")) && after.contains(&notes()),
        "{pushes:#?}"
    );
    assert!(passes_on_main(&held), "{ran}");
}
