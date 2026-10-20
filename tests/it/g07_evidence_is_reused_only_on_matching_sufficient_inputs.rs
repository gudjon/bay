//! G7. Evidence is reused only on matching, sufficient inputs (`DESIGN` § 7 Reuse and Tree
//! equality; `INTERFACES` § 6, § 7; ruling R18, BACKLOG 28).
//!
//! With `[policy] evidence_reuse = "exact-inputs"` on a trunk with a producer registry, an
//! operation reuses another operation's producer-signed record, from any producer, when it is
//! about the same tree on the same base under the same policy, fixtures and (for a check that
//! reads the commit) commit, and runs nothing for that check. Anything else runs again: reuse
//! off, no registry (an unsigned record is never evidence), another base, another commit for a
//! commit-keyed check, another fixture digest, a request that carries named inputs. A refusal that rests on a reused failure names
//! it. An input bay cannot identify makes the policy invalid, so nothing runs and nothing is
//! reused. Each adapter run appends its request to a marker file.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{
    PRODUCER, TestBox, TestKeys, check_files, passing_result, result_json, write_executable,
};

const REUSE_ON: &str = "[policy]\nevidence_reuse = \"exact-inputs\"\n";
/// The second producer the registry enrolls, under the test keys' rogue key.
const OTHER_PRODUCER: &str = "ci-2";

/// A named input whose version is the contents of `api/version`; `src/` consumes it.
const INPUT: &str = "contract:api";
const PROVIDER: &str = "#!/bin/sh\ncat >/dev/null\nv=$(cat api/version) || exit 1\nprintf '{\"version\":\"%s\",\"consumers\":[{\"path\":\"src/\",\"kind\":\"executable\"}]}' \"$v\"\n";
/// The `[[input]]` entry for [`INPUT`].
const INPUT_ENTRY: &str = "\n[[input]]\nname = \"contract:api\"\nprovider = \"scripts/api-version\"\nadapter = [\"scripts/api-version\"]\npath = [\"/usr/bin\", \"/bin\"]\n";

fn fixture(digit: char) -> String {
    format!("data@sha256:{}", digit.to_string().repeat(64))
}

/// A policy with one check `tests`, its `fixtures` and an `inputs` line when given.
fn policy(revision: u32, fixtures: &[String], inputs: &str) -> String {
    let fixtures: Vec<String> = fixtures.iter().map(|f| format!("\"{f}\"")).collect();
    format!(
        "revision = {revision}\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nfixtures = [{}]\n{inputs}",
        fixtures.join(", ")
    )
}

/// A trunk and an agent's clone of it.
struct Trunk {
    test_box: TestBox,
    remote: PathBuf,
    repo: PathBuf,
    /// The adapter appends each request it gets here.
    runs: PathBuf,
    /// Present when trunk is a bay host with a registry that enrolls two producers.
    keys: Option<TestKeys>,
    /// Whether the policy names [`INPUT`], which each change then reads.
    named_input: bool,
}

/// A trunk whose check gives `verdict` under `config` and `policy`: a bay host with a
/// registry when `signed`, a plain remote without one otherwise.
fn trunk(config: &str, policy: &str, verdict: &str, signed: bool) -> Trunk {
    let test_box = TestBox::new();
    let (repo, remote) = if signed {
        let host = test_box.path().join("host.git");
        test_box.host_init(&host);
        let repo = test_box.repo_without_remote_any("repo");
        test_box.add_origin(&repo, &host);
        (repo, host)
    } else {
        let repo = test_box.repo_any("repo");
        (repo, test_box.path().join("repo-remote.git"))
    };
    let runs = test_box.path().join("runs");
    let result = passing_result(1).replace("\\\"pass\\\"", &format!("\\\"{verdict}\\\""));
    let mut files = check_files(
        &test_box.relay(&format!("printf '%s\\n' \"$req\" >> '{}'", runs.display())),
        &result,
        1,
        60,
    );
    files.retain(|(path, _)| path != ".bay/policy.toml");
    files.push((".bay/policy.toml".to_owned(), policy.to_owned()));
    files.push((".bay/config.toml".to_owned(), config.to_owned()));
    std::fs::create_dir_all(repo.join("scripts")).unwrap();
    write_executable(&repo.join("scripts/api-version"), PROVIDER);
    // Trunk is seeded before the registry exists: from then on only executors publish.
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("api/version", "v1\n")],
        &files,
    );
    let keys = signed.then(|| {
        let keys = test_box.keys();
        test_box.enroll_classes(
            &repo,
            &keys,
            &keys.executor,
            &[],
            &[(PRODUCER, &keys.producer), (OTHER_PRODUCER, &keys.rogue)],
        );
        test_box.use_keys(&repo, &keys);
        keys
    });
    Trunk {
        test_box,
        remote,
        repo,
        runs,
        keys,
        named_input: policy.contains("[[input]]"),
    }
}

