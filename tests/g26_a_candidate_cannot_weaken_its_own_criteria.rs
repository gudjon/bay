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

mod support;

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
    // The candidate drops the failing check and raises the revision: the base policy still judges it.
    write(&view, ".bay/policy.toml", "revision = 2\n");
    let (code, blocked) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{blocked}");
    assert_eq!(blocked["reason"], "CHECK_FAILED");
}

#[test]
fn a_policy_edit_that_keeps_the_old_revision_is_refused() {
    let (test_box, _repo, remote, _id, view) = change(true);
    let trunk = test_box.git_bare(&remote, &["rev-parse", "main"]);
    write(&view, ".bay/policy.toml", &policy(1, 0));
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
    // No reason code covers an invalid policy edit (INTERFACES § 2), so it is a refused
    // operation with a receipt.
    assert_eq!(code, 20, "{refused}");
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
fn an_adapter_that_reads_an_unlisted_file_is_an_infra_failure() {
    if !(cfg!(target_os = "macos")
        || std::process::Command::new("bwrap")
            .args(["--dev-bind", "/", "/", "true"])
            .output()
            .is_ok_and(|out| out.status.success()))
    {
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
    let before = format!(
        "echo run >> '{runs}'; n=$(wc -l < '{runs}' | tr -d ' '); \
         if [ \"$n\" -le 2 ]; then ( cd '{other}' && git pull -q --rebase && \
         sed \"s/^revision = .*/revision = $((n + 1))/\" .bay/policy.toml > policy.new && \
         mv policy.new .bay/policy.toml && git add .bay/policy.toml && \
         git -c user.name=t -c user.email=t@t commit -q -m \"policy revision $((n + 1))\" && \
         git push -q origin main ); fi",
        runs = runs.display(),
        other = other.display(),
    );
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
    let before = format!(
        "echo run >> '{runs}'; n=$(wc -l < '{runs}' | tr -d ' '); \
         if [ \"$n\" -le 3 ]; then ( cd '{other}' && git pull -q --rebase && \
         if [ \"$n\" -le 2 ]; then echo \"$n\" > docs/move-$n.md && git add docs; \
         else sed 's/^revision = .*/revision = 2/' .bay/policy.toml > policy.new && \
         mv policy.new .bay/policy.toml && git add .bay/policy.toml; fi && \
         git -c user.name=t -c user.email=t@t commit -q -m \"move $n\" && \
         git push -q origin main ); fi",
        runs = runs.display(),
        other = other.display(),
    );
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
