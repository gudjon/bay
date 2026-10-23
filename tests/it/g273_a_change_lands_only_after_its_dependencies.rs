//! G273 (N-16): a change lands only after its dependencies.
//!
//! `bay start <intent> --paths ... --after <change>` (repeatable) records, in the change's
//! record, the changes it depends on. The change may be checked and queued at once, but the
//! executor publishes it only once every named change has landed on trunk. Until then `bay
//! land` answers `queued` with `DEPENDENCY_PENDING` at once, and `bay merge --wait` the same,
//! naming each pending dependency and its state; trunk does not move and the operation holds no
//! queue place. Once the dependency lands, the dependent lands after it, and its trunk commit
//! carries `Bay-After: <dependency>`. A dependency that ends without landing refuses the
//! dependent `DEPENDENCY_ABANDONED`, naming it and how to drop it. `bay start` refuses an
//! `--after` that is not a change id, names no change the remote holds, names the change
//! itself, or closes a cycle.
//!
//! The guards to mutate, each of which turns a case red:
//!
//! - `execute_op` in `src/publish.rs`, the call `dependency_gate(ctx, op_id, &revision,
//!   &trunk)?`: drop it, and the first case's dependent lands before its dependency;
//! - `dependency_waiting` in `src/publish.rs`, `if pending.is_empty() { return Ok(None); }`:
//!   make it `if true`, and the first case's dependent lands before its dependency;
//! - `dependency_waiting`, `if let Some(first) = ended.first()`: skip it, and the second case is
//!   answered `DEPENDENCY_PENDING`;
//! - `crate::after::ended_unlanded` in `src/after.rs`: answer `false`, and the second case is
//!   answered `DEPENDENCY_PENDING`;
//! - `submit_land` in `src/publish.rs`, the call `dependency_unrun(&ctx, &op_id, &trunk)?`: drop
//!   it, and the first case's plain `bay merge` answers with no reason;
//! - `crate::after::trailers`: return an empty string, and the first case's `Bay-After:` is
//!   missing;
//! - `named_or_refused` in `src/start.rs`, `if !is_change_id(dependency)`: drop it, and the
//!   third case's malformed id is refused for the wrong reason;
//! - `after_itself` in `src/start.rs`, `if request.after.iter().any(|dependency| dependency ==
//!   change)`: drop it, and the third case's change is recorded as landing after itself;
//! - `after_itself`, `if record.after.iter().any(|named| named == change)`: drop it, and the
//!   third case's cycle is recorded.

// Each case runs shell scripts (the check adapter), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::{TestBox, TestKeys, check_files, passing_result, result_json};

/// A bay-hosted remote (its hook judges every push) with an enrolled key registry.
fn hosted() -> (TestBox, PathBuf, PathBuf, TestKeys) {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    (test_box, host, repo, keys)
}

fn start(test_box: &TestBox, repo: &Path, args: &[&str]) -> (i32, Value) {
    result_json(test_box.bay(repo).arg("start").args(args))
}

/// Starts a change on `intent` landing after each of `after`, and writes `file` in its view.
/// Returns (id, view).
fn start_writing(
    test_box: &TestBox,
    repo: &Path,
    intent: &str,
    file: &str,
    after: &[&str],
) -> (String, PathBuf) {
    let mut args = vec![intent, "--paths", "src/"];
    for dependency in after {
        args.extend(["--after", *dependency]);
    }
    let (code, started) = start(test_box, repo, &args);
    assert_eq!(code, 0, "{started}");
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(file), format!("// {file}\n")).unwrap();
    (started["change"].as_str().unwrap().to_owned(), view)
}

/// `bay merge --wait` in `view`.
fn land_wait(test_box: &TestBox, view: &Path) -> (i32, Value) {
    result_json(test_box.bay(view).args(["merge", "--wait"]))
}

