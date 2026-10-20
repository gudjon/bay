//! G26. A candidate cannot weaken its own criteria.
//!
//! A candidate that edits the check policy is judged by the base's policy; the manifest digest
//! equals the one computed from the policy at the base (and the RFC 8785 test vector is a unit
//! test of `policy.rs`); a policy edit that keeps the old revision is refused; two entries for
//! one (id, environment) are refused; a candidate that edits its adapter is judged by the
//! base's adapter, and without raising revision is refused; an adapter edit that fails under
//! its own new adapter is refused; after a broken adapter lands, a registry-signed repair
//! restores a passing trunk and an agent-signed repair is refused; an adapter that reads an
//! unlisted file fails with `infra_failure`; a policy that lands while a candidate is checked
//! makes it `POLICY_CHANGED` and rechecked, and releases a queue head without counting toward any
//! limit. Fewer tests than `expected_minimum` is G3.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

const PASS_SCRIPT_MARK: &str = "# adapter v1";

fn policy(revision: u32, expected_minimum: u32) -> String {
    format!(
        "revision = {revision}\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = {expected_minimum}\ntimeout_s = 60\n"
    )
}

/// The adapter script: passes, or reports a product failure.
fn script(pass: bool, mark: &str) -> String {
    let result = if pass {
        passing_result(1)
    } else {
        passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"")
    };
    let files = check_files(mark, &result, 1, 60);
    files
        .into_iter()
        .find(|(path, _)| path == "scripts/bay-check")
        .unwrap()
        .1
}

/// A trunk with a policy and an adapter, and a change whose view covers both.
fn change(base_passes: bool) -> (TestBox, PathBuf, PathBuf, String, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let files = vec![
        (
            "scripts/bay-check".to_owned(),
            script(base_passes, PASS_SCRIPT_MARK),
        ),
        (".bay/policy.toml".to_owned(), policy(1, 1)),
    ];
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let started = test_box.bay_ok(
        &repo,
        &["start", "CARD-1", "--paths", "src/", ".bay/", "scripts/"],
    );
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    (test_box, repo, remote, id, view)
}

fn write(view: &Path, path: &str, text: &str) {
    std::fs::write(view.join(path), text).unwrap();
}

#[test]
fn a_candidate_that_edits_the_policy_is_judged_by_the_bases_policy() {
    let (test_box, _repo, _remote, _id, view) = change(false);
    // The candidate renames the failing check away and raises the revision: the base policy
    // still judges it.
    write(
        &view,
        ".bay/policy.toml",
        &policy(2, 1).replace("id = \"tests\"", "id = \"other\""),
    );
    let (code, blocked) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{blocked}");
    assert_eq!(blocked["reason"], "CHECK_FAILED");
}

#[test]
fn a_policy_edit_that_keeps_the_old_revision_is_refused() {
    let (test_box, _repo, remote, _id, view) = change(true);
    let trunk = test_box.git_bare(&remote, &["rev-parse", "main"]);
    write(&view, ".bay/policy.toml", &policy(1, 2));
    let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    // No reason code covers an invalid policy edit (INTERFACES § 2), so it is a refused
    // operation with a receipt.
    assert_eq!(code, 20, "{refused}");
    assert!(
        refused.to_string().contains("must raise revision"),
        "{refused}"
    );
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), trunk);
}

#[test]
fn an_adapter_edit_without_raising_revision_is_refused() {
    let (test_box, _repo, _remote, _id, view) = change(true);
    write(&view, "scripts/bay-check", &script(true, "# adapter v2"));
    let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    // No reason code covers an invalid policy edit (INTERFACES § 2), so it is a refused
    // operation with a receipt.
    assert_eq!(code, 20, "{refused}");
    assert!(
        refused.to_string().contains("must raise revision"),
        "{refused}"
    );
}

#[test]
fn a_candidate_that_edits_its_adapter_is_judged_by_the_bases_adapter() {
    let (test_box, _repo, _remote, _id, view) = change(false);
    write(&view, "scripts/bay-check", &script(true, "# adapter v2"));
    write(&view, ".bay/policy.toml", &policy(2, 1));
    let (code, blocked) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{blocked}");
    assert_eq!(blocked["reason"], "CHECK_FAILED");
    assert!(
        !blocked["detail"]["candidate"]
            .as_str()
            .unwrap()
            .ends_with("-own"),
        "the base's adapter refused it: {blocked}"
    );
}

