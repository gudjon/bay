//! G8. Invalidation stops stale work and permits repair (`DESIGN` § 3.1, § 8).
//!
//! A change declares a named input with `bay start --input <kind>:<name>`; the policy at the
//! accepted base names its provider in an `[[input]]` entry. When trunk's version of the input
//! moves past what the change observed, land is refused with `INPUT_UNOBSERVED`. After
//! `start --resume` the change has observed the new version: status says `INPUT_CHANGED` until a
//! candidate is validated under it, a candidate whose check fails is refused with
//! `INPUT_CHANGED`, and a repaired candidate lands (never a deadlock). While a contract is in
//! `expand` a change built on the old shape stays valid; only `contract` invalidates. A
//! historical consumer never blocks. A change cannot supply its own provider. A provider that
//! gives no version is a coverage gap, never a clear answer. The stale inputs of a change are
//! read from local data only. The `pre-tool` hook refuses a write that a stale input covers,
//! from the coordination cache a fetching command left (rulings R4, R5): a claim never refuses
//! a write, an old cache allows with `COVERAGE_UNKNOWN`, and with the hooks removed land still
//! refuses the same change (G23's hook clauses). A notice (`bay watch`, a hook event) changes
//! none of a change's versions, and a read in a view whose base is behind trunk never counts as
//! observing: only `start --resume` writes `observed_versions` (ruling R3).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json, write_executable};

const INPUT: &str = "contract:api";

/// The provider: the version is the contents of `api/version`; `src/` is an executable
/// consumer and `docs/archive/` a historical one.
const PROVIDER: &str = "#!/bin/sh\ncat >/dev/null\nv=$(cat api/version) || exit 1\nprintf '{\"version\":\"%s\",\"consumers\":[{\"path\":\"src/\",\"kind\":\"executable\"},{\"path\":\"docs/archive/\",\"kind\":\"historical\"}]}' \"$v\"\n";

/// The policy: one check, and the `[[input]]` entry for `contract:api` in `phase` (none: any
/// version change invalidates).
fn policy(revision: u32, phase: Option<&str>) -> String {
    let check = check_files("", "", 1, 60)
        .into_iter()
        .find(|(path, _)| path == ".bay/policy.toml")
        .unwrap()
        .1
        .replace("revision = 1", &format!("revision = {revision}"));
    let phase = phase.map_or_else(String::new, |phase| format!("phase = \"{phase}\"\n"));
    format!(
        "{check}\n[[input]]\nname = \"{INPUT}\"\nprovider = \"scripts/api-version\"\nadapter = [\"scripts/api-version\"]\npath = [\"/usr/bin\", \"/bin\"]\n{phase}"
    )
}

/// The check: a client still on `v17` against an `api/version` of `v18` is a product failure.
fn check_script() -> String {
    let pass = passing_result(1);
    let fail = pass.replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let result = format!(
        "if grep -q v17 src/client.rs && grep -q v18 api/version; then {fail}; else {pass}; fi"
    );
    check_files("", &result, 1, 60)
        .into_iter()
        .find(|(path, _)| path == "scripts/bay-check")
        .unwrap()
        .1
}

/// A trunk at `v17` with the policy in `phase`, and the work repository.
fn trunk(phase: Option<&str>) -> (TestBox, PathBuf, PathBuf) {
    trunk_with(phase, |_| PROVIDER.to_owned(), &check_script())
}

/// [`trunk`] with the provider `provider` gives for the test box, and the check `check`.
fn trunk_with(
    phase: Option<&str>,
    provider: impl Fn(&TestBox) -> String,
    check: &str,
) -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    std::fs::create_dir_all(repo.join("scripts")).unwrap();
    write_executable(&repo.join("scripts/api-version"), &provider(&test_box));
    let files = vec![
        ("scripts/bay-check".to_owned(), check.to_owned()),
        (".bay/policy.toml".to_owned(), policy(1, phase)),
    ];
    test_box.seed_with_check(
        &repo,
        &[
            ("src/a.rs", "fn a() {}\n"),
            ("src/client.rs", "uses v17\n"),
            ("api/version", "v17\n"),
            ("docs/archive/old.md", "cites v17\n"),
        ],
        &files,
    );
    (test_box, repo, remote)
}

/// Starts a change on `paths` that declares the input; returns (id, view).
fn start(test_box: &TestBox, repo: &Path, intent: &str, paths: &[&str]) -> (String, PathBuf) {
    let mut args = vec!["start", intent, "--input", INPUT, "--paths"];
    args.extend(paths);
    let started = test_box.bay_ok(repo, &args);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    (id, view)
}

/// Lands `files` on trunk outside bay (another landing changed the input).
fn move_trunk(test_box: &TestBox, repo: &Path, files: &[(&str, &str)]) -> String {
    test_box.git_ok(repo, &["fetch", "-q", "origin", "main"]);
    test_box.git_ok(repo, &["reset", "-q", "--hard", "origin/main"]);
    test_box.seed_trunk(repo, files)
}

fn change_status(test_box: &TestBox, repo: &Path, id: &str) -> serde_json::Value {
    let status = test_box.bay_ok(repo, &["status", id]);
    status["detail"]["changes"][0].clone()
}

fn land(test_box: &TestBox, view: &Path) -> (i32, serde_json::Value) {
    result_json(test_box.bay(view).args(["land", "--wait"]))
}

#[test]
fn start_records_the_declared_input_and_the_version_it_observed() {
    let (test_box, repo, _remote) = trunk(None);
    let (id, _view) = start(&test_box, &repo, "CARD-1", &["src/"]);
    let record = test_box.git_ok(
        &repo,
        &[
            "cat-file",
            "blob",
            &format!("refs/bay/changes/{id}:change.json"),
        ],
    );
    let record: serde_json::Value = serde_json::from_str(&record).unwrap();
    // A record with a named input is a new major version: a bay that cannot read named inputs
    // refuses it instead of dropping its observed versions on a rewrite.
    assert_eq!(record["schema"], "bay.change/2", "{record}");
    assert_eq!(
        record["declared_inputs"],
        serde_json::json!(["src/", {"input": INPUT}]),
        "{record}"
    );
    assert_eq!(record["observed_versions"][INPUT], "v17", "{record}");
    assert!(
        record.get("required_versions").is_none(),
        "required versions are derived on read, never stored: {record}"
    );
    let shown = change_status(&test_box, &repo, &id);
    assert_eq!(shown["invalidations"], serde_json::json!([]), "{shown}");
    assert_eq!(shown["inputs"][0]["input"], INPUT, "{shown}");
    assert_eq!(shown["inputs"][0]["coverage"], "covered", "{shown}");
}

