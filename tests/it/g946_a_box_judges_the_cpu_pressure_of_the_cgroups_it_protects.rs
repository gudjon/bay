//! G946 (a fleet finding, 2026-11-27: a box's two runners waited 189 times in a day for CPU some
//! of 20 %, read at 17.6 % for the box while the job it protects stalled 0.7 %): the box's own
//! checks raised the pressure that held them. A box names the cgroups it protects as Git config
//! `bay.checkStartCpuCgroup` (a `*` in a segment for a unit named anew at each start); its
//! `bay.checkStartCpuSome` is then judged against their CPU pressure, never the box's. Its runner
//! waits while what it protects stalls, and names the cgroup, and claims the change while that
//! runs freely, however busy the box is with its own checks.
//!
//! Guards to mutate, in `src/load.rs`: `cpu_pressure` reading the box's pressure while cgroups
//! are named (the first pass claims; the last waits), and `pressures` reading no cgroup (the
//! first pass claims).

#![cfg(unix)]

use crate::support;

use std::path::Path;

use serde_json::Value;
use support::queued::{Check, Held, Trust, host_under};
use support::{PRODUCER, result_json};

/// Writes `some` as `some avg10` of the `cpu.pressure` of cgroup `path` under `root`.
fn pressure(root: &Path, path: &str, some: &str) {
    let dir = root.join(path);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("cpu.pressure"),
        format!(
            "some avg10={some} avg60=0.00 avg300=0.00 total=1\n\
             full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"
        ),
    )
    .unwrap();
}

/// One runner pass on `clone` with the cgroups under `root` and the box's CPU some `box_some`.
fn pass_at(held: &Held, clone: &Path, root: &Path, box_some: &str) -> Value {
    let (code, result) = result_json(
        held.test_box
            .bay(clone)
            .env("BAY_TEST_CPU_SOME", box_some)
            .env("BAY_TEST_CGROUP_ROOT", root.to_str().unwrap())
            .args(["runner", "run", "--once"]),
    );
    assert_eq!(code, 0, "{result}");
    result
}

/// The reason of the pass's row for change `change`.
fn reason_for(pass: &Value, change: &str) -> Option<String> {
    pass["detail"]["ran"]
        .as_array()?
        .iter()
        .find(|row| row["change"] == change)
        .and_then(|row| row["reason"].as_str().map(str::to_owned))
}

#[test]
fn a_box_judges_the_cpu_pressure_of_the_cgroups_it_protects() {
    let held = host_under(
        &Check {
            config: "",
            policy: None,
            seed: &[("src/a.rs", "fn a() {}\n")],
            hold_first: false,
            fails: "false",
        },
        Trust::Unsigned,
    );
    let root = std::env::temp_dir().join(format!("bay-g946-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    // GIVEN a runner whose box sets CPU some of 20 % for the cgroups plant-*.service
    let runner = held.executor("exec-a", PRODUCER);
    let test_box = &held.test_box;
    test_box.git_ok(&runner, &["config", "bay.checkStartCpuSome", "20"]);
    test_box.git_ok(
        &runner,
        &[
            "config",
            "bay.checkStartCpuCgroup",
            "user.slice/u/plant-*.service",
        ],
    );
    // AND one cgroup it protects stalls 41 %, another 3 %, and its runner's own 90 %
    pressure(&root, "user.slice/u/plant-1.service", "41.00");
    pressure(&root, "user.slice/u/plant-2.service", "3.00");
    pressure(&root, "user.slice/u/bay-r.service", "90.00");
    // AND a queued change
    let one = held.queue_with("one", &["src"], &[("src/b.rs", "fn b() {}\n")]);
    let chain = || {
        test_box.git_bare(
            &held.remote,
            &["log", "--format=%s", &format!("refs/bay/changes/{one}")],
        )
    };
    let claims = || {
        chain()
            .lines()
            .filter(|line| line.starts_with("claim "))
            .count()
    };

    // WHEN it passes while the box reads 5 %
    let pressed = pass_at(&held, &runner, &root, "5");
    // THEN it waits for what it protects, names that cgroup, and writes nothing on the change
    assert_eq!(
        reason_for(&pressed, &one).as_deref(),
        Some("BOX_BUSY"),
        "{pressed:#}"
    );
    assert!(
        pressed.to_string().contains("user.slice/u/plant-1.service"),
        "{pressed:#}"
    );
    assert_eq!(claims(), 0, "a busy box claimed {one}:\n{}", chain());

    // WHEN what it protects runs freely while the box reads 90 % from its own checks
    pressure(&root, "user.slice/u/plant-1.service", "2.00");
    let ran = vec![
        pass_at(&held, &runner, &root, "90"),
        pass_at(&held, &runner, &root, "90"),
    ];
    // THEN it takes the change, and the change merges
    assert!(
        test_box.trunk_change_ids(&held.remote).contains(&one),
        "{one} did not merge:\n{}\n{ran:#?}",
        chain()
    );
    let _ = std::fs::remove_dir_all(&root);
}