/// The change record at the host's head of change `id`'s chain.
fn host_record(test_box: &TestBox, host: &Path, id: &str) -> Value {
    let text = test_box.git_bare(
        host,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    serde_json::from_str(&text).unwrap()
}

/// Asserts `result` is the change waiting on `dependency`, which has not landed.
fn pending(result: &Value, dependency: &str) {
    assert_eq!(result["decision"], "queued", "{result}");
    assert_eq!(result["reason"], "DEPENDENCY_PENDING", "{result}");
    assert_eq!(result["allowed"], json!(["wait"]), "{result}");
    assert_eq!(result["retry_when"], "after_wait", "{result}");
    assert_ne!(result["decision"], "merged", "{result}");
    let named = &result["detail"]["pending"][0];
    assert_eq!(named["change"], dependency, "{result}");
    let state = named["state"].as_str().unwrap_or_default();
    assert!(!state.is_empty() && state != "landed", "{result}");
}

#[test]
fn a_dependent_waits_queued_until_its_dependency_lands_then_lands_after_it() {
    let (test_box, host, repo, _) = hosted();
    let (first, first_view) = start_writing(&test_box, &repo, "CARD-2731", "src/first.rs", &[]);
    let (second, second_view) = start_writing(
        &test_box,
        &repo,
        "CARD-2732",
        "src/second.rs",
        &[first.as_str()],
    );
    let record = host_record(&test_box, &host, &second);
    assert_eq!(record["after"], json!([first]), "{record}");
    let before = test_box.trunk_change_ids(&host);

    // A plain land answers at once: queued, waiting on the dependency.
    let (code, submitted) = result_json(test_box.bay(&second_view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");
    pending(&submitted, &first);

    // So does land --wait, and again: the wait counts toward no limit, and trunk stays.
    for _ in 0..3 {
        let (code, waiting) = land_wait(&test_box, &second_view);
        assert_eq!(code, 0, "{waiting}");
        pending(&waiting, &first);
        assert_eq!(test_box.trunk_change_ids(&host), before);
    }

    // The dependency lands; then the dependent lands after it.
    let (code, landed) = land_wait(&test_box, &first_view);
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    let (code, landed) = land_wait(&test_box, &second_view);
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");

    let ids = test_box.trunk_change_ids(&host);
    assert_eq!(ids[..2], [second.as_str(), first.as_str()], "trunk order");
    let trailer = test_box.git_bare(
        &host,
        &[
            "log",
            "-1",
            "--format=%(trailers:key=Bay-After,valueonly)",
            "main",
        ],
    );
    assert_eq!(trailer.trim(), first);
}

#[test]
fn a_dependency_that_ends_without_landing_refuses_its_dependent() {
    let (test_box, host, repo, _) = hosted();
    let (first, first_view) = start_writing(&test_box, &repo, "CARD-2733", "src/first.rs", &[]);
    let (_, second_view) = start_writing(
        &test_box,
        &repo,
        "CARD-2734",
        "src/second.rs",
        &[first.as_str()],
    );
    let before = test_box.trunk_change_ids(&host);

    let (code, abandoned) = result_json(test_box.bay(&first_view).args(["abandon"]));
    assert_eq!(code, 0, "{abandoned}");
    assert_eq!(abandoned["detail"]["state"], "abandoned", "{abandoned}");

    let (code, refused) = land_wait(&test_box, &second_view);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["decision"], "blocked", "{refused}");
    assert_eq!(refused["reason"], "DEPENDENCY_ABANDONED", "{refused}");
    assert_eq!(refused["allowed"], json!(["read", "repair"]), "{refused}");
    assert_eq!(refused["retry_when"], "after_fix", "{refused}");
    let ended = &refused["detail"]["ended"][0];
    assert_eq!(ended["change"], first.as_str(), "{refused}");
    assert_eq!(ended["state"], "abandoned", "{refused}");
    let recovery = refused["detail"]["recovery"].as_str().unwrap_or_default();
    assert!(recovery.contains(&format!("--after {first}")), "{refused}");
    assert_eq!(test_box.trunk_change_ids(&host), before);
}

#[test]
fn start_refuses_an_after_that_is_no_other_change() {
    let (test_box, host, repo, _) = hosted();
    let failed_with = |args: &[&str], why: &str| {
        let (code, refused) = start(&test_box, &repo, args);
        assert_eq!(code, 20, "{refused}");
        assert_eq!(refused["decision"], "failed", "{refused}");
        let detail = refused["detail"].as_str().unwrap_or_default();
        assert!(detail.contains(why), "{refused}");
    };

    // Not a change id, and an id the remote holds no change of: nothing is created.
    failed_with(
        &["CARD-2735", "--paths", "src/", "--after", "not-a-change"],
        "is not a change id",
    );
    let unknown = bay::ids::new_change_id().unwrap();
    failed_with(
        &["CARD-2735", "--paths", "src/", "--after", &unknown],
        "holds no such change",
    );
    let changes = test_box.git_bare(&host, &["for-each-ref", "refs/bay/changes/"]);
    assert!(changes.trim().is_empty(), "{changes}");

    // The change itself.
    let (own, _) = start_writing(&test_box, &repo, "CARD-2736", "src/own.rs", &[]);
    failed_with(
        &["CARD-2736", "--paths", "src/", "--after", &own],
        "names the change itself",
    );
    assert_eq!(host_record(&test_box, &host, &own)["after"], Value::Null);

    // A cycle: a change named as a dependency cannot then land after its dependent.
    let (dependent, _) =
        start_writing(&test_box, &repo, "CARD-2737", "src/dep.rs", &[own.as_str()]);
    failed_with(
        &["CARD-2736", "--paths", "src/", "--after", &dependent],
        "neither could merge",
    );
    assert_eq!(host_record(&test_box, &host, &own)["after"], Value::Null);
}

/// A dependency that ends `no_effect` (its work was already on trunk) is met, not abandoned: its
/// dependent lands.
#[test]
fn a_dependency_with_no_effect_is_met_and_its_dependent_lands() {
    let (test_box, host, repo, _) = hosted();
    let (first, first_view) = start_writing(&test_box, &repo, "CARD-2736", "src/first.rs", &[]);
    // Its only edit is undone: the view holds trunk's tree again, so landing it has no effect.
    std::fs::remove_file(first_view.join("src/first.rs")).unwrap();
    let (code, nothing) = land_wait(&test_box, &first_view);
    assert_eq!(code, 0, "{nothing}");
    let (_, second_view) = start_writing(
        &test_box,
        &repo,
        "CARD-2737",
        "src/second.rs",
        &[first.as_str()],
    );
    let (code, landed) = land_wait(&test_box, &second_view);
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert!(
        test_box
            .trunk_change_ids(&host)
            .contains(&landed["change"].as_str().unwrap().to_owned()),
        "{landed}"
    );
}