#[test]
fn a_malformed_input_name_is_a_usage_error() {
    let (test_box, repo, _remote) = trunk(None);
    let out = test_box
        .bay(&repo)
        .args(["start", "CARD-1", "--input", "no-kind"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
}

#[test]
fn an_unobserved_change_is_refused_then_repaired_and_lands_without_deadlock() {
    let (test_box, repo, remote) = trunk(None);
    let (id, view) = start(&test_box, &repo, "CARD-1", &["src/"]);
    std::fs::write(view.join("src/feature.rs"), "fn feature() {}\n").unwrap();
    let moved = move_trunk(&test_box, &repo, &[("api/version", "v18\n")]);

    // Not observed: land is refused before any candidate is built, and says what to repair.
    let (code, refused) = land(&test_box, &view);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "INPUT_UNOBSERVED", "{refused}");
    assert_eq!(refused["detail"]["input"], INPUT, "{refused}");
    assert_eq!(refused["detail"]["observed"], "v17", "{refused}");
    assert_eq!(refused["detail"]["required"], "v18", "{refused}");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), moved);
    let shown = change_status(&test_box, &repo, &id);
    assert_eq!(shown["state"], "invalidated", "{shown}");
    assert_eq!(
        shown["invalidations"][0]["reason"], "INPUT_UNOBSERVED",
        "{shown}"
    );

    // Observed after resume: edits are allowed, but no candidate is validated under v18 yet.
    let resumed = test_box.bay_ok(&repo, &["start", "--resume", &id]);
    assert_eq!(resumed["detail"]["observed_versions"][INPUT], "v18");
    let shown = change_status(&test_box, &repo, &id);
    assert_eq!(shown["state"], "repairing", "{shown}");
    assert_eq!(
        shown["invalidations"][0]["reason"], "INPUT_CHANGED",
        "{shown}"
    );
    assert_eq!(shown["invalidations"][0]["observed"], "v18", "{shown}");
    assert_eq!(shown["invalidations"][0]["required"], "v18", "{shown}");

    // The candidate checked under v18 fails: the input it rests on needs repair.
    let (code, refused) = land(&test_box, &view);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "INPUT_CHANGED", "{refused}");
    assert_eq!(refused["detail"]["input"], INPUT, "{refused}");
    assert_eq!(refused["detail"]["check"], "CHECK_FAILED", "{refused}");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), moved);

    // The repair: a candidate validated under v18 lands.
    std::fs::write(view.join("src/client.rs"), "uses v18\n").unwrap();
    let (code, landed) = land(&test_box, &view);
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "landed", "{landed}");
    assert_eq!(test_box.trunk_change_ids(&remote)[0], id);
    let shown = change_status(&test_box, &repo, &id);
    let validated = &shown["validated_candidate"];
    assert_eq!(
        validated["candidate"], shown["operation"]["candidate"],
        "the landed attempt is the validated candidate: {shown}"
    );
    assert_eq!(validated["versions"][INPUT], "v18", "{shown}");
}

#[test]
fn a_resumed_change_whose_check_passes_lands_at_once() {
    let (test_box, repo, _remote) = trunk(None);
    let (id, view) = start(&test_box, &repo, "CARD-1", &["src/"]);
    std::fs::write(view.join("src/feature.rs"), "fn feature() {}\n").unwrap();
    std::fs::write(view.join("src/client.rs"), "uses v18\n").unwrap();
    move_trunk(&test_box, &repo, &[("api/version", "v18\n")]);
    test_box.bay_ok(&repo, &["start", "--resume", &id]);
    let (code, landed) = land(&test_box, &view);
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "landed", "{landed}");
}

#[test]
fn in_expand_a_consumer_stays_valid_and_only_contract_invalidates() {
    let (test_box, repo, _remote) = trunk(Some("expand"));
    let (_a, view_a) = start(&test_box, &repo, "CARD-1", &["src/"]);
    let (b, view_b) = start(&test_box, &repo, "CARD-2", &["src/"]);
    std::fs::write(view_a.join("src/feature_a.rs"), "fn a2() {}\n").unwrap();
    std::fs::write(view_b.join("src/feature_b.rs"), "fn b2() {}\n").unwrap();
    // The compatible provider: the old shape is still served.
    move_trunk(
        &test_box,
        &repo,
        &[
            ("api/version", "v17.1\n"),
            (".bay/policy.toml", &policy(2, Some("expand"))),
        ],
    );
    let (code, landed) = land(&test_box, &view_a);
    assert_eq!(
        code, 0,
        "a change built on the old shape stays valid: {landed}"
    );

    // The old shape is removed: the remaining consumer is invalidated.
    move_trunk(
        &test_box,
        &repo,
        &[(".bay/policy.toml", &policy(3, Some("contract")))],
    );
    let (code, refused) = land(&test_box, &view_b);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "INPUT_UNOBSERVED", "{refused}");
    assert_eq!(refused["detail"]["phase"], "contract", "{refused}");
    let shown = change_status(&test_box, &repo, &b);
    assert_eq!(
        shown["invalidations"][0]["reason"], "INPUT_UNOBSERVED",
        "{shown}"
    );
}

#[test]
fn a_historical_consumer_never_blocks() {
    let (test_box, repo, _remote) = trunk(None);
    let (id, view) = start(&test_box, &repo, "CARD-1", &["docs/archive/"]);
    std::fs::write(view.join("docs/archive/new.md"), "notes\n").unwrap();
    // The landing that moved the contract also migrated its executable consumers.
    move_trunk(
        &test_box,
        &repo,
        &[("api/version", "v18\n"), ("src/client.rs", "uses v18\n")],
    );
    let shown = change_status(&test_box, &repo, &id);
    assert_eq!(shown["invalidations"], serde_json::json!([]), "{shown}");
    assert_eq!(shown["inputs"][0]["consumer"], "historical", "{shown}");
    let (code, landed) = land(&test_box, &view);
    assert_eq!(code, 0, "{landed}");
}

#[test]
fn a_change_cannot_supply_its_own_provider() {
    let (test_box, repo, _remote) = trunk(None);
    let (_id, view) = start(&test_box, &repo, "CARD-1", &["src/", "scripts/", ".bay/"]);
    move_trunk(&test_box, &repo, &[("api/version", "v18\n")]);
    // The change's own provider would report the version it observed; it is not the policy.
    write_executable(
        &view.join("scripts/api-version"),
        "#!/bin/sh\ncat >/dev/null\nprintf '{\"version\":\"v17\",\"consumers\":[]}'\n",
    );
    std::fs::write(view.join(".bay/policy.toml"), policy(2, None)).unwrap();
    let (code, refused) = land(&test_box, &view);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "INPUT_UNOBSERVED", "{refused}");
    assert_eq!(refused["detail"]["required"], "v18", "{refused}");
}