#[test]
fn an_adapter_edit_that_fails_under_its_own_new_adapter_is_refused() {
    let (test_box, _repo, _remote, _id, view) = change(true);
    write(&view, "scripts/bay-check", &script(false, "# adapter v2"));
    write(&view, ".bay/policy.toml", &policy(2, 1));
    let (code, blocked) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{blocked}");
    assert_eq!(blocked["reason"], "CHECK_FAILED");
    assert!(
        blocked["detail"]["candidate"]
            .as_str()
            .unwrap()
            .ends_with("-own"),
        "its own adapter refused it: {blocked}"
    );
}

#[test]
fn an_adapter_edit_that_passes_both_adapters_lands_and_its_manifest_is_the_bases() {
    let (test_box, repo, remote, _id, view) = change(true);
    let base = test_box.git_bare(&remote, &["rev-parse", "main"]);
    write(&view, "scripts/bay-check", &script(true, "# adapter v2"));
    write(&view, ".bay/policy.toml", &policy(2, 1));
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let tree = test_box.git_bare(&remote, &["rev-parse", "main^{tree}"]);
    let notes = test_box.git_ok(
        &repo,
        &[
            "notes",
            "--ref=refs/notes/bay/evidence/local",
            "show",
            &tree,
        ],
    );
    let records: Vec<serde_json::Value> = serde_json::from_str(&notes).unwrap();
    let base_manifest = bay::policy::load_at(&bay::git::Git::new(&repo), &base)
        .unwrap()
        .unwrap()
        .manifest_digest;
    assert!(
        records
            .iter()
            .any(|record| record["manifest_digest"] == base_manifest.as_str()),
        "{notes}"
    );
}

#[test]
fn a_policy_with_two_entries_for_one_check_is_refused() {
    let (test_box, repo, _remote, _id, view) = change(true);
    // Trunk gets an invalid policy (pushed directly, as a broken forge would allow).
    let doubled = format!(
        "{}\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n",
        policy(2, 1)
    );
    write(&repo, ".bay/policy.toml", &doubled);
    test_box.git_ok(&repo, &["commit", "-q", "-am", "invalid policy"]);
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);
    let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    // The policy at the accepted base breaks a validity rule (INTERFACES § 2, § 7).
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "POLICY_INVALID", "{refused}");
    assert!(
        refused.to_string().contains("two [[check]] entries"),
        "{refused}"
    );
}

#[test]
fn a_registry_signed_repair_restores_a_broken_adapter_and_an_agent_signed_one_is_refused() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let files = vec![
        (
            "scripts/bay-check".to_owned(),
            script(true, PASS_SCRIPT_MARK),
        ),
        (".bay/policy.toml".to_owned(), policy(1, 1)),
    ];
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    // A broken adapter reaches trunk (simulating a bad policy that got through).
    write(&repo, "scripts/bay-check", &script(false, "# broken"));
    write(&repo, ".bay/policy.toml", &policy(2, 1));
    test_box.git_ok(&repo, &["commit", "-q", "-am", "broken adapter"]);
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);

    let repair = |key: &Path, intent: &str| {
        let started = test_box.bay_ok(&repo, &["start", intent, "--paths", ".bay/", "scripts/"]);
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        write(&view, "scripts/bay-check", &script(true, "# fixed"));
        write(&view, ".bay/policy.toml", &policy(3, 1));
        test_box.git_ok(&repo, &["config", "bay.agentKey", key.to_str().unwrap()]);
        result_json(test_box.bay(&view).args([
            "land",
            "--wait",
            "--change",
            started["change"].as_str().unwrap(),
        ]))
    };

    // Agent-signed: checked by the broken base adapter too, so it is refused.
    let (code, refused) = repair(&keys.agent, "REPAIR-1");
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED");

    // Registry-signed: the last resort, judged by its own new adapter alone.
    let (code, repaired) = repair(&keys.registry, "REPAIR-2");
    assert_eq!(code, 0, "{repaired}");
    assert_eq!(repaired["decision"], "landed");
    assert!(
        test_box
            .git_bare(&remote, &["show", "main:scripts/bay-check"])
            .contains("# fixed")
    );
    let log = test_box.git_bare(
        &remote,
        &["log", "-1", "--format=%H", "refs/bay/log/exec-1"],
    );
    let record = test_box.git_bare(&remote, &["show", &format!("{log}:record.json")]);
    assert!(record.contains("\"policy_repair\": \"admin\""), "{record}");
}

