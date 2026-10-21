//! G257. A footprint check is carried onto a candidate with the same read set (NEW-10,
//! `crate::throughput`).
//!
//! With `[executor] footprint_lanes = true` and exact-inputs reuse on a bay host, two changes
//! wait in the publication queue: the head changes `ui/`, the second `api/`, and each path has
//! its own check with `when_paths`. While executor A checks the head, executor B pre-checks the
//! second on the current tip, since its footprint is disjoint from the head's. When the second
//! becomes head, its candidate's tree is another one (it holds the head's landing), but the
//! `api` check declares `footprint = true` and its read set is the same, byte for byte, in the
//! new tree and its base: A's producer carries the record onto the new tree, naming the record
//! it carries, and the second lands with no new run, after the head (queue order). Without the
//! opt-in, or when the head's landing changed the check's read set, the second is checked again.
//!
//! The guards to mutate: the `footprint_lanes` branch of `finished_checks` in `src/publish.rs`
//! (never carry), `throughput::same_stamp` (always true, or with neither tree digest compared:
//! the read-set case then lands on a record about other bytes; for one change, either digest
//! implies the other), and `throughput::disjoint` (always false: no pre-check runs).

// Each case runs shell scripts (the check adapter, the relay), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::Value;

use support::PRODUCER;
use support::queued::{Check, Held, OTHER_PRODUCER, finished, host, ran_for};

const CONFIG: &str = "[policy]\nevidence_reuse = \"exact-inputs\"\n\n[executor]\nlease_s = 30\nheartbeat_s = 1\nfootprint_lanes = true\n";

/// The policy: check `ui` on `ui/`, and check `api` on `api/` with `api_extra` lines.
fn policy(api_extra: &str) -> String {
    let check = |id: &str, extra: &str| {
        format!(
            "\n[[check]]\nid = \"{id}\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\nwhen_paths = [\"{id}/\"]\n{extra}"
        )
    };
    format!(
        "revision = 1\n{}{}",
        check("ui", "footprint = true\n"),
        check("api", api_extra)
    )
}

/// The head (`ui/one.rs`, and `lib/one.rs` when `head_touches_lib`) and the second
/// (`api/two.rs`) wait in the queue; A checks the head, which holds, while B pre-checks the
/// second on the tip. Returns the host, the two ids and A's pass.
fn while_the_head_is_checked(
    api_extra: &str,
    head_touches_lib: bool,
) -> (Held, [String; 2], Value) {
    let policy = policy(api_extra);
    let held = host(&Check {
        config: CONFIG,
        policy: Some(&policy),
        seed: &[
            ("ui/a.rs", "fn a() {}\n"),
            ("api/a.rs", "fn a() {}\n"),
            ("lib/a.rs", "fn a() {}\n"),
        ],
        hold_first: true,
        fails: "false",
    });
    let mut files = vec![("ui/one.rs", "fn one() {}\n")];
    if head_touches_lib {
        files.push(("lib/one.rs", "fn one() {}\n"));
    }
    let one = held.queue_with("one", &["ui", "lib"], &files);
    let two = held.queue("two", "api");
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    let child = held.spawn(&a);
    held.wait_held();
    let ahead = held.pass(&b);
    let requests = held.requests();
    assert_eq!(
        requests.len(),
        2,
        "the head's run and one pre-check: {ahead}"
    );
    let precheck = &ran_for(&ahead, &two)["detail"]["precheck"];
    assert_eq!(precheck["on"], "tip", "{ahead}");
    assert_eq!(
        precheck["tree"], requests[1]["candidate"]["tree"],
        "{ahead}"
    );
    held.go();
    let outcome = finished(child);
    for id in [&one, &two] {
        assert_eq!(ran_for(&outcome, id)["decision"], "landed", "{outcome}");
    }
    assert_eq!(
        held.test_box.trunk_change_ids(&held.remote),
        vec![two.clone(), one.clone()],
        "the queue's order"
    );
    (held, [one, two], outcome)
}

#[test]
fn a_record_whose_read_set_is_unchanged_is_carried_and_the_entry_lands_with_no_new_run() {
    let (held, _, _) = while_the_head_is_checked("footprint = true\n", false);
    let requests = held.requests();
    assert_eq!(requests.len(), 2, "carried, not rechecked: {requests:?}");
    let landed = held.tree("main");
    assert_ne!(
        requests[1]["candidate"]["tree"].as_str().unwrap(),
        landed,
        "the pre-check was about another tree"
    );
    // Evidence names exactly what it covers: a record about the landed tree, carried from the
    // pre-check's record, under the same stamp.
    let records = held.records(PRODUCER, &landed);
    let carried = records
        .iter()
        .find(|record| record["check"]["id"] == "api")
        .unwrap_or_else(|| panic!("{records:?}"));
    assert_eq!(carried["subject"]["tree"], landed.as_str(), "{carried}");
    assert_eq!(carried["verdict"], "pass", "{carried}");
    assert_eq!(
        carried["footprint"]["from"]["tree"], requests[1]["candidate"]["tree"],
        "{carried}"
    );
    assert_eq!(carried["footprint"]["paths"][0], "api/", "{carried}");
}

#[test]
fn a_check_without_the_footprint_opt_in_is_checked_again() {
    let (held, _, _) = while_the_head_is_checked("", false);
    assert_eq!(held.requests().len(), 3, "rechecked: {:?}", held.requests());
}

/// The read set is `paths` when the check has them: `api` sees `lib/` too, so a head that
/// changes `lib/` (without waking `api`) changes what the second's check read, and its record
/// is not carried.
#[test]
fn a_record_whose_read_set_changed_is_not_carried() {
    let (held, _, _) =
        while_the_head_is_checked("footprint = true\npaths = [\"api/\", \"lib/\"]\n", true);
    assert_eq!(held.requests().len(), 3, "rechecked: {:?}", held.requests());
}