#[test]
fn a_provider_with_no_version_is_a_coverage_gap() {
    let (test_box, repo, _remote) = trunk(None);
    let (id, view) = start(&test_box, &repo, "CARD-1", &["src/"]);
    std::fs::write(view.join("src/feature.rs"), "fn feature() {}\n").unwrap();
    // A provider that breaks on trunk's tree gives no version.
    test_box.git_ok(&repo, &["fetch", "-q", "origin", "main"]);
    test_box.git_ok(&repo, &["reset", "-q", "--hard", "origin/main"]);
    test_box.git_ok(&repo, &["rm", "-q", "api/version"]);
    test_box.git_ok(&repo, &["commit", "-q", "-m", "drop api/version"]);
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);
    let shown = change_status(&test_box, &repo, &id);
    assert_eq!(shown["inputs"][0]["coverage"], "unknown", "{shown}");
    assert!(shown["inputs"][0]["why"].is_string(), "{shown}");
    assert_eq!(shown["invalidations"], serde_json::json!([]), "{shown}");
    // G8 fails closed: trunk names a provider for the input and it gives no version, so no
    // one can say the change observed what trunk requires. Publication waits for an answer.
    let (code, refused) = land(&test_box, &view);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "COVERAGE_UNKNOWN", "{refused}");
    assert_eq!(refused["detail"]["input"], INPUT, "{refused}");
    assert_eq!(
        refused["detail"]["inputs"][0]["coverage"], "unknown",
        "{refused}"
    );
    // Never a deadlock: once trunk's provider answers again, the same revision lands.
    move_trunk(&test_box, &repo, &[("api/version", "v17\n")]);
    let (code, landed) = land(&test_box, &view);
    assert_eq!(code, 0, "{landed}");
    assert_eq!(
        landed["detail"]["inputs"][0]["coverage"], "covered",
        "{landed}"
    );
}

#[test]
fn a_policy_that_turns_its_provider_off_is_invalid() {
    let (test_box, repo, _remote) = trunk(None);
    let (_id, view) = start(&test_box, &repo, "CARD-1", &["src/"]);
    std::fs::write(view.join("src/feature.rs"), "fn feature() {}\n").unwrap();
    // A timeout of zero would make every provider time out: coverage off with no notice.
    let off = format!("{}timeout_s = 0\n", policy(2, None));
    move_trunk(&test_box, &repo, &[(".bay/policy.toml", &off)]);
    let (code, refused) = land(&test_box, &view);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "POLICY_INVALID", "{refused}");
}

#[test]
fn a_provider_path_that_climbs_out_of_its_adapter_makes_the_policy_invalid() {
    let (test_box, repo, _remote) = trunk(None);
    let (_id, view) = start(&test_box, &repo, "CARD-1", &["src/"]);
    std::fs::write(view.join("src/feature.rs"), "fn feature() {}\n").unwrap();
    // `scripts/../..` names no file of the adapter: it reaches a host binary no policy lists.
    let climbing = policy(2, None)
        .replace(
            "adapter = [\"scripts/api-version\"]",
            "adapter = [\"scripts/\"]",
        )
        .replace(
            "provider = \"scripts/api-version\"",
            &format!("provider = \"scripts/{}bin/cat\"", "../".repeat(40)),
        );
    move_trunk(&test_box, &repo, &[(".bay/policy.toml", &climbing)]);
    let (code, refused) = land(&test_box, &view);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "POLICY_INVALID", "{refused}");
}

#[test]
fn a_planted_provider_answer_never_passes_the_land_gate() {
    let (test_box, repo, _remote) = trunk(None);
    let (id, view) = start(&test_box, &repo, "CARD-1", &["src/"]);
    std::fs::write(view.join("src/feature.rs"), "fn feature() {}\n").unwrap();
    move_trunk(&test_box, &repo, &[("api/version", "v18\n")]);
    // A view fills the local answer cache; then a process that can write the repository
    // rewrites every answer to the version the change observed.
    test_box.bay_ok(&repo, &["status", &id]);
    let common = PathBuf::from(test_box.git_ok(&repo, &["rev-parse", "--git-common-dir"]));
    let common = if common.is_absolute() {
        common
    } else {
        repo.join(common)
    };
    let cache = common.join("bay/inputs");
    let mut planted = 0;
    for entry in std::fs::read_dir(&cache).unwrap().flatten() {
        if entry.path().extension().is_some_and(|ext| ext == "json") {
            std::fs::write(entry.path(), r#"{"version":"v17","consumers":[]}"#).unwrap();
            planted += 1;
        }
    }
    assert!(
        planted > 0,
        "the view cached no answer in {}",
        cache.display()
    );
    let (code, refused) = land(&test_box, &view);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "INPUT_UNOBSERVED", "{refused}");
    assert_eq!(refused["detail"]["required"], "v18", "{refused}");
}

#[test]
fn land_without_wait_refuses_an_unobserved_change_at_once() {
    let (test_box, repo, _remote) = trunk(None);
    let (_id, view) = start(&test_box, &repo, "CARD-1", &["src/"]);
    std::fs::write(view.join("src/feature.rs"), "fn feature() {}\n").unwrap();
    move_trunk(&test_box, &repo, &[("api/version", "v18\n")]);
    let (code, refused) = result_json(test_box.bay(&view).args(["land"]));
    assert_eq!(code, 10, "refused at admit, never queued: {refused}");
    assert_eq!(refused["reason"], "INPUT_UNOBSERVED", "{refused}");
    assert!(refused["op_id"].is_string(), "{refused}");
}

