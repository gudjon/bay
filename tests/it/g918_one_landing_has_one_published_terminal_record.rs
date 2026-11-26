//! G918 (a fleet census, 2026-11-26: 510 of 11,186 published operations carried two published
//! terminal records, from two runners or from one runner twice, 0-191 s apart, so a reader that
//! counts records counts those landings twice; one second record said no claim and no attempt, as
//! a chain read before its lander's claim says): one landing has one published terminal record on
//! the remote's logs. A visit that finds a landing on main records it only with the push of its own
//! chain's landed record; when the remote refuses that chain, another process moved it, and the
//! record is that process's to write.
//!
//! The case keeps runner B's copy of the chain from before the landing, and B's reads of the
//! remote's chains and logs failing, while main still reads: B's claim finds the landing on main
//! on the chain it holds, and the remote refuses its chain's push.
//!
//! Guard to mutate, in `src/publish.rs`: `Ctx::push_chain_with_record` answering `Riding::Apart` for a
//! chain the remote refused (the record goes alone, and the landing has two).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::Value;

use support::queued::{Check, Held, OTHER_PRODUCER, Trust, host_under};
use support::{PRODUCER, result_json};

/// A host whose first check waits for the test, so runner A holds the head while B reads it.
fn held() -> Held {
    host_under(
        &Check {
            config: "[runner]\nlease_s = 60\nheartbeat_s = 1\n",
            policy: None,
            seed: &[("src/a.rs", "fn a() {}\n")],
            hold_first: true,
            fails: "false",
        },
        Trust::Unsigned,
    )
}

/// The logs on the remote that hold a published terminal record of `op_id`, one entry a record.
fn published(held: &Held, op_id: &str) -> Vec<String> {
    let git = |args: &[&str]| held.test_box.git_bare(&held.remote, args);
    let mut found = Vec::new();
    for log in git(&["for-each-ref", "--format=%(refname)", "refs/bay/log/"]).lines() {
        for commit in git(&["log", "--format=%H", log]).lines() {
            let record: Value =
                serde_json::from_str(&git(&["show", &format!("{commit}:record.json")])).unwrap();
            if record["op_id"] == op_id && record["outcome"] == "published" {
                found.push(log.to_owned());
            }
        }
    }
    found
}

#[test]
fn a_visit_that_finds_a_landing_on_a_chain_it_could_not_read_again_records_nothing() {
    let rig = held();
    let head = rig.queue("head", "src");
    let op = format!("{head}.1");
    let first = rig.executor("exec-a", PRODUCER);
    let second = rig.executor("exec-b", OTHER_PRODUCER);
    rig.test_box
        .git_ok(&second, &["config", "bay.runnerName", "exec-b"]);

    // GIVEN runner B read the head's chain while runner A held it in its check
    let holder = rig.spawn(&first);
    rig.wait_held();
    let (code, pass) = result_json(rig.test_box.bay(&second).args(["runner", "run", "--once"]));
    assert_eq!(code, 0, "{pass}");
    // AND A landed the head, its terminal record riding with its publishing push
    rig.go();
    let output = holder.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        rig.test_box.trunk_change_ids(&rig.remote).contains(&head),
        "runner A did not land the head"
    );
    assert_eq!(published(&rig, &op).len(), 1, "A's landing");

    // WHEN runner B visits the head while the remote's chains and logs cannot be read
    let dark = rig.test_box.path().join("dark");
    rig.test_box.wrap_git(&format!(
        "if [ -e '{dark}' ]; then\n  case \" $* \" in\n    *\" fetch \"*refs/bay/changes/*|*\" fetch \"*refs/bay/log/*)\n      echo 'fatal: unable to access the remote' >&2\n      exit 128\n      ;;\n  esac\nfi",
        dark = dark.display()
    ));
    std::fs::write(&dark, "").unwrap();
    let (code, visit) = result_json(rig.test_box.bay(&second).args(["runner", "run", "--once"]));
    std::fs::remove_file(&dark).unwrap();
    assert_eq!(code, 0, "{visit}");
    // AND each runner passes once more, so no record it holds stays on its box
    for clone in [&first, &second] {
        let (code, pass) = result_json(rig.test_box.bay(clone).args(["runner", "run", "--once"]));
        assert_eq!(code, 0, "{pass}");
    }

    // THEN the landing has one published terminal record: its lander's, never B's
    let logs = published(&rig, &op);
    assert_eq!(logs.len(), 1, "{logs:?}; B's visit: {visit}");
    assert!(!logs[0].ends_with("exec-b"), "{logs:?}");
}
