//! G586 (a fleet finding, 2026-10-30: three check environments were served by the runner of one
//! box, and each stop of that box stopped their checks, with nothing said). A check environment
//! that the runners of only one machine serve is named.
//!
//! With operations queued and live runners on two or more machines, `bay status` warns
//! `single_machine_environment`, naming each environment a check of the policy on main runs in
//! that only one machine's runners serve, that machine, its runners and the checks. A fleet on
//! one machine is never warned, and a warning is never a refusal.
//!
//! Guards to mutate, in `src/presence.rs`: `runner_warnings` without the
//! `single_machine_environments` chain (no warning); `machines.len() < 2` as `< 1` (a
//! one-machine fleet is warned).

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::{TestBox, check_files, passing_result, result_json};

fn single(result: &Value) -> Option<&Value> {
    result["detail"]["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|warning| warning["code"] == "single_machine_environment")
}

/// A runner clone of `remote` named `name` on machine `machine`, declaring `environments`.
fn runner(
    test_box: &TestBox,
    remote: &Path,
    name: &str,
    machine: &str,
    environments: &[&str],
) -> PathBuf {
    let clone = test_box.clone_of(remote, name);
    test_box.git_ok(&clone, &["config", "bay.runnerName", name]);
    test_box.git_ok(
        &clone,
        &["config", "bay.producerName", &format!("{name}-checks")],
    );
    test_box.git_ok(&clone, &["config", "bay.machine", machine]);
    for environment in environments {
        test_box.git_ok(
            &clone,
            &["config", "--add", "bay.checkEnvironment", environment],
        );
    }
    clone
}

fn pass(test_box: &TestBox, clone: &Path) {
    let (code, ran) = result_json(test_box.bay(clone).args(["runner", "run", "--once"]));
    assert_eq!(code, 0, "{ran}");
}

fn queue_and_read(test_box: &TestBox, repo: &Path) -> Value {
    let (_, view) = test_box.start_and_edit(repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let (code, status) = result_json(test_box.bay(&view).arg("status"));
    assert_eq!(code, 0, "{status}");
    assert_eq!(status["decision"], "ok", "{status}");
    status
}

#[test]
fn queued_work_names_the_environment_one_machine_serves() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let one = runner(&test_box, &remote, "one", "box-a", &["local", "elsewhere"]);
    let two = runner(&test_box, &remote, "two", "box-a", &["elsewhere"]);
    pass(&test_box, &one);
    pass(&test_box, &two);

    // One machine: the fleet has no second machine to name.
    let status = queue_and_read(&test_box, &repo);
    assert!(single(&status).is_none(), "{status}");

    // Runner `two` on a second machine that serves no `local`: `local` hangs on box-a.
    test_box.git_ok(&two, &["config", "bay.machine", "box-b"]);
    pass(&test_box, &two);
    let (code, status) = result_json(test_box.bay(&repo).arg("status"));
    assert_eq!(code, 0, "{status}");
    assert_eq!(status["decision"], "ok", "{status}");
    let warning = single(&status).unwrap_or_else(|| panic!("no warning: {status}"));
    assert_eq!(warning["machines"], json!(["box-a", "box-b"]), "{status}");
    let environments = warning["environments"].as_array().unwrap();
    assert_eq!(environments.len(), 1, "{status}");
    assert_eq!(environments[0]["environment"], "local", "{status}");
    assert_eq!(environments[0]["machine"], "box-a", "{status}");
    assert_eq!(environments[0]["runners"], json!(["one"]), "{status}");
    assert!(
        environments[0]["checks"]
            .as_array()
            .is_some_and(|checks| !checks.is_empty()),
        "{status}"
    );

    // Box-b serves `local` too: two machines serve it, nothing to warn of.
    test_box.git_ok(&two, &["config", "--add", "bay.checkEnvironment", "local"]);
    pass(&test_box, &two);
    let status = queue_and_read(&test_box, &repo);
    assert!(single(&status).is_none(), "{status}");
}
