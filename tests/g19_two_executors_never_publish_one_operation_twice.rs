//! G19. Two executors never publish one operation twice.
//!
//! Two executors receive the same operation concurrently: exactly one publishes; the other
//! records `superseded` or reports the existing publication.

mod support;

use support::{TestBox, check_files, passing_result, result_json};

fn run(test_box: &TestBox, round: usize) {
    let repo = test_box.repo_any(&format!("repo{round}"));
    let remote = test_box.path().join(format!("repo{round}-remote.git"));
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("sleep 1", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0);

    let one = test_box.clone_of(&remote, &format!("executor-a{round}"));
    let two = test_box.clone_of(&remote, &format!("executor-b{round}"));
    for (clone, name) in [(&one, "exec-a"), (&two, "exec-b")] {
        test_box.git_ok(clone, &["config", "bay.executorName", name]);
        test_box.git_ok(
            clone,
            &["config", "bay.producerName", &format!("{name}-checks")],
        );
    }
    let first = test_box
        .bay_process(&one)
        .args(["--json", "executor", "run", "--once"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let second = test_box
        .bay_process(&two)
        .args(["--json", "executor", "run", "--once"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let outputs: Vec<serde_json::Value> = [first, second]
        .into_iter()
        .map(|child| {
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success());
            serde_json::from_slice(&out.stdout).unwrap()
        })
        .collect();

    let results: Vec<&serde_json::Value> = outputs
        .iter()
        .filter_map(|out| out["detail"]["ran"].get(0))
        .collect();
    let published = results
        .iter()
        .filter(|result| result["decision"] == "landed" && result["detail"]["existing"] != true)
        .count();
    assert_eq!(published, 1, "exactly one executor publishes: {outputs:?}");
    for result in &results {
        let landed_existing = result["decision"] == "landed";
        let superseded = result["detail"]["superseded"] == true;
        let log = test_box.git_bare(
            &remote,
            &["log", "--format=%s", &format!("refs/bay/changes/{id}")],
        );
        assert!(
            landed_existing || superseded,
            "{result}\nall: {outputs:?}\nchain:\n{log}"
        );
    }
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}

#[test]
fn two_executors_on_one_operation_publish_it_once() {
    let test_box = TestBox::new();
    for round in 0..3 {
        run(&test_box, round);
    }
}