#[test]
fn an_invalid_policy_on_trunk_is_policy_invalid_until_a_registry_signed_repair() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let files = vec![
        (
            "scripts/bay-check".to_owned(),
            script(true, PASS_SCRIPT_MARK),
        ),
        (".bay/policy.toml".to_owned(), policy(1, 1)),
    ];
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    // A policy with no check above zero reaches trunk outside bay, at revision 7.
    write(&repo, ".bay/policy.toml", &policy(7, 0));
    test_box.git_ok(&repo, &["commit", "-q", "-am", "no minimum"]);
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);

    let repair_at = |key: &Path, intent: &str, revision: u32| {
        let started = test_box.bay_ok(&repo, &["start", intent, "--paths", ".bay/", "scripts/"]);
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        write(&view, ".bay/policy.toml", &policy(revision, 1));
        test_box.git_ok(&repo, &["config", "bay.agentKey", key.to_str().unwrap()]);
        result_json(test_box.bay(&view).args([
            "land",
            "--wait",
            "--change",
            started["change"].as_str().unwrap(),
        ]))
    };

    // An ordinary change is judged by the invalid base policy: refused, never passed.
    let (code, refused) = repair_at(&keys.agent, "REPAIR-1", 8);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "POLICY_INVALID", "{refused}");
    assert_eq!(refused["retry_when"], "policy_repaired");

    // An invalid policy still states its revision: a repair below it would move revision
    // back, so it is refused.
    let (code, lowered) = repair_at(&keys.registry, "REPAIR-LOW", 3);
    assert_ne!(code, 0, "{lowered}");
    assert!(
        lowered.to_string().contains("raise revision above 7"),
        "{lowered}"
    );

    // A registry-signed repair is judged by its own new policy alone.
    let (code, repaired) = repair_at(&keys.registry, "REPAIR-2", 8);
    assert_eq!(code, 0, "{repaired}");
    assert_eq!(repaired["decision"], "landed");
}

/// The `policy_provenance` of every evidence record on the remote trunk's tree.
fn provenance_at_trunk(test_box: &TestBox, repo: &Path, remote: &Path) -> Vec<String> {
    let tree = test_box.git_bare(remote, &["rev-parse", "main^{tree}"]);
    let notes = test_box.git_ok(
        repo,
        &[
            "notes",
            "--ref=refs/notes/bay/evidence/local",
            "show",
            &tree,
        ],
    );
    let records: Vec<serde_json::Value> = serde_json::from_str(&notes).unwrap();
    assert!(!records.is_empty(), "{notes}");
    records
        .iter()
        .map(|record| {
            record["policy_provenance"]
                .as_str()
                .unwrap_or("absent")
                .to_owned()
        })
        .collect()
}

/// Every `policy_changed_outside_bay` event in the remote's executor logs.
fn outside_events(test_box: &TestBox, remote: &Path) -> Vec<serde_json::Value> {
    let logs = test_box.git_bare(
        remote,
        &["for-each-ref", "--format=%(refname)", "refs/bay/log/"],
    );
    let mut events = Vec::new();
    for log in logs.lines() {
        for commit in test_box.git_bare(remote, &["rev-list", log]).lines() {
            let text = test_box.git_bare(remote, &["show", &format!("{commit}:record.json")]);
            let record: serde_json::Value = serde_json::from_str(&text).unwrap();
            if record["kind"] == "policy_changed_outside_bay" {
                events.push(record);
            }
        }
    }
    events
}

