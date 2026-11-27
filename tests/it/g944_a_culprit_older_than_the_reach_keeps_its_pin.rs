//! G944 (BACKLOG 358, a load measurement, 2026-11-27: batches landed on a red main, and once the
//! reach no longer held the pass below the culprit a bisect had named, the red lost its culprit,
//! its owner went to the newest failure, and later bisects said the red was the check's own). A
//! failure at the oldest landing read, when main goes on below it, says only that the cause is
//! older: the walk reads on past the reach until a pass, so the culprit keeps its pin. The next
//! judgement reads its line as deep at once, with the landings since, so the pin stays when the
//! oldest landing read has no answer. A landing on the check's paths runs it before its merge
//! while main is red (G677), so it answers; the landings off its paths here do not.
//!
//! Guards to mutate, in `src/red.rs`: `Answers::further` reading nothing (the red is the check's
//! own at the reach's end); `judge_from` carrying no depth (`carried` 0: the next judgement
//! loses the pin).

#![cfg(unix)]

use crate::g244d_a_red_post_check_names_its_culprit_by_bisection::trunk_with;

use serde_json::{Value, json};

/// Window and reach of six main commits, so the reach ends above the pass below the culprit
/// after five more landings.
const CONFIG: &str = "[runner]\nbisect_post = true\n\n[red]\nwindow = 6\nreach = 6\n";

fn mark((change, commit): &(String, String)) -> Value {
    json!({"merge": commit, "change": change})
}

#[test]
fn a_culprit_older_than_the_reach_keeps_its_pin_as_main_grows() {
    // GIVEN a red whose bisect named the fourth landing, the pass right below it
    let trunk = trunk_with(CONFIG, &[]);
    let mut landings = trunk.red_over_four();
    let report = trunk.executor_pass().expect("the executor bisects");
    assert_eq!(report[0]["outcome"], "culprit", "{report}");
    let culprit = mark(&landings[3]);
    assert_eq!(report[0]["culprit"], culprit, "{report}");

    // WHEN five more landings come, the middle three off the check's paths, the newest seen
    // failing, so the reach ends at the failure above the culprit
    let path = |n: usize| match n {
        6..=8 => format!("docs/l{n}.md"),
        _ => format!("src/l{n}.rs"),
    };
    landings.extend((5..10).map(|n| trunk.land(&path(n))));
    trunk.suite(&landings[9].1, "product_failure");

    // THEN the walk reads on past the reach, and the culprit keeps its pin and owns the red
    let red = trunk.red();
    assert_eq!(red.as_array().map(Vec::len), Some(1), "{red}");
    let suspect = &red[0]["suspect"];
    assert_eq!(red[0]["culprit"]["merge"], culprit["merge"], "{red}");
    assert_eq!(
        red[0]["change"], culprit["change"],
        "the culprit owns it: {red}"
    );
    assert_eq!(suspect["last_passed"], mark(&landings[2]), "{red}");
    assert_eq!(suspect["base_failed"], Value::Null, "{red}");
    assert_eq!(suspect["searched"], Value::Null, "{red}");

    // WHEN two more landings come off the check's paths, so the oldest landing the reach reads
    // has no answer
    landings.extend((10..12).map(|n| trunk.land(&format!("docs/l{n}.md"))));

    // THEN the judgement reads as deep as the last one did, and the pin stays
    let red = trunk.red();
    assert_eq!(red[0]["culprit"]["merge"], culprit["merge"], "{red}");
    assert_eq!(red[0]["change"], culprit["change"], "{red}");
    assert_eq!(
        red[0]["suspect"]["last_passed"],
        mark(&landings[2]),
        "{red}"
    );
}
