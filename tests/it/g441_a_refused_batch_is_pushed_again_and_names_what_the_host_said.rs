//! G441. A batch the host refused on an unmoved main is pushed again as built, and its record
//! says what the host said.
//!
//! 2026-10-28 14:03-14:12Z: the host refused one batch's push twice while main stood still.
//! Each refusal gave the batch back, so it was built and checked again (4.5 min each), and the
//! third push of the same refs landed. The record kept only the first 160 characters of the
//! push's command line, which names thirty refs and more, so the host's reason was lost; and
//! the yield said "main moved" although main had not moved. Now a refused batch push is pushed
//! again as built while main stands still (BACKLOG 83), its record names the host's own lines
//! (BACKLOG 81), and its yield names the refusal (BACKLOG 82).
//!
//! The guards to mutate: the re-push loop in `land_batch` (`src/publish.rs`; the batch is
//! given back and nothing lands in the pass), `host_said` in the batch refusal record (the
//! record holds the command line, not GH013) and the yield's `cause` (it says "main moved").

// Each case runs shell scripts (the check adapter, a host hook), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::PRODUCER;
use support::queued::{Check, Held, host, ran_for};
use support::write_executable;

/// A bay host that batches at most eight, and whose check passes every candidate.
fn batching() -> Held {
    host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\nbatch = { min = 2, max = 8 }\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "[ -e src/bad.rs ]",
    })
}

/// Makes the host refuse the next `times` pushes that move main, as GitHub's GH013 did; returns
/// the file that counts the refusals.
fn refuse_main(held: &Held, times: u32) -> PathBuf {
    let count = held.test_box.path().join("refused");
    write_executable(
        &held.remote.join("hooks/pre-receive"),
        &format!(
            "#!/bin/sh\nn=$(cat '{count}' 2>/dev/null || echo 0)\nwhile read old new name; do\n  if [ \"$name\" = refs/heads/main ] && [ \"$n\" -lt {times} ]; then\n    echo $((n + 1)) > '{count}'\n    echo 'error: GH013: Repository rule violations found for refs/heads/main.' >&2\n    exit 1\n  fi\ndone\nexit 0\n",
            count = count.display()
        ),
    );
    count
}

/// How many pushes of main the host refused.
fn refused(count: &Path) -> u32 {
    std::fs::read_to_string(count).map_or(0, |text| text.trim().parse().unwrap_or(0))
}

/// The subjects of change `id`'s chain on the host, newest first.
fn chain(held: &Held, id: &str) -> Vec<String> {
    held.test_box
        .git_bare(
            &held.remote,
            &["log", "--format=%s", &format!("refs/bay/changes/{id}")],
        )
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_batch_refused_once_on_an_unmoved_main_lands_in_the_same_pass() {
    let held = batching();
    let ids = [held.queue("one", "src"), held.queue("two", "src")];
    let count = refuse_main(&held, 1);
    let executor = held.executor("exec", PRODUCER);
    let pass = held.pass(&executor);
    assert_eq!(refused(&count), 1, "the host never refused: {pass}");
    for id in &ids {
        assert_eq!(
            ran_for(&pass, id)["decision"],
            "merged",
            "the refused batch was not pushed again: {pass}"
        );
    }
    assert_eq!(held.requests().len(), 1, "the batch was checked again");
}

#[test]
fn a_batch_refused_past_its_re_pushes_names_the_host_and_lands_on_its_next_turn() {
    let held = batching();
    let ids = [held.queue("one", "src"), held.queue("two", "src")];
    let count = refuse_main(&held, 3);
    let executor = held.executor("exec", PRODUCER);
    let first = held.pass(&executor);
    assert_eq!(refused(&count), 3, "{first}");
    let records = chain(&held, &ids[0]);
    assert!(
        records.iter().any(|record| {
            record.contains("the host refused the batch's push") && record.contains("GH013")
        }),
        "the record lost what the host said: {records:?}"
    );
    assert!(
        records
            .iter()
            .any(|record| record.contains("yielded") && record.contains("the host refused")),
        "{records:?}"
    );
    assert!(
        !records.iter().any(|record| record.contains("main moved")),
        "main never moved: {records:?}"
    );
    // The head yields with its place kept and lands on its next turn, in this pass or the next.
    let _ = held.pass(&executor);
    let landed = held.test_box.trunk_change_ids(&held.remote);
    for id in &ids {
        assert!(landed.contains(id), "{id} did not land: {landed:?}");
    }
}