/// Starts a change on `paths`, writes `files` in its view, and lands it.
fn land_files(
    test_box: &TestBox,
    repo: &Path,
    intent: &str,
    paths: &[&str],
    files: &[(&str, String)],
) {
    let mut args = vec!["start", intent, "--paths"];
    args.extend(paths);
    let started = test_box.bay_ok(repo, &args);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    for (path, text) in files {
        write(&view, path, text);
    }
    let (code, landed) = result_json(test_box.bay(&view).args([
        "land",
        "--wait",
        "--change",
        started["change"].as_str().unwrap(),
    ]));
    assert_eq!(code, 0, "{landed}");
}

#[test]
fn evidence_says_how_its_policy_reached_trunk() {
    let (test_box, repo, remote, _id, view) = change(true);

    // The seed added the first policy by hand. That is the bootstrap, the one way a first
    // policy reaches any trunk, and there was no policy before it to weaken: evidence under it
    // is `bay`, and no event is recorded.
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let provenance = provenance_at_trunk(&test_box, &repo, &remote);
    assert!(provenance.iter().all(|p| p == "bay"), "{provenance:?}");
    assert_eq!(
        outside_events(&test_box, &remote),
        Vec::<serde_json::Value>::new()
    );

    // An adapter edit pushed straight to the forge together with code: `outside_bay`, and one
    // event naming the adapter path and saying the landing mixed in other paths.
    let forge = test_box.clone_of(&remote, "forge-mixed");
    write(&forge, "scripts/bay-check", &script(true, "# adapter v2"));
    write(&forge, "src/m.rs", "fn m() {}\n");
    test_box.git_ok(&forge, &["add", "src/m.rs"]);
    test_box.git_ok(
        &forge,
        &["commit", "-q", "-am", "adapter and code on the forge"],
    );
    test_box.git_ok(&forge, &["push", "-q", "origin", "main"]);
    let mixed = test_box.git_ok(&forge, &["rev-parse", "HEAD"]);
    land_files(
        &test_box,
        &repo,
        "CARD-B",
        &["src/"],
        &[("src/b.rs", "fn b() {}\n".to_owned())],
    );
    let provenance = provenance_at_trunk(&test_box, &repo, &remote);
    assert!(
        provenance.iter().all(|p| p == "outside_bay"),
        "{provenance:?}"
    );
    let events = outside_events(&test_box, &remote);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["schema"], "bay.event/1");
    assert_eq!(events[0]["commit"], mixed.as_str());
    assert_eq!(events[0]["paths"], serde_json::json!(["scripts/bay-check"]));
    assert_eq!(events[0]["other_paths"], true, "{events:?}");

    // A policy change through bay that touches the policy and its adapter: evidence under it is
    // `bay`, and the old edit is not recorded again.
    land_files(
        &test_box,
        &repo,
        "POLICY-2",
        &[".bay/", "scripts/"],
        &[
            ("scripts/bay-check", script(true, "# adapter v3")),
            (".bay/policy.toml", policy(2, 1)),
        ],
    );
    land_files(
        &test_box,
        &repo,
        "CARD-C",
        &["src/"],
        &[("src/c.rs", "fn c() {}\n".to_owned())],
    );
    let provenance = provenance_at_trunk(&test_box, &repo, &remote);
    assert!(provenance.iter().all(|p| p == "bay"), "{provenance:?}");
    assert_eq!(outside_events(&test_box, &remote).len(), 1);

    // An adapter edit alone pushed straight to the forge: `outside_bay` again, one new event.
    let forge = test_box.clone_of(&remote, "forge");
    write(&forge, "scripts/bay-check", &script(true, "# adapter v4"));
    test_box.git_ok(
        &forge,
        &["commit", "-q", "-am", "adapter edit on the forge"],
    );
    test_box.git_ok(&forge, &["push", "-q", "origin", "main"]);
    let outside = test_box.git_ok(&forge, &["rev-parse", "HEAD"]);
    land_files(
        &test_box,
        &repo,
        "CARD-D",
        &["src/"],
        &[("src/d.rs", "fn d() {}\n".to_owned())],
    );
    let provenance = provenance_at_trunk(&test_box, &repo, &remote);
    assert!(
        provenance.iter().all(|p| p == "outside_bay"),
        "{provenance:?}"
    );
    let events = outside_events(&test_box, &remote);
    assert_eq!(events.len(), 2, "{events:?}");
    let new = events
        .iter()
        .find(|event| event["commit"] == outside.as_str())
        .unwrap_or_else(|| panic!("no event for {outside}: {events:?}"));
    assert_eq!(new["paths"], serde_json::json!(["scripts/bay-check"]));
    assert_eq!(new["other_paths"], false, "{new}");
}

