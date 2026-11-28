//! G955. A box CPU list keeps each check on those CPUs.
//!
//! A runner that shares its machine with other work names the CPUs its checks may use with the
//! Git config `bay.checkCpus`, a CPU list as `taskset -c` reads it: `git config --global
//! bay.checkCpus 8-11,20-23`. bay starts each check under `taskset -c <list>`, so the check and
//! every process it starts run on those CPUs alone, and a test runner that sizes its threads from
//! the CPUs it may use sizes them to the list. The list is a box fact: it never enters the
//! definition digest, and the record of each check names it in `check_cpus`, next to `sandbox`.
//! Without the setting a check runs on bay's own CPUs, and the record names none. A value that is
//! not a CPU list refuses the check, and its record names the value; so does the setting on a
//! machine that is not Linux, or has no `taskset`. `bay doctor` lists the list in its
//! `check-cpus` row, with `count`, how many of its CPUs bay may run on; with none it is degraded.
//!
//! The guards to mutate: in `src/check.rs`, `run_one` starts the check through `pinned(cmd,
//! place.cpus)` (drop it, and the check runs on every CPU of the test); `evidence` inserts
//! `CHECK_CPUS` (drop it, and the record does not name the list); `cpu_list` refuses an item
//! that is not a number or a range (make it accept every item, and the bad value is not refused
//! by name); `doctor::check_cpus_row` counts the listed CPUs bay may run on.

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json, row};

/// Every evidence record in `repo`'s notes, as one text.
fn records(test_box: &TestBox, repo: &Path) -> String {
    let notes = test_box.git_ok(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/notes/bay/evidence/",
        ],
    );
    notes
        .lines()
        .flat_map(|notes_ref| {
            test_box
                .git_ok(
                    repo,
                    &["ls-tree", "-r", "--format=%(objectname)", notes_ref],
                )
                .lines()
                .map(|blob| test_box.git_ok(repo, &["cat-file", "blob", blob]))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The CPUs this test may run on, as `/proc/self/status` lists them (`0-11`): bay, which this
/// test starts, may run on the same ones.
#[cfg(target_os = "linux")]
fn own_cpus() -> String {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
        .unwrap()
        .trim()
        .to_owned()
}

/// The first CPU of `own_cpus`.
#[cfg(target_os = "linux")]
fn first_cpu() -> String {
    own_cpus().split([',', '-']).next().unwrap().to_owned()
}

/// A trunk whose one check passes only when the CPUs it may run on are exactly `cpus`, as its
/// own `/proc/self/status` lists them; otherwise it reports a product failure. Returns (repo,
/// view).
fn submitted(test_box: &TestBox, cpus: &str) -> (PathBuf, PathBuf) {
    let before = "cpus=$(sed -n 's/^Cpus_allowed_list:[[:space:]]*//p' /proc/self/status)";
    let pass = passing_result(1);
    let fail = pass.replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let result = format!("if [ \"$cpus\" = '{cpus}' ]; then {pass}; else {fail}; fi");
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files(before, &result, 1, 60),
    );
    let (_, view) = test_box.start_and_edit(&repo);
    (repo, view)
}

fn set_cpus(test_box: &TestBox, value: &str) {
    test_box.git_ok(
        test_box.path(),
        &["config", "--global", "bay.checkCpus", value],
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_box_cpu_list_keeps_the_check_on_those_cpus_and_its_record_names_it() {
    let test_box = TestBox::new();
    // With one CPU to run on, a check on every CPU of the test is on that one too: the case
    // cannot tell the two apart there.
    let cpu = first_cpu();
    if own_cpus() == cpu {
        eprintln!("this test may run on one CPU only; G955 cannot tell a pinned check here");
    }
    set_cpus(&test_box, &cpu);
    let (repo, view) = submitted(&test_box, &cpu);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(
        code, 0,
        "the check did not run on the box's CPU {cpu} alone: {landed}"
    );
    assert_eq!(landed["decision"], "merged", "{landed}");
    let records = records(&test_box, &repo);
    assert!(
        records.contains(&format!("\"check_cpus\": \"{cpu}\"")),
        "the record does not name the CPU list {cpu}: {records}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn without_the_setting_the_check_runs_on_bays_cpus_and_the_record_names_none() {
    let test_box = TestBox::new();
    let (repo, view) = submitted(&test_box, &own_cpus());
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "the check did not run on bay's own CPUs: {landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    let records = records(&test_box, &repo);
    assert!(!records.contains("\"check_cpus\""), "{records}");
}

#[test]
fn a_value_that_is_not_a_cpu_list_refuses_the_check_by_name() {
    let test_box = TestBox::new();
    set_cpus(&test_box, "0-x");
    let (_repo, view) = submitted(&test_box, "0");
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(
        code, 10,
        "a bad CPU list did not refuse the check: {refused}"
    );
    assert_eq!(refused["reason"], "CHECK_UNKNOWN", "{refused}");
    assert_eq!(
        refused["detail"]["checks"][0]["verdict"], "infra_failure",
        "{refused}"
    );
    let text = refused.to_string();
    assert!(
        text.contains("bay.checkCpus")
            && text.contains("0-x")
            && text.contains("is not a CPU list"),
        "the refusal does not name the bad CPU list: {refused}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn doctor_lists_the_cpu_list_and_counts_the_cpus_bay_may_run_on() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    // No setting: no row.
    let (_, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert!(
        result["detail"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["name"] != "check-cpus"),
        "{result}"
    );
    let cpu = first_cpu();
    set_cpus(&test_box, &format!("{cpu},4095"));
    let (_, result) = result_json(test_box.bay(&repo).arg("doctor"));
    let cpus = row(&result, "check-cpus");
    assert_eq!(cpus["state"], "green", "{result}");
    assert_eq!(cpus["cpus"], format!("{cpu},4095"), "{result}");
    // CPU 4095 is not one this test may run on, so the list counts one.
    assert_eq!(cpus["count"], 1, "{result}");
    set_cpus(&test_box, "4095");
    let (_, result) = result_json(test_box.bay(&repo).arg("doctor"));
    let cpus = row(&result, "check-cpus");
    assert_eq!(cpus["state"], "degraded", "{result}");
    assert_eq!(cpus["count"], 0, "{result}");
    set_cpus(&test_box, "0-x");
    let (_, result) = result_json(test_box.bay(&repo).arg("doctor"));
    let cpus = row(&result, "check-cpus");
    assert_eq!(cpus["state"], "degraded", "{result}");
    assert!(
        cpus["note"].as_str().unwrap().contains("is not a CPU list"),
        "{result}"
    );
}