#[test]
fn reading_the_required_version_rearms_a_refused_land() {
    // The provider fails while `<root>/flaky` exists: start observes nothing.
    // The provider asks the test: it sees nothing of the box (G62).
    let provider = |test_box: &TestBox| {
        format!(
            "#!/bin/sh\nif {}then exit 1; fi\n{}",
            test_box.relay(&format!(
                "[ -e '{}' ]",
                test_box.path().join("flaky").display()
            )),
            PROVIDER.trim_start_matches("#!/bin/sh\n")
        )
    };
    let (test_box, repo, _remote) = trunk_with(None, provider, &check_script());
    let flaky = test_box.path().join("flaky");
    std::fs::write(&flaky, "").unwrap();
    let (id, view) = start(&test_box, &repo, "CARD-1", &["src/"]);
    std::fs::write(view.join("src/feature.rs"), "fn feature() {}\n").unwrap();
    std::fs::remove_file(&flaky).unwrap();
    let (code, refused) = land(&test_box, &view);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "INPUT_UNOBSERVED", "{refused}");
    // Neither the revision nor trunk moves; the actor reads the required version.
    let resumed = test_box.bay_ok(&repo, &["start", "--resume", &id]);
    assert_eq!(resumed["detail"]["observed_versions"][INPUT], "v17");
    let (code, landed) = land(&test_box, &view);
    assert_eq!(code, 0, "never a deadlock: {landed}");
}

#[test]
fn a_finished_change_keeps_its_state_and_runs_no_provider() {
    // The provider leaves `<root>/ran` behind each time it runs.
    let provider = |test_box: &TestBox| {
        format!(
            "#!/bin/sh\n{}{}",
            test_box.relay(&format!(": > '{}'", test_box.path().join("ran").display())),
            PROVIDER.trim_start_matches("#!/bin/sh\n")
        )
    };
    let (test_box, repo, _remote) = trunk_with(None, provider, &check_script());
    let (id, view) = start(&test_box, &repo, "CARD-1", &["src/"]);
    move_trunk(&test_box, &repo, &[("api/version", "v18\n")]);
    let (code, abandoned) = result_json(test_box.bay(&view).args(["land", "--abandon"]));
    assert_eq!(code, 0, "{abandoned}");
    let ran = test_box.path().join("ran");
    let _ = std::fs::remove_file(&ran);
    let shown = change_status(&test_box, &repo, &id);
    assert_eq!(shown["state"], "abandoned", "{shown}");
    assert_eq!(shown["invalidations"], serde_json::json!([]), "{shown}");
    assert!(
        !ran.exists(),
        "status ran a provider for an abandoned change"
    );
}

#[test]
fn the_hooks_read_only_answers_already_in_the_cache() {
    let provider = |test_box: &TestBox| {
        format!(
            "#!/bin/sh\n{}{}",
            test_box.relay(&format!(": > '{}'", test_box.path().join("ran").display())),
            PROVIDER.trim_start_matches("#!/bin/sh\n")
        )
    };
    let (test_box, repo, _remote) = trunk_with(None, provider, &check_script());
    let (id, _view) = start(&test_box, &repo, "CARD-1", &["src/"]);
    // Trunk moves; the local cache has its commit but no provider answer for its tree.
    move_trunk(&test_box, &repo, &[("api/version", "v18\n")]);
    test_box.git_ok(&repo, &["fetch", "-q", "origin"]);
    let ran = test_box.path().join("ran");
    let _ = std::fs::remove_file(&ran);
    let opened = bay::repo::Repo::open(&repo).unwrap();
    let (_, record) = bay::change::Chain::new(&opened.git).read(&id).unwrap();
    let stale = bay::inputs::stale_inputs(&opened, &record).unwrap();
    assert!(stale.is_empty(), "{stale:?}");
    let cached = bay::status::cached(&opened, &|_, _| true).unwrap();
    let shown = &cached.detail.unwrap()["changes"][0];
    assert_eq!(shown["inputs"][0]["coverage"], "unknown", "{shown}");
    assert!(!ran.exists(), "a hook path ran a provider");
}

#[test]
fn a_provider_runs_in_the_check_sandbox() {
    if support::sandbox().is_none() {
        eprintln!("skipped: this machine has no check sandbox");
        return;
    }
    // The provider reports `leaked` when it can read a file of the repository's work tree.
    let provider = |test_box: &TestBox| {
        format!(
            "#!/bin/sh\ncat >/dev/null\nif cat '{}' >/dev/null 2>&1; then v=leaked; else v=$(cat api/version); fi\nprintf '{{\"version\":\"%s\",\"consumers\":[]}}' \"$v\"\n",
            test_box.path().join("repo/secret.txt").display()
        )
    };
    let (test_box, repo, _remote) = trunk_with(None, provider, &check_script());
    std::fs::write(repo.join("secret.txt"), "a secret\n").unwrap();
    let (id, _view) = start(&test_box, &repo, "CARD-1", &["src/"]);
    let shown = change_status(&test_box, &repo, &id);
    assert_eq!(shown["inputs"][0]["observed"], "v17", "{shown}");
}

/// Ruling R14: on a machine with no check sandbox a provider runs only by the box's own choice,
/// `git config --global bay.executorSandbox none`. The sandbox is hidden by a failing `bwrap`
/// on `PATH`, so this runs on Linux.
#[cfg(target_os = "linux")]
#[test]
fn a_provider_never_runs_unconfined_unless_the_box_chose_it() {
    let provider = |test_box: &TestBox| {
        format!(
            "#!/bin/sh\ntouch '{}'\n{}",
            test_box.path().join("provider-ran").display(),
            PROVIDER.trim_start_matches("#!/bin/sh\n")
        )
    };
    let (test_box, repo, _remote) = trunk_with(None, provider, &check_script());
    let ran = test_box.path().join("provider-ran");
    test_box.no_sandbox();
    test_box.choose_unsandboxed(false);
    let _ = std::fs::remove_file(&ran);
    let (id, _view) = start(&test_box, &repo, "CARD-1", &["src/"]);
    assert!(
        !ran.exists(),
        "a provider ran with no sandbox and no choice"
    );
    let shown = change_status(&test_box, &repo, &id);
    assert_ne!(shown["inputs"][0]["observed"], "v17", "{shown}");

    test_box.choose_unsandboxed(true);
    let (id, _view) = start(&test_box, &repo, "CARD-2", &["src/"]);
    assert!(
        ran.exists(),
        "the box chose no sandbox, yet the provider did not run"
    );
    let shown = change_status(&test_box, &repo, &id);
    assert_eq!(shown["inputs"][0]["observed"], "v17", "{shown}");
}

#[test]
fn a_provider_that_leaves_a_child_behind_is_still_bounded() {
    // The provider answers, but a background child keeps its output open far past its timeout.
    let provider = |_: &TestBox| {
        format!(
            "#!/bin/sh\nsleep 60 &\n{}",
            PROVIDER.trim_start_matches("#!/bin/sh\n")
        )
    };
    let (test_box, repo, _remote) = trunk_with(None, provider, &check_script());
    let started = std::time::Instant::now();
    let (id, _view) = start(&test_box, &repo, "CARD-1", &["src/"]);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(40),
        "start waited {:?} on a provider's child",
        started.elapsed()
    );
    let shown = change_status(&test_box, &repo, &id);
    assert_eq!(shown["inputs"][0]["observed"], "v17", "{shown}");
}

