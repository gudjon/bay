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
//! read from local data only. The hook's refusal of a stale write is a later item (B3).

mod support;

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

/// [`trunk`] with the provider `provider` gives for the test box's root, and the check `check`.
fn trunk_with(
    phase: Option<&str>,
    provider: impl Fn(&Path) -> String,
    check: &str,
) -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    std::fs::create_dir_all(repo.join("scripts")).unwrap();
    write_executable(
        &repo.join("scripts/api-version"),
        &provider(test_box.path()),
    );
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
    let provider = |root: &Path| {
        format!(
            "#!/bin/sh\n[ -e '{}' ] && exit 1\n{}",
            root.join("flaky").display(),
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
    let provider = |root: &Path| {
        format!(
            "#!/bin/sh\n: > '{}'\n{}",
            root.join("ran").display(),
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
    let provider = |root: &Path| {
        format!(
            "#!/bin/sh\n: > '{}'\n{}",
            root.join("ran").display(),
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
    let provider = |root: &Path| {
        format!(
            "#!/bin/sh\ncat >/dev/null\nif cat '{}' >/dev/null 2>&1; then v=leaked; else v=$(cat api/version); fi\nprintf '{{\"version\":\"%s\",\"consumers\":[]}}' \"$v\"\n",
            root.join("repo/secret.txt").display()
        )
    };
    let (test_box, repo, _remote) = trunk_with(None, provider, &check_script());
    std::fs::write(repo.join("secret.txt"), "a secret\n").unwrap();
    let (id, _view) = start(&test_box, &repo, "CARD-1", &["src/"]);
    let shown = change_status(&test_box, &repo, &id);
    assert_eq!(shown["inputs"][0]["observed"], "v17", "{shown}");
}

#[test]
fn a_provider_that_leaves_a_child_behind_is_still_bounded() {
    // The provider answers, but a background child keeps its output open far past its timeout.
    let provider = |_: &Path| {
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
