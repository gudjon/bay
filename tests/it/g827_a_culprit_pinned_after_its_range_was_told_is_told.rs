//! G827 (a fleet finding, 2026-11-20: a red was told to the merges on its check's paths as "one
//! of N, not a proven cause" (G634), and a later bisection pinned one of them as its culprit; that
//! change was never told it broke the check). A culprit pinned after its range was told is told
//! so, once.
//!
//! A `red_owner` event's id is a digest of the check, its environment and the change it tells,
//! and a log that holds the id is never written again. A merge of the range and the red's owner
//! shared that id, so the culprit's id was already held when the evidence pinned it.
//!
//! Guard to mutate, in `src/red_owner.rs` `event_id`: the `"owns": "culprit"` field (the
//! culprit is never told).

#![cfg(unix)]

use crate::g244d_a_red_post_check_names_its_culprit_by_bisection::trunk_with;

use serde_json::Value;

/// The wake adapter: appends each request to `$HOME/woken`. bay gives an adapter only PATH and
/// HOME.
const WAKE: &str = "#!/bin/sh\ncat >> \"$HOME/woken\"\necho >> \"$HOME/woken\"\nprintf '{\"schema\":\"bay.wake-result/1\",\"result\":\"woken\",\"reason\":null}'\n";

const CONFIG: &str =
    "[runner]\nbisect_post = true\n\n[plugin.wake]\ncommand = \"scripts/bay-wake\"\n";

#[test]
fn a_culprit_pinned_after_its_range_was_told_is_told_it_broke_the_check() {
    let trunk = trunk_with(CONFIG, &[("scripts/bay-wake", WAKE)]);
    let landings = trunk.red_over_four();
    let red_wakes = || -> Vec<Value> {
        std::fs::read_to_string(trunk.test_box.home.join("woken"))
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|request| request["cause"] == "red")
            .collect()
    };

    // WHEN a pass tells the red, which no landing is pinned on yet, and then bisects it.
    let report = trunk.executor_pass().expect("the executor bisects");
    assert_eq!(report[0]["outcome"], "culprit", "{report}");

    // THEN the four merges on suite's paths since it passed were told, each once, as one of 4.
    let wakes = red_wakes();
    assert_eq!(wakes.len(), 4, "{wakes:?}");
    for (wake, (change, _)) in wakes.iter().zip(&landings[1..]) {
        assert_eq!(wake["change"], change.as_str(), "{wakes:?}");
        let line = wake["line"].as_str().unwrap();
        assert!(
            line.contains(
                "is one of 4 merges on its paths since it last passed, not a proven cause"
            ),
            "{line}"
        );
    }

    // WHEN the next pass reads the red, now pinned on the fourth landing.
    let red = trunk.red();
    assert_eq!(red[0]["culprit"]["change"], landings[3].0.as_str(), "{red}");
    assert_eq!(
        trunk.executor_pass(),
        None,
        "a pinned range is not bisected again"
    );

    // THEN that landing's change is told it broke the check, once, and no one else is told.
    let wakes = red_wakes();
    assert_eq!(wakes.len(), 5, "{wakes:?}");
    assert_eq!(wakes[4]["change"], landings[3].0.as_str(), "{wakes:?}");
    let line = wakes[4]["line"].as_str().unwrap();
    assert!(
        line.contains("broke it: the check passed on the merge before it and failed on it"),
        "{line}"
    );
    trunk.executor_pass();
    assert_eq!(red_wakes().len(), 5, "a culprit is told once");
}