#[test]
fn the_check_is_told_the_input_versions_it_validates() {
    // This check passes only when its request names the version of the input it checks under.
    let pass = passing_result(1);
    let fail = pass.replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let result = format!(
        "if printf '%s' \"$req\" | grep -q '\"inputs\":\\[\"{INPUT}@v17\"\\]'; then {pass}; else {fail}; fi"
    );
    let check = check_files("", &result, 1, 60)
        .into_iter()
        .find(|(path, _)| path == "scripts/bay-check")
        .unwrap()
        .1;
    let (test_box, repo, _remote) = trunk_with(None, |_| PROVIDER.to_owned(), &check);
    let (_id, view) = start(&test_box, &repo, "CARD-1", &["src/"]);
    std::fs::write(view.join("src/feature.rs"), "fn feature() {}\n").unwrap();
    let (code, landed) = land(&test_box, &view);
    assert_eq!(code, 0, "{landed}");
}

#[test]
fn a_left_over_provider_scratch_is_swept() {
    let (test_box, repo, _remote) = trunk(None);
    let common = PathBuf::from(test_box.git_ok(&repo, &["rev-parse", "--git-common-dir"]));
    let common = if common.is_absolute() {
        common
    } else {
        repo.join(common)
    };
    // A run killed mid-way an hour ago left its materialized tree behind.
    let left = common.join("bay/inputs-run/run-1-old");
    std::fs::create_dir_all(left.join("tree")).unwrap();
    let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::open(&left)
        .unwrap()
        .set_modified(hour_ago)
        .unwrap();
    start(&test_box, &repo, "CARD-1", &["src/"]);
    assert!(!left.exists(), "{} was left behind", left.display());
}

#[test]
fn the_stale_inputs_of_a_change_are_read_from_local_data_only() {
    let (test_box, repo, _remote) = trunk(None);
    let (id, _view) = start(&test_box, &repo, "CARD-1", &["src/"]);
    move_trunk(&test_box, &repo, &[("api/version", "v18\n")]);
    // One fetch brings trunk into the local cache; then the remote is gone.
    test_box.bay_ok(&repo, &["status", &id]);
    test_box.git_ok(
        &repo,
        &["remote", "set-url", "origin", "/nonexistent/remote.git"],
    );
    let opened = bay::repo::Repo::open(&repo).unwrap();
    let (_, record) = bay::change::Chain::new(&opened.git).read(&id).unwrap();
    let stale = bay::inputs::stale_inputs(&opened, &record).unwrap();
    assert_eq!(stale.len(), 1, "{stale:?}");
    assert_eq!(stale[0].input.as_str(), INPUT);
    assert_eq!(stale[0].state, bay::inputs::InputState::Unobserved);
    assert_eq!(stale[0].observed.as_deref(), Some("v17"));
    assert_eq!(stale[0].required.as_deref(), Some("v18"));
}

#[test]
fn a_provider_outside_its_adapter_list_makes_the_policy_invalid() {
    let (test_box, repo, _remote) = trunk(None);
    let (_id, view) = start(&test_box, &repo, "CARD-1", &["src/"]);
    std::fs::write(view.join("src/feature.rs"), "fn feature() {}\n").unwrap();
    let invalid = policy(2, None).replace(
        "adapter = [\"scripts/api-version\"]",
        "adapter = [\"api/version\"]",
    );
    move_trunk(&test_box, &repo, &[(".bay/policy.toml", &invalid)]);
    let (code, refused) = land(&test_box, &view);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "POLICY_INVALID", "{refused}");
}

/// `bay hook pre-tool` with probed payload 03 (a `Write`), its tool, target and session
/// rewritten; returns (exit, stdout, stderr).
fn pre_tool(
    test_box: &TestBox,
    cwd: &Path,
    tool: &str,
    file: &Path,
    session: &str,
) -> (Option<i32>, String, String) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code-2.1.302/03-PreToolUse.json");
    let mut payload: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fixture).unwrap()).unwrap();
    payload["session_id"] = session.into();
    payload["cwd"] = cwd.display().to_string().into();
    payload["tool_name"] = tool.into();
    payload["tool_input"]["file_path"] = file.display().to_string().into();
    hook_run(test_box, cwd, &payload)
}