#[test]
fn a_policy_removed_and_added_again_outside_bay_is_no_bootstrap() {
    let (test_box, repo, remote, _id, _view) = change(true);
    // Only the first add of the policy file is the bootstrap.
    let forge = test_box.clone_of(&remote, "forge-readd");
    let text = std::fs::read_to_string(forge.join(".bay/policy.toml")).unwrap();
    test_box.git_ok(&forge, &["rm", "-q", ".bay/policy.toml"]);
    test_box.git_ok(
        &forge,
        &["commit", "-q", "-m", "policy removed on the forge"],
    );
    write(&forge, ".bay/policy.toml", &text);
    test_box.git_ok(&forge, &["add", ".bay/policy.toml"]);
    test_box.git_ok(
        &forge,
        &["commit", "-q", "-m", "policy added again on the forge"],
    );
    test_box.git_ok(&forge, &["push", "-q", "origin", "main"]);
    let readded = test_box.git_ok(&forge, &["rev-parse", "HEAD"]);
    land_files(
        &test_box,
        &repo,
        "CARD-E",
        &["src/"],
        &[("src/e.rs", "fn e() {}\n".to_owned())],
    );
    let provenance = provenance_at_trunk(&test_box, &repo, &remote);
    assert!(
        provenance.iter().all(|p| p == "outside_bay"),
        "{provenance:?}"
    );
    let events = outside_events(&test_box, &remote);
    assert!(
        events
            .iter()
            .any(|event| event["commit"] == readded.as_str()
                && event["paths"] == serde_json::json!([".bay/policy.toml"])),
        "{events:?}"
    );
}

/// Lands a change under a check whose policy entry ends with `fields` and whose adapter
/// first runs `before`; the executor's environment adds `env`.
fn land_with_environment(
    fields: &str,
    before: &str,
    env: &[(&str, &str)],
) -> (i32, serde_json::Value) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let script = check_files(before, &passing_result(1), 1, 60)
        .into_iter()
        .find(|(path, _)| path == "scripts/bay-check")
        .unwrap()
        .1;
    let policy = format!("{}{fields}", policy(1, 1));
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[
            ("scripts/bay-check".to_owned(), script),
            (".bay/policy.toml".to_owned(), policy),
        ],
    );
    let (_, view) = test_box.start_and_edit(&repo);
    let mut land = test_box.bay(&view);
    land.args(["land", "--wait"]);
    for (key, value) in env {
        land.env(key, value);
    }
    result_json(&mut land)
}

#[test]
fn a_check_sees_only_the_fixed_baseline_and_its_declared_variables() {
    let home = std::env::var("HOME").unwrap_or_default();
    // Each failed condition exits without a result, so the check is unknown, never a pass.
    let before = format!(
        "[ -z \"$UNDECLARED_VAR\" ] || {{ echo inherited UNDECLARED_VAR >&2; exit 3; }}\n\
         [ \"$DECLARED_VAR\" = declared ] || {{ echo no DECLARED_VAR >&2; exit 3; }}\n\
         [ \"$PATH\" = /usr/bin:/bin ] || {{ echo PATH is $PATH >&2; exit 3; }}\n\
         [ \"$LANG\" = C.UTF-8 ] || {{ echo LANG is $LANG >&2; exit 3; }}\n\
         [ -n \"$HOME\" ] && [ \"$HOME\" != '{home}' ] && touch \"$HOME/written\" || {{ echo HOME is $HOME >&2; exit 3; }}\n\
         [ -n \"$TMPDIR\" ] && touch \"$TMPDIR/written\" || {{ echo TMPDIR is $TMPDIR >&2; exit 3; }}"
    );
    let (code, result) = land_with_environment(
        "path = [\"/usr/bin\", \"/bin\"]\nenv = [\"DECLARED_VAR\"]\n",
        &before,
        &[("UNDECLARED_VAR", "leak"), ("DECLARED_VAR", "declared")],
    );
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["decision"], "landed", "{result}");
}

