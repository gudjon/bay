//! G685 (a fleet finding, 2026-11-09: 556 git processes, 20 GB of memory and 3.4 GB of swap on
//! one box. Its bay commands fetched a 5 GB store every few seconds, and each fetch left a
//! detached `git maintenance run --auto` whose cruft repack outlived the next fetch). No fetch bay
//! runs starts Git's auto maintenance, whatever the store's settings ask; the store is kept by
//! scheduled maintenance instead.
//!
//! Read from Git's own event trace (`GIT_TRACE2_EVENT`), which every Git process bay starts, and
//! every process those start, append to.
//!
//! Guard to mutate: in `src/git.rs` `as_formatted`, drop `--no-auto-maintenance` (a fetch that
//! brings objects starts `git maintenance run --auto`).

use crate::support;

use support::result_json;

use crate::g617_a_listings_sweep_serves_the_reads_in_its_window::{move_trunk, store};

#[test]
fn a_bay_fetch_never_starts_gits_auto_maintenance() {
    let (test_box, _, remote, reader, _) = store();
    // GIVEN a store whose settings ask for auto maintenance after every fetch that brings anything
    test_box.git_ok(&reader, &["config", "maintenance.auto", "true"]);
    test_box.git_ok(&reader, &["config", "gc.auto", "1"]);
    test_box.git_ok(&reader, &["config", "gc.autoDetach", "false"]);
    move_trunk(&test_box, &remote);

    // WHEN bay fetches from the remote (a read sweeps, and settles what moved)
    let events = test_box.path().join("trace2.jsonl");
    let (code, read) = result_json(
        test_box
            .bay(&reader)
            .arg("status")
            .env("BAY_ACTOR", "reader")
            .env("GIT_TRACE2_EVENT", &events),
    );
    assert_eq!(code, 0, "{read}");

    // THEN some Git fetched, and no Git started maintenance
    let text = std::fs::read_to_string(&events).unwrap();
    let starts: Vec<serde_json::Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .filter(|event: &serde_json::Value| event["event"] == "start")
        .collect();
    let argv = |event: &serde_json::Value| -> Vec<String> {
        event["argv"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|word| word.as_str().map(str::to_owned))
            .collect()
    };
    assert!(
        starts
            .iter()
            .any(|event| argv(event).iter().any(|word| word == "fetch")),
        "no fetch ran, so nothing was tested"
    );
    let maintained: Vec<String> = starts
        .iter()
        .map(argv)
        .filter(|words| {
            words
                .iter()
                .any(|word| matches!(word.as_str(), "maintenance" | "gc" | "repack"))
        })
        .map(|words| words.join(" "))
        .collect();
    assert!(maintained.is_empty(), "{}", maintained.join("\n"));
}