fn hook_run(
    test_box: &TestBox,
    cwd: &Path,
    payload: &serde_json::Value,
) -> (Option<i32>, String, String) {
    use std::io::Write as _;
    let mut child = test_box
        .bay_process(cwd)
        .args(["hook", "pre-tool"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn common_dir(test_box: &TestBox, repo: &Path) -> PathBuf {
    let common = PathBuf::from(test_box.git_ok(repo, &["rev-parse", "--git-common-dir"]));
    if common.is_absolute() {
        common
    } else {
        repo.join(common)
    }
}

/// The last line of a session's work record.
fn last_row(test_box: &TestBox, repo: &Path, session: &str) -> serde_json::Value {
    let path = common_dir(test_box, repo)
        .join("bay/sessions")
        .join(format!("{session}.jsonl"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(text.lines().last().unwrap()).unwrap()
}

/// Moves the coordination cache's fetch time `secs` into the past.
fn age_cache(test_box: &TestBox, repo: &Path, secs: u64) {
    let path = common_dir(test_box, repo).join("bay/coord.json");
    let mut coord: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let at = coord["fetched_at_s"].as_u64().unwrap();
    coord["fetched_at_s"] = (at - secs).into();
    std::fs::write(&path, coord.to_string()).unwrap();
}

/// The committed `.bay/config.toml` with `[hooks] <hooks>` in place of any earlier `[hooks]`,
/// committed to trunk, where bay reads it (ruling: config location).
fn hooks_config(test_box: &TestBox, repo: &Path, hooks: &str) {
    let committed = test_box.git_ok(repo, &["show", "refs/remotes/origin/main:.bay/config.toml"]);
    let base = committed.split("\n[hooks]\n").next().unwrap_or_default();
    test_box.commit_config(repo, &format!("{base}\n[hooks]\n{hooks}\n"));
}

/// A change on `src/` and `docs/archive/` that declares the input, and a trunk that moved its
/// version past what the change observed, fetched by `bay status`. The cache stays fresh for
/// 600 s, so a slow box cannot turn a refusal into a notice.
fn stale_change() -> (TestBox, PathBuf, String, PathBuf) {
    let (test_box, repo, _remote) = trunk(None);
    let (id, view) = start(&test_box, &repo, "CARD-1", &["src/", "docs/archive/"]);
    move_trunk(&test_box, &repo, &[("api/version", "v18\n")]);
    hooks_config(&test_box, &repo, "cache_max_age_s = 600");
    test_box.bay_ok(&repo, &["status"]);
    (test_box, repo, id, view)
}

/// Writes `text` as the coordination cache.
fn put_cache(test_box: &TestBox, repo: &Path, text: &str) {
    std::fs::write(common_dir(test_box, repo).join("bay/coord.json"), text).unwrap();
}

/// The one `COVERAGE_UNKNOWN` notice a hook allowed a write with: on stdout as
/// `additionalContext` and on stderr, at exit 0.
fn assert_notice(found: &(Option<i32>, String, String), what: &str) -> String {
    let (code, stdout, stderr) = found;
    assert_eq!(*code, Some(0), "{what}: {stdout} {stderr}");
    let output: serde_json::Value = serde_json::from_str(stdout)
        .unwrap_or_else(|error| panic!("{what}: no notice ({error}): {stdout:?} {stderr}"));
    let context = output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(context.contains("COVERAGE_UNKNOWN"), "{what}: {context}");
    assert!(stderr.contains("COVERAGE_UNKNOWN"), "{what}: {stderr}");
    context
}

#[test]
fn the_hook_judges_the_target_where_it_lands_and_a_config_it_cannot_read_is_not_silent() {
    let (test_box, repo, id, view) = stale_change();
    let target = view.join("src/feature.rs");
    // A session outside the repository writes into the view: the view that holds the target
    // judges it (R-B3-3), not the session's directory.
    let outside = test_box.path().join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    let (code, stdout, stderr) = pre_tool(&test_box, &outside, "Write", &target, "s-6");
    assert_eq!(code, Some(2), "{stdout} {stderr}");
    assert!(stderr.contains("INPUT_UNOBSERVED"), "{stderr}");
    // A relative target, and one through a symbolic link and `..`, are resolved as the system
    // resolves them: `<link>/..` is the view, not the link's directory.
    let (code, _, stderr) = pre_tool(&test_box, &view, "Write", Path::new("src/x.rs"), "s-6");
    assert_eq!(code, Some(2), "{stderr}");
    std::os::unix::fs::symlink(view.join("src"), outside.join("link")).unwrap();
    let through = outside.join("link/../src/feature.rs");
    let (code, stdout, stderr) = pre_tool(&test_box, &outside, "Write", &through, "s-6");
    assert_eq!(code, Some(2), "{through:?}: {stdout} {stderr}");
    // A config bay cannot read cannot say how old a cache may be: a notice, never silent.
    test_box.commit_config(&repo, "[hooks]\ncache_max_age_s = \"soon\"\n");
    let found = pre_tool(&test_box, &view, "Write", &target, "s-6");
    let context = assert_notice(&found, "an unreadable config");
    assert!(context.contains(&id), "{context}");
    // A write in no view of that repository stays silent.
    let (code, stdout, _) = pre_tool(&test_box, &repo, "Write", &repo.join("src/x.rs"), "s-6");
    assert_eq!((code, stdout.as_str()), (Some(0), ""));
}

#[test]
fn a_cache_that_cannot_say_gives_a_notice_only_where_an_input_could_cover() {
    let (test_box, repo, _id, view) = stale_change();
    let target = view.join("src/feature.rs");
    let started = test_box.bay_ok(&repo, &["start", "CARD-2", "--paths", "src/"]);
    let plain = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    // An edit tool whose payload names no `file_path` (`NotebookEdit` sends another field,
    // which no probe recorded): bay cannot say what it writes, so it says so.
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code-2.1.302/03-PreToolUse.json");
    let mut payload: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fixture).unwrap()).unwrap();
    payload["session_id"] = "s-7".into();
    payload["cwd"] = view.display().to_string().into();
    payload["tool_name"] = "NotebookEdit".into();
    payload["tool_input"] = serde_json::json!({"notebook_path": target, "new_source": "x"});
    let context = assert_notice(&hook_run(&test_box, &view, &payload), "no file_path");
    assert!(context.contains(INPUT), "{context}");
    // A fetch time past what a clock can hold is a notice, not a crash.
    let path = common_dir(&test_box, &repo).join("bay/coord.json");
    let good = std::fs::read_to_string(&path).unwrap();
    let mut far: serde_json::Value = serde_json::from_str(&good).unwrap();
    far["fetched_at_s"] = u64::MAX.into();
    put_cache(&test_box, &repo, &far.to_string());
    let found = pre_tool(&test_box, &view, "Write", &target, "s-7");
    assert_notice(&found, "a fetch time out of range");
    // A cache that does not parse, or none at all: a notice for the change with an input,
    // silence for the change that declares none.
    for (what, text) in [("garbage", Some("{not json")), ("absent", None)] {
        match text {
            Some(text) => put_cache(&test_box, &repo, text),
            None => std::fs::remove_file(&path).unwrap(),
        }
        assert_notice(&pre_tool(&test_box, &view, "Write", &target, "s-7"), what);
        let (code, stdout, stderr) =
            pre_tool(&test_box, &plain, "Write", &plain.join("src/a.rs"), "s-8");
        assert_eq!((code, stdout.as_str()), (Some(0), ""), "{what}: {stderr}");
    }
}

#[test]
fn a_looping_executor_writes_the_cache_after_each_pass_that_fetched() {
    let (test_box, repo, remote) = trunk(None);
    let (id, view) = start(&test_box, &repo, "CARD-1", &["src/"]);
    std::fs::write(view.join("src/feature.rs"), "fn feature() {}\n").unwrap();
    let (code, queued) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{queued}");
    let clone = test_box.clone_of(&remote, "executor");
    test_box.git_ok(&clone, &["config", "bay.executorName", "exec"]);
    test_box.git_ok(&clone, &["config", "bay.producerName", "exec-checks"]);
    let cache = clone.join(".git/bay/coord.json");
    let mut executor = test_box
        .bay_process(&clone)
        .args(["executor", "run", "--interval-s", "1"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    let (landed, written) = loop {
        let landed = test_box.trunk_change_ids(&remote).contains(&id);
        if (landed && cache.exists()) || std::time::Instant::now() > deadline {
            break (landed, cache.exists());
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    let still_running = executor.try_wait().unwrap().is_none();
    executor.kill().unwrap();
    let _ = executor.wait();
    assert!(still_running, "the executor loops");
    assert!(landed, "the executor's pass landed the change");
    assert!(
        written,
        "a looping executor never wrote {}",
        cache.display()
    );
}

#[test]
fn the_hook_refuses_a_write_its_stale_input_covers() {
    let (test_box, repo, id, view) = stale_change();
    for tool in ["Write", "Edit"] {
        let (code, stdout, stderr) =
            pre_tool(&test_box, &view, tool, &view.join("src/feature.rs"), "s-1");
        assert_eq!(code, Some(2), "{tool}: {stdout} {stderr}");
        assert_eq!(stderr.lines().count(), 1, "one line: {stderr}");
        assert!(stderr.contains("INPUT_UNOBSERVED"), "{stderr}");
        assert!(stderr.contains(INPUT), "{stderr}");
        assert!(
            stderr.contains(&format!("bay start --resume {id}")),
            "{stderr}"
        );
    }
    // The refusal is recorded in the session's row.
    let row = last_row(&test_box, &repo, "s-1");
    assert_eq!(row["state"], "blocked(INPUT_UNOBSERVED)", "{row}");
    assert_eq!(row["refused"]["input"], INPUT, "{row}");
    // G60: the row's verdict in AGENT-HOOKS-0.1's words.
    assert_eq!(
        row["verdict"],
        serde_json::json!({"decision": "deny", "reason": "INPUT_UNOBSERVED"}),
        "{row}"
    );
    assert_eq!(row["change"], id.as_str(), "{row}");
    // The refusal's exit is the shell deny list's (G39).
    let deny = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": "git reset --hard"},
        "cwd": view,
    });
    let (denied, _, _) = hook_run(&test_box, &view, &deny);
    assert_eq!(denied, Some(2));
    // Once the change observed the version, the repair is an edit: never a deadlock.
    test_box.bay_ok(&repo, &["start", "--resume", &id]);
    let (code, stdout, stderr) = pre_tool(
        &test_box,
        &view,
        "Write",
        &view.join("src/client.rs"),
        "s-1",
    );
    assert_eq!(code, Some(0), "{stdout} {stderr}");
    assert_eq!(stdout, "", "a write after the repair is silent");
}

#[test]
fn the_hook_allows_a_write_no_stale_input_covers_and_a_claim_never_refuses() {
    let (test_box, repo, _id, view) = stale_change();
    // A declared path that is no consumer of the input (a historical one): R4 judges by the
    // provider's consumer paths, not by every declared path.
    let (code, stdout, stderr) = pre_tool(
        &test_box,
        &view,
        "Write",
        &view.join("docs/archive/new.md"),
        "s-2",
    );
    assert_eq!(code, Some(0), "{stdout} {stderr}");
    assert_eq!(stdout, "", "{stdout}");
    // A path in no view.
    let (code, _, stderr) = pre_tool(&test_box, &repo, "Write", &repo.join("src/x.rs"), "s-2");
    assert_eq!(code, Some(0), "{stderr}");
    // Another change claims the same paths and declares no input: its writes are allowed.
    let started = test_box.bay_ok(&repo, &["start", "CARD-2", "--paths", "src/"]);
    let other_view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let (code, stdout, stderr) = pre_tool(
        &test_box,
        &other_view,
        "Edit",
        &other_view.join("src/a.rs"),
        "s-3",
    );
    assert_eq!(code, Some(0), "{stdout} {stderr}");
    assert_eq!(stdout, "", "{stdout}");
}

#[test]
fn an_old_cache_allows_the_write_with_a_coverage_unknown_notice() {
    let (test_box, repo, id, view) = stale_change();
    age_cache(&test_box, &repo, 3600);
    let (code, stdout, stderr) = pre_tool(
        &test_box,
        &view,
        "Write",
        &view.join("src/feature.rs"),
        "s-4",
    );
    assert_eq!(
        code,
        Some(0),
        "R5: an old cache allows, with a notice: {stderr}"
    );
    let output: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(output["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    let context = output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.contains("COVERAGE_UNKNOWN"), "{context}");
    assert!(context.contains("cache_max_age_s = 600"), "{context}");
    // The same notice on stderr at exit 0: PreToolUse's additionalContext is not probed.
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.contains("COVERAGE_UNKNOWN"), "{stderr}");
    let row = last_row(&test_box, &repo, "s-4");
    assert_eq!(row["notice"]["reason"], "COVERAGE_UNKNOWN", "{row}");
    assert_eq!(row["verdict"]["decision"], "allow", "G60: {row}");
    let age = row["notice"]["cache_age_s"].as_u64().unwrap();
    assert!(age >= 3600, "{row}");
    assert!(context.contains(&format!("{age} s old")), "{context}");
    assert_eq!(row["change"], id.as_str(), "{row}");
    // A write no declared input covers is still silent.
    let (code, stdout, _) = pre_tool(
        &test_box,
        &view,
        "Write",
        &view.join("docs/archive/new.md"),
        "s-4",
    );
    assert_eq!((code, stdout.as_str()), (Some(0), ""));
    // A longer [hooks] cache_max_age_s makes the same cache fresh: the write is refused.
    hooks_config(&test_box, &repo, "cache_max_age_s = 7200");
    let (code, _, stderr) = pre_tool(
        &test_box,
        &view,
        "Write",
        &view.join("src/feature.rs"),
        "s-4",
    );
    assert_eq!(code, Some(2), "{stderr}");
}

#[test]
fn with_the_hooks_removed_land_refuses_the_same_stale_change() {
    let (test_box, _repo, _id, view) = stale_change();
    // The hook refuses this write; made without the hooks, land refuses the change.
    let (code, _, _) = pre_tool(
        &test_box,
        &view,
        "Write",
        &view.join("src/feature.rs"),
        "s-5",
    );
    assert_eq!(code, Some(2));
    std::fs::write(view.join("src/feature.rs"), "fn feature() {}\n").unwrap();
    let (code, refused) = land(&test_box, &view);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "INPUT_UNOBSERVED", "{refused}");
}

/// The change record of `id` as this repository holds it.
fn change_record(test_box: &TestBox, repo: &Path, id: &str) -> serde_json::Value {
    let record = test_box.git_ok(
        repo,
        &[
            "cat-file",
            "blob",
            &format!("refs/bay/changes/{id}:change.json"),
        ],
    );
    serde_json::from_str(&record).unwrap()
}

/// A probed payload `name`, its session and cwd rewritten to `session` and `cwd`.
fn payload(name: &str, session: &str, cwd: &Path) -> serde_json::Value {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code-2.1.302")
        .join(name);
    let mut payload: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fixture).unwrap()).unwrap();
    payload["session_id"] = session.into();
    payload["cwd"] = cwd.display().to_string().into();
    payload
}

/// `bay hook <event>` with `payload` on stdin; returns its exit.
fn hook_event(test_box: &TestBox, cwd: &Path, event: &str, payload: &serde_json::Value) -> i32 {
    use std::io::Write as _;
    let mut child = test_box
        .bay_process(cwd)
        .args(["hook", event])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    out.status.code().unwrap_or(-1)
}

/// A change's versions: its recorded `observed_versions` and `validated_candidate`, and the
/// required versions `bay status` derives (its judged inputs and invalidations).
fn versions(test_box: &TestBox, repo: &Path, id: &str) -> serde_json::Value {
    let record = change_record(test_box, repo, id);
    let shown = change_status(test_box, repo, id);
    serde_json::json!({
        "observed_versions": record["observed_versions"],
        "validated_candidate": record["validated_candidate"],
        "shown_validated_candidate": shown["validated_candidate"],
        "inputs": shown["inputs"],
        "invalidations": shown["invalidations"],
    })
}

#[test]
fn a_notice_changes_none_of_the_versions() {
    let (test_box, repo, _remote) = trunk(None);
    let (y, view_y) = start(&test_box, &repo, "CARD-Y", &["src/"]);
    std::fs::write(view_y.join("src/feature.rs"), "fn feature() {}\n").unwrap();
    // X moves the contract, migrates its consumer, and lands through bay.
    let started = test_box.bay_ok(
        &repo,
        &["start", "CARD-X", "--paths", "api/", "src/client.rs"],
    );
    let x = started["change"].as_str().unwrap().to_owned();
    let view_x = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view_x.join("api/version"), "v18\n").unwrap();
    std::fs::write(view_x.join("src/client.rs"), "uses v18\n").unwrap();
    let (code, landed) = land(&test_box, &view_x);
    assert_eq!(code, 0, "{landed}");

    let before = versions(&test_box, &repo, &y);
    assert_eq!(before["observed_versions"][INPUT], "v17", "{before}");
    assert_eq!(
        before["invalidations"][0]["reason"], "INPUT_UNOBSERVED",
        "{before}"
    );
    assert_eq!(before["invalidations"][0]["required"], "v18", "{before}");

    // The notice in Y's view: `bay watch` names X's landing and Y's invalidation.
    let watched = test_box.bay_ok(&view_y, &["watch"]);
    let landings = watched["detail"]["landings"].as_array().unwrap();
    assert!(
        landings
            .iter()
            .any(|landing| landing["change"] == x.as_str()),
        "{watched}"
    );
    assert_eq!(
        watched["detail"]["invalidations"][0]["reason"], "INPUT_UNOBSERVED",
        "{watched}"
    );
    assert_eq!(versions(&test_box, &repo, &y), before, "after bay watch");

    // The same notice reaching Y's session through its hooks.
    for (event, name) in [
        ("user-prompt-submit", "02-UserPromptSubmit.json"),
        ("post-tool", "08-PostToolBatch.json"),
        ("post-tool", "04-PostToolUse.json"),
    ] {
        let code = hook_event(&test_box, &view_y, event, &payload(name, "s-y", &view_y));
        assert_eq!(code, 0, "{event} {name}");
        assert_eq!(
            versions(&test_box, &repo, &y),
            before,
            "after {event} {name}"
        );
    }
    let (code, refused) = land(&test_box, &view_y);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "INPUT_UNOBSERVED", "{refused}");

    // Only `start --resume` moves what Y observed.
    let resumed = test_box.bay_ok(&repo, &["start", "--resume", &y]);
    assert_eq!(resumed["detail"]["observed_versions"][INPUT], "v18");
    let after = versions(&test_box, &repo, &y);
    assert_eq!(after["observed_versions"][INPUT], "v18", "{after}");
}

#[test]
fn a_read_from_a_stale_view_never_counts_as_observed() {
    let (test_box, repo, _remote) = trunk(None);
    let (id, view) = start(&test_box, &repo, "CARD-1", &["src/"]);
    std::fs::write(view.join("src/feature.rs"), "fn feature() {}\n").unwrap();
    move_trunk(&test_box, &repo, &[("api/version", "v18\n")]);
    test_box.bay_ok(&repo, &["status", &id]);
    let before = versions(&test_box, &repo, &id);
    assert_eq!(before["observed_versions"][INPUT], "v17", "{before}");

    // The session in the view, whose base is behind trunk, reads the provider's file there: it
    // sees the old version, and nothing read there is what trunk requires.
    let target = view.join("api/version");
    let mut pre = payload("03-PreToolUse.json", "s-r", &view);
    pre["tool_name"] = "Read".into();
    pre["tool_input"] = serde_json::json!({ "file_path": target });
    let mut post = payload("04-PostToolUse.json", "s-r", &view);
    post["tool_name"] = "Read".into();
    post["tool_input"] = serde_json::json!({ "file_path": target });
    post["tool_response"] = serde_json::json!({
        "type": "text",
        "file": {"filePath": target, "content": "v17\n"},
    });
    let mut batch = payload("08-PostToolBatch.json", "s-r", &view);
    batch["tool_calls"] = serde_json::json!([{
        "tool_name": "Read",
        "tool_input": {"file_path": target},
        "tool_use_id": "toolu_read",
        "tool_response": "v17\n",
    }]);
    for (event, payload) in [
        ("pre-tool", &pre),
        ("post-tool", &post),
        ("post-tool", &batch),
    ] {
        assert_eq!(hook_event(&test_box, &view, event, payload), 0, "{event}");
        assert_eq!(versions(&test_box, &repo, &id), before, "after {event}");
    }
    // The reads bay itself serves in that view observe nothing either.
    test_box.bay_ok(&view, &["status"]);
    test_box.bay_ok(&view, &["watch"]);
    assert_eq!(
        versions(&test_box, &repo, &id),
        before,
        "after status and watch"
    );

    let (code, refused) = land(&test_box, &view);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "INPUT_UNOBSERVED", "{refused}");
    assert_eq!(refused["detail"]["observed"], "v17", "{refused}");
    // `start --resume` moves the view to trunk, and only then is v18 observed.
    let resumed = test_box.bay_ok(&repo, &["start", "--resume", &id]);
    assert_eq!(resumed["detail"]["observed_versions"][INPUT], "v18");
    let (_, after) = land(&test_box, &view);
    assert_ne!(after["reason"], "INPUT_UNOBSERVED", "{after}");
}