#[test]
fn a_declared_variable_the_executor_does_not_set_is_an_infra_failure() {
    let (code, result) = land_with_environment("env = [\"MISSING_VAR\"]\n", "true", &[]);
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "CHECK_UNKNOWN", "{result}");
    assert_eq!(result["detail"]["checks"][0]["verdict"], "infra_failure");
    assert!(result.to_string().contains("MISSING_VAR"), "{result}");
}

#[test]
fn a_relative_path_entry_makes_the_policy_invalid() {
    let (code, result) = land_with_environment("path = [\"bin\"]\n", "true", &[]);
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "POLICY_INVALID", "{result}");
    assert!(result.to_string().contains("bin"), "{result}");
}

#[test]
fn a_check_without_path_runs_under_the_stated_default_path() {
    // An absent `path` takes its stated default, so no shell falls back to a search path the
    // policy does not name.
    let (code, result) = land_with_environment(
        "",
        "[ \"$PATH\" = /usr/bin:/bin ] || { echo PATH is $PATH >&2; exit 3; }",
        &[],
    );
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["decision"], "landed", "{result}");
}

#[test]
fn an_empty_path_or_a_baseline_name_in_env_makes_the_policy_invalid() {
    // `path = []` would be an empty PATH, which a shell reads as the current directory: the
    // candidate's tree. An `env` name of the baseline would override it from the executor.
    for fields in [
        "path = []\n",
        "env = [\"HOME\"]\n",
        "env = [\"PATH\"]\n",
        "path = [\"/usr/bin:/tmp\"]\n",
    ] {
        let (code, result) = land_with_environment(fields, "true", &[]);
        assert_eq!(code, 10, "{fields}: {result}");
        assert_eq!(result["reason"], "POLICY_INVALID", "{fields}: {result}");
    }
}

#[test]
fn a_candidates_own_invalid_policy_is_policy_invalid() {
    let (test_box, _repo, _remote, _id, view) = change(true);
    write(
        &view,
        ".bay/policy.toml",
        &format!("{}path = [\"bin\"]\n", policy(2, 1)),
    );
    let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "POLICY_INVALID", "{refused}");
    assert_eq!(refused["detail"]["policy"], "candidate", "{refused}");
}

#[test]
fn an_adapter_that_reads_an_unlisted_file_is_an_infra_failure() {
    if support::sandbox().is_none() {
        eprintln!("no check sandbox on this machine; G26's sandbox case cannot run");
        return;
    }
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let helper = repo.join("scripts/helper.sh");
    let before = format!(". '{}'", helper.display());
    let files = check_files(&before, &passing_result(1), 1, 60);
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("scripts/helper.sh", "true\n")],
        &files,
    );
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, blocked) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{blocked}");
    assert_eq!(blocked["reason"], "CHECK_UNKNOWN");
    assert_eq!(
        blocked["detail"]["checks"][0]["verdict"], "infra_failure",
        "{blocked}"
    );
}

