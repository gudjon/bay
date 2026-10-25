//! G382 (a fleet finding, 2026-10-25: two changes submitted at 17:55 and 18:00 waited two hours
//! while changes submitted after them merged, because their ids sorted last). The runner takes
//! waiting changes oldest first.
//!
//! Changes with no queue place are run in the order of their chain's last write, oldest first,
//! never in change-id order, so a change waiting untouched is never passed over by newer ones.
//!
//! Guard to mutate: `src/executor.rs` `pass`: sort by the queue place alone (the changes run in
//! id order).

use crate::support;

use std::path::PathBuf;

use support::{TestBox, result_json, unchecked_files};

#[test]
fn changes_waiting_for_the_runner_run_in_the_order_they_were_submitted() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let mut started = Vec::new();
    for n in 0..3 {
        let result = test_box.bay_ok(
            &repo,
            &["start", &format!("CARD-{n}"), "--paths", &format!("d{n}/")],
        );
        let view = PathBuf::from(result["detail"]["view"].as_str().unwrap());
        std::fs::create_dir_all(view.join(format!("d{n}"))).unwrap();
        std::fs::write(view.join(format!("d{n}/f.txt")), "f\n").unwrap();
        started.push((result["change"].as_str().unwrap().to_owned(), view));
    }
    // Submitted in reverse id order, a second apart: the oldest has the largest id.
    started.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, view) in &started {
        let (code, queued) = result_json(test_box.bay(view).arg("merge"));
        assert_eq!(code, 0, "{queued}");
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }
    let runner = test_box.clone_of(&remote, "runner");
    let (code, ran) = result_json(test_box.bay(&runner).args(["runner", "run", "--once"]));
    assert_eq!(code, 0, "{ran}");
    let order: Vec<String> = ran["detail"]["ran"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|run| run["change"].as_str().map(str::to_owned))
        .collect();
    let submitted: Vec<String> = started.iter().map(|(id, _)| id.clone()).collect();
    assert_eq!(order, submitted, "not oldest first: {ran}");
}
