//! G786 (owner, 2026-11-18: "I just want to know exactly when this will be on origin/dev, and
//! what to do for speeding that up"). A live change's row names its `eta`: the stage it is in
//! (a draft too, counted from when `bay start` opened it), when it is on main as a range
//! (`on_main_at`, low, likely and high), what remains of each stage left, and the landings of the
//! last hours it stands on. Its sentence says it in plain words. Each landing's record keeps when
//! its change was started, so the next draft's estimate has a past to stand on.
//!
//! Guards to mutate: in `src/status.rs` `live_fields`, set `row["eta"]` to null on a full read
//! (no row names when it is on main); in `src/start.rs` `create`, start a change with no
//! `started_at` (a landing's record cannot say how long its draft took, and a draft has no
//! estimate).

#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_change_says_when_it_is_on_main_from_every_stage() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let executor = test_box.clone_of(&remote, "executor");
    // GIVEN three landings, each drafted for longer than a read takes on a loaded box
    let drafts: Vec<(String, std::path::PathBuf)> = ["one", "two", "three"]
        .iter()
        .map(|name| {
            let (id, view) = test_box.start_and_edit(&repo);
            std::fs::write(
                view.join(format!("src/{name}.rs")),
                format!("fn {name}() {{}}\n"),
            )
            .unwrap();
            (id, view)
        })
        .collect();
    std::thread::sleep(std::time::Duration::from_secs(20));
    let mut landed = Vec::new();
    for (id, view) in drafts {
        let (code, submitted) = result_json(
            test_box
                .bay(&view)
                .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
                .arg("merge"),
        );
        assert_eq!(code, 0, "{submitted}");
        let ran = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
        assert_eq!(ran["detail"]["ran"][0]["decision"], "merged", "{ran}");
        landed.push(id);
    }
    let one = test_box.bay_ok(&repo, &["status", &landed[0]]);
    let record = one["detail"]["changes"][0]["terminal"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["outcome"] == "merged" || record["outcome"] == "published")
        .unwrap()
        .clone();
    let (started, sent) = (
        record["started_at"].as_u64(),
        record["submitted_at"].as_u64(),
    );
    assert!(
        started.is_some() && started <= sent,
        "a landing keeps when its change started: {record}"
    );

    // WHEN a new draft is read
    let (draft, view) = test_box.start_and_edit(&repo);
    let read = test_box.bay_ok(&repo, &["status", &draft]);
    let row = &read["detail"]["changes"][0];

    // THEN it says when it was started, and when it is on main, from its draft on
    assert!(row["started_at"].as_u64().is_some(), "{read}");
    let eta = &row["eta"];
    assert_eq!(eta["stage"], "draft", "{read}");
    assert_eq!(eta["main"], "main", "{read}");
    // bay's word for a landing is printed as "merged".
    let basis = &eta["basis"];
    assert_eq!(
        basis.get("landed").or_else(|| basis.get("merged")),
        Some(&serde_json::json!(3)),
        "{read}"
    );
    let on_main = &eta["on_main_at"];
    let (low, likely, high) = (
        on_main["low"].as_str().unwrap_or_default(),
        on_main["likely"].as_str().unwrap_or_default(),
        on_main["high"].as_str().unwrap_or_default(),
    );
    assert!(!low.is_empty() && low <= likely && likely <= high, "{read}");
    let stages: Vec<&str> = eta["stages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|stage| stage["stage"].as_str().unwrap())
        .collect();
    assert_eq!(stages, ["draft", "sent", "in_line", "at_head"], "{read}");
    assert!(eta["speedups"].is_array(), "{read}");
    let said = read["say"].as_str().unwrap();
    assert!(said.contains("On main "), "{said}");

    // AND once sent, it says so, with the stages it has left
    let (code, submitted) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{submitted}");
    let read = test_box.bay_ok(&repo, &["status", &draft]);
    let eta = &read["detail"]["changes"][0]["eta"];
    assert_eq!(eta["stage"], "sent", "{read}");
    assert_eq!(eta["stages"][0]["stage"], "sent", "{read}");
    assert_eq!(eta["stages"][2]["stage"], "at_head", "{read}");
}