/// A rules-only policy edit (a higher `revision`, the same checks) lands on trunk during the
/// first two checks. Each time the candidate was checked under an older policy, so it is
/// `POLICY_CHANGED` and rechecked under the new one (DESIGN § 7). That is not a `TRUNK_MOVED`
/// refusal, so it never sends the operation into the publication queue (§ 6).
#[test]
fn a_policy_edit_landing_during_the_check_is_policy_changed_and_rechecked() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let runs = test_box.path().join("runs");
    let other = test_box.path().join("policy-owner");
    // The test logs the run and lands the edit while the check waits (G62).
    let before = test_box.relay(&format!(
        "echo run >> '{runs}'; n=$(wc -l < '{runs}' | tr -d ' '); \
         if [ \"$n\" -le 2 ]; then ( cd '{other}' && git pull -q --rebase && \
         sed \"s/^revision = .*/revision = $((n + 1))/\" .bay/policy.toml > policy.new && \
         mv policy.new .bay/policy.toml && git add .bay/policy.toml && \
         git -c user.name=t -c user.email=t@t commit -q -m \"policy revision $((n + 1))\" && \
         git push -q origin main ); fi",
        runs = runs.display(),
        other = other.display(),
    ));
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files(&before, &passing_result(1), 1, 60),
    );
    test_box.clone_of(&remote, "policy-owner");
    let (id, view) = test_box.start_and_edit(&repo);

    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "landed", "{landed}");
    assert_eq!(
        std::fs::read_to_string(&runs).unwrap().lines().count(),
        3,
        "checked once per policy"
    );
    assert_eq!(
        test_box
            .git_bare(&remote, &["show", "main:.bay/policy.toml"])
            .lines()
            .next(),
        Some("revision = 3")
    );
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let op = &status["detail"]["changes"][0]["operation"];
    assert_eq!(op["policy_changed"], 2, "{status}");
    assert_eq!(op["trunk_moved"], 0, "{status}");
    // The landed tree's evidence was produced under the policy it landed with.
    let tree = test_box.git_bare(&remote, &["rev-parse", "main^{tree}"]);
    let note = test_box.git_bare(
        &remote,
        &[
            "notes",
            "--ref=refs/notes/bay/evidence/local",
            "show",
            &tree,
        ],
    );
    assert!(note.contains("\"policy_revision\": 3"), "{note}");
}

/// The operation reaches the head of the publication queue after two ordinary trunk moves.
/// While it checks as head, a rules-only policy edit lands: the head is released with
/// `policy_changed`, which counts toward no limit, rejoins, and lands after a recheck under the
/// new policy (DESIGN § 6, § 7).
#[test]
fn a_rules_only_policy_edit_makes_queued_candidates_policy_changed() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let runs = test_box.path().join("runs");
    let other = test_box.path().join("mover");
    // The test logs the run and lands the edit while the check waits (G62).
    let before = test_box.relay(&format!(
        "echo run >> '{runs}'; n=$(wc -l < '{runs}' | tr -d ' '); \
         if [ \"$n\" -le 3 ]; then ( cd '{other}' && git pull -q --rebase && \
         if [ \"$n\" -le 2 ]; then echo \"$n\" > docs/move-$n.md && git add docs; \
         else sed 's/^revision = .*/revision = 2/' .bay/policy.toml > policy.new && \
         mv policy.new .bay/policy.toml && git add .bay/policy.toml; fi && \
         git -c user.name=t -c user.email=t@t commit -q -m \"move $n\" && \
         git push -q origin main ); fi",
        runs = runs.display(),
        other = other.display(),
    ));
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("docs/keep.md", "k\n")],
        &check_files(&before, &passing_result(1), 1, 60),
    );
    test_box.clone_of(&remote, "mover");
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0);
    let executor = test_box.clone_of(&remote, "executor");

    // Two trunk moves: the operation joins the queue.
    let first = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(
        first["detail"]["ran"][0]["reason"], "PUBLICATION_QUEUED",
        "{first}"
    );
    // As head it checks while the policy changes: released as policy_changed, and it rejoins.
    let second = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(
        second["detail"]["ran"][0]["reason"], "POLICY_CHANGED",
        "{second}"
    );
    let release = test_box.git_bare(&remote, &["show", "refs/bay/queue-done/1:release.json"]);
    assert!(release.contains("\"policy_changed\""), "{release}");
    // Rechecked under the new policy, it lands.
    let third = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(third["detail"]["ran"][0]["decision"], "landed", "{third}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id.clone()]);
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let op = &status["detail"]["changes"][0]["operation"];
    assert_eq!(op["policy_changed"], 1, "{status}");
    assert_eq!(op["trunk_moved"], 2, "{status}");
    let tree = test_box.git_bare(&remote, &["rev-parse", "main^{tree}"]);
    let notes = test_box.git_bare(
        &remote,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/notes/bay/evidence/",
        ],
    );
    let evidence: String = notes
        .lines()
        .map(|notes_ref| {
            test_box.git_bare(
                &remote,
                &["notes", &format!("--ref={notes_ref}"), "show", &tree],
            )
        })
        .collect();
    assert!(evidence.contains("\"policy_revision\": 2"), "{evidence}");
}