impl Trunk {
    /// The requests the adapter got, one per run.
    fn requests(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(&self.runs)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// Starts a change that may edit `src/` and `.bay/`, and writes `src/new.rs` in its view.
    fn start(&self, intent: &str) -> PathBuf {
        let mut args = vec!["start", intent, "--paths", "src/", ".bay/"];
        if self.named_input {
            args.extend(["--input", INPUT]);
        }
        let started = self.test_box.bay_ok(&self.repo, &args);
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
        view
    }

    fn land_wait(&self, view: &Path) -> (i32, serde_json::Value) {
        result_json(self.test_box.bay(view).args(["land", "--wait"]))
    }

    /// Submits the change in `view` without running it.
    fn submit(&self, view: &Path) {
        let (code, submitted) = result_json(self.test_box.bay(view).arg("land"));
        assert_eq!(code, 0, "{submitted}");
    }

    /// Makes the executor in `dir` write evidence as `producer`, under that producer's key
    /// when trunk has a registry.
    fn produce_as(&self, dir: &Path, producer: &str) {
        if let Some(keys) = &self.keys {
            self.test_box.use_keys(dir, keys);
            let key = if producer == PRODUCER {
                &keys.producer
            } else {
                &keys.rogue
            };
            self.test_box
                .git_ok(dir, &["config", "bay.producerKey", key.to_str().unwrap()]);
        }
        self.test_box
            .git_ok(dir, &["config", "bay.producerName", producer]);
    }

    /// Executor E1, a clone writing evidence as `producer`, checks the submitted change, pushes
    /// its evidence and dies before it publishes.
    fn checked_elsewhere(&self, producer: &str) {
        let e1 = self.test_box.clone_of(&self.remote, "e1");
        self.produce_as(&e1, producer);
        let died = self
            .test_box
            .bay_process(&e1)
            .env("BAY_TEST_KILL_AT", "before-publish")
            .args(["--json", "executor", "run", "--once"])
            .output()
            .unwrap();
        assert_eq!(died.status.code(), Some(137), "E1 died before it published");
        assert!(!self.requests().is_empty(), "E1 checked the change");
        assert!(
            self.test_box.trunk_change_ids(&self.remote).is_empty(),
            "nothing landed yet"
        );
    }

    fn trunk_tree(&self) -> String {
        self.test_box
            .git_bare(&self.remote, &["rev-parse", "main^{tree}"])
    }
}

/// Two changes make the same edit on the same base, so their candidates share a tree and a
/// base and differ in commit. Executor E1 (one producer) checks the first, pushes its evidence
/// and dies before it publishes; the agent's own box, another producer, then lands the
/// second. Returns the trunk and the number of adapter runs the second landing added.
fn reuse_across_producers(config: &str, policy: &str, signed: bool) -> (Trunk, usize) {
    let trunk = trunk(config, policy, "pass", signed);
    let first = trunk.start("CARD-1");
    let second = trunk.start("CARD-2");
    trunk.submit(&first);
    trunk.checked_elsewhere(if signed { PRODUCER } else { "e1-checks" });
    let checked = trunk.requests().len();

    let producer = if signed { OTHER_PRODUCER } else { "e2-checks" };
    trunk.produce_as(&trunk.repo, producer);
    let (code, landed) = trunk.land_wait(&second);
    assert_eq!(code, 0, "the second change landed: {landed}");
    let tree = trunk.trunk_tree();
    assert_eq!(
        trunk.requests()[0]["candidate"]["tree"].as_str(),
        Some(tree.as_str()),
        "the first change's candidate had the landed tree"
    );
    let added = trunk.requests().len() - checked;
    (trunk, added)
}

#[test]
fn another_producers_record_on_the_same_tree_and_base_is_reused() {
    let (trunk, added) = reuse_across_producers(REUSE_ON, &policy(1, &[fixture('a')], ""), true);
    assert_eq!(added, 0, "reused, not rechecked: {:?}", trunk.requests());
}

#[test]
fn with_reuse_off_the_same_tree_and_base_is_rechecked() {
    let (trunk, added) = reuse_across_producers("", &policy(1, &[fixture('a')], ""), true);
    // Seen once under a loaded run as 2; the requests say which candidate ran twice.
    assert_eq!(added, 1, "rechecked, not reused: {:?}", trunk.requests());
}

/// `DESIGN` § 7: an unsigned record never counts as evidence, so without a producer registry
/// nothing stands for a run.
#[test]
fn without_a_producer_registry_nothing_is_reused() {
    let (_, added) = reuse_across_producers(REUSE_ON, &policy(1, &[fixture('a')], ""), false);
    assert_eq!(added, 1, "rechecked, not reused");
}

/// A record does not hold the versions of the named inputs a request carries, so it cannot
/// show they matched: a change that reads a named input is rechecked.
#[test]
fn a_change_with_named_inputs_is_rechecked() {
    let (trunk, added) = reuse_across_producers(
        REUSE_ON,
        &format!("{}{INPUT_ENTRY}", policy(1, &[fixture('a')], "")),
        true,
    );
    assert_eq!(added, 1, "rechecked, not reused");
    let ran = trunk.requests();
    assert_eq!(ran[1]["inputs"][0], "contract:api@v1", "{ran:?}");
}

/// Tree equality is an index, not proof: a check that reads the commit is keyed by it, and
/// the two candidates differ in commit.
#[test]
fn a_commit_keyed_check_on_another_commit_with_the_same_tree_is_rechecked() {
    let (trunk, added) = reuse_across_producers(
        REUSE_ON,
        &policy(1, &[fixture('a')], "inputs = [\"commit\"]\n"),
        true,
    );
    assert_eq!(added, 1, "rechecked for its own commit");
    let ran = trunk.requests();
    assert_eq!(ran[0]["candidate"]["tree"], ran[1]["candidate"]["tree"]);
    assert_ne!(
        ran[0]["candidate"]["commit"], ran[1]["candidate"]["commit"],
        "two commits"
    );
}

/// A change that only moves the fixture digest is checked twice on one tree: by the base's
/// policy, then by its own. The own run has another definition digest, so the base run's record
/// on the same tree does not answer it. A second change on that tree and base then reuses each
/// policy's own record.
#[test]
fn the_same_tree_under_another_fixture_digest_runs_again() {
    let moved = policy(2, &[fixture('b')], "");
    let trunk = trunk(REUSE_ON, &policy(1, &[fixture('a')], ""), "pass", true);
    let first = trunk.start("CARD-1");
    let second = trunk.start("CARD-2");
    for view in [&first, &second] {
        std::fs::write(view.join(".bay/policy.toml"), &moved).unwrap();
    }
    trunk.submit(&first);
    trunk.checked_elsewhere(PRODUCER);
    let ran = trunk.requests();
    assert_eq!(
        ran.len(),
        2,
        "one run per fixture digest on one tree: {ran:?}"
    );
    assert_eq!(ran[0]["requires"]["fixtures"][0], fixture('a').as_str());
    assert_eq!(ran[1]["requires"]["fixtures"][0], fixture('b').as_str());
    let tree = ran[0]["candidate"]["tree"].as_str().unwrap().to_owned();
    assert_eq!(ran[1]["candidate"]["tree"], tree.as_str(), "the same tree");
    let notes = trunk.test_box.git_bare(
        &trunk.remote,
        &[
            "notes",
            &format!("--ref=refs/notes/bay/evidence/{PRODUCER}"),
            "show",
            &tree,
        ],
    );
    let records: Vec<serde_json::Value> = serde_json::from_str(&notes).unwrap();
    let fixtures: Vec<&serde_json::Value> = records
        .iter()
        .map(|record| &record["inputs"]["fixtures"][0])
        .collect();
    assert_eq!(
        fixtures,
        [fixture('a').as_str(), fixture('b').as_str()],
        "each record names its fixtures: {notes}"
    );

    trunk.produce_as(&trunk.repo, OTHER_PRODUCER);
    let (code, landed) = trunk.land_wait(&second);
    assert_eq!(code, 0, "{landed}");
    assert_eq!(trunk.trunk_tree(), tree, "{landed}");
    assert_eq!(trunk.requests().len(), 2, "each policy's record is reused");
}

/// The same tree on another base: the adapter was given other `changed_paths`, so the first
/// candidate's record does not stand for the second (`INTERFACES` § 3). Two changes make the
/// same two edits; executor E1 checks the first and dies before it publishes. A third change
/// then lands one of the two edits, so the second change's candidate has the first one's tree
/// on a base with another tree. (A tree already on trunk would end `no_effect`, G213.)
#[test]
fn the_same_tree_on_another_base_is_rechecked() {
    let trunk = trunk(REUSE_ON, &policy(1, &[fixture('a')], ""), "pass", true);
    let first = trunk.start("CARD-1");
    let second = trunk.start("CARD-2");
    for view in [&first, &second] {
        std::fs::write(view.join("src/b.rs"), "fn b() {}\n").unwrap();
    }
    trunk.submit(&first);
    trunk.checked_elsewhere(PRODUCER);
    assert_eq!(trunk.requests().len(), 1);
    let part = trunk.start("CARD-3");
    std::fs::remove_file(part.join("src/new.rs")).unwrap();
    std::fs::write(part.join("src/b.rs"), "fn b() {}\n").unwrap();
    trunk.produce_as(&trunk.repo, OTHER_PRODUCER);
    let (code, landed) = trunk.land_wait(&part);
    assert_eq!(code, 0, "{landed}");
    let (code, landed) = trunk.land_wait(&second);
    assert_eq!(code, 0, "{landed}");
    let ran = trunk.requests();
    assert_eq!(ran.len(), 3, "rechecked on its own base: {ran:?}");
    assert_eq!(ran[0]["candidate"]["tree"], ran[2]["candidate"]["tree"]);
    assert_ne!(ran[0]["changed_paths"], ran[2]["changed_paths"]);
}

/// A refusal that rests on a reused `product_failure` says what to repair (§ 8), as one that
/// ran the check does.
#[test]
fn a_refusal_on_a_reused_failure_names_the_check() {
    for (config, runs) in [(REUSE_ON, 1), ("", 2)] {
        let trunk = trunk(
            config,
            &policy(1, &[fixture('a')], ""),
            "product_failure",
            true,
        );
        let first = trunk.start("CARD-1");
        let second = trunk.start("CARD-2");
        for view in [&first, &second] {
            let (code, blocked) = trunk.land_wait(view);
            assert_eq!(code, 10, "{blocked}");
            assert_eq!(blocked["reason"], "CHECK_FAILED", "{blocked}");
            let checks = blocked["detail"]["checks"].as_array().unwrap();
            assert_eq!(checks.len(), 1, "{blocked}");
            assert_eq!(checks[0]["check"], "tests", "{blocked}");
            assert_eq!(checks[0]["verdict"], "product_failure", "{blocked}");
        }
        assert_eq!(trunk.requests().len(), runs, "{config:?}");
    }
}

/// BACKLOG 28: an input bay cannot identify is never reused and never a pass. The policy is
/// invalid, so the check never runs.
#[test]
fn an_input_bay_cannot_identify_runs_nothing_and_reuses_nothing() {
    for config in ["", REUSE_ON] {
        let trunk = trunk(
            config,
            &policy(1, &[], "inputs = [\"network\"]\n"),
            "pass",
            false,
        );
        let view = trunk.start("CARD-1");
        let (code, blocked) = trunk.land_wait(&view);
        assert_eq!(code, 10, "{blocked}");
        assert_eq!(blocked["reason"], "POLICY_INVALID", "{blocked}");
        assert!(trunk.requests().is_empty(), "nothing ran");
        assert!(
            trunk.test_box.trunk_change_ids(&trunk.remote).is_empty(),
            "nothing landed"
        );
    }
}
