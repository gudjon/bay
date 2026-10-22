//! G198. A concurrent create race has one winner.
//!
//! Where bay creates a ref that must not exist yet, it says so: an intent claim and its change
//! are pushed with a lease that expects no ref, so of several starts racing for one intent
//! generation exactly one creates the claim, and every other one is refused with a stated reason
//! that names the winner (`CHANGE_SUPERSEDED`), never a failure and never a second claim. A
//! loser leaves no change behind, on the remote or in its own store. The race runs from separate
//! clones in the test's object and ref formats (`BAY_TEST_FORMATS`), so it holds on files and on
//! reftable.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

/// How many starts race for one intent generation.
const RACERS: usize = 6;

const INTENT: &str = "CARD-198";

/// A clone of `remote` at `<root>/<name>` in the ref format of [`support::formats`] (the object
/// format follows the remote).
fn clone_in_format(test_box: &TestBox, remote: &Path, name: &str) -> PathBuf {
    let (_, ref_format) = support::formats();
    let dir = test_box.path().join(name);
    test_box.git_ok(
        test_box.path(),
        &[
            "clone",
            "-q",
            &format!("--ref-format={ref_format}"),
            remote.to_str().unwrap(),
            dir.to_str().unwrap(),
        ],
    );
    dir
}

/// The names of the refs under `prefix`, in the repository the git options `args` name.
fn ref_names(test_box: &TestBox, args: &[&str], prefix: &str) -> BTreeSet<String> {
    let mut all: Vec<&str> = args.to_vec();
    all.extend(["for-each-ref", "--format=%(refname)", prefix]);
    test_box
        .git_ok(test_box.path(), &all)
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn racing_starts_for_one_generation_make_one_claim_and_refuse_the_rest() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    // Generation 1 lands, so `start --new` races for generation 2: a loser is refused at once,
    // with no takeover window to wait out.
    let (code, first) = result_json(
        test_box
            .bay(&repo)
            .env("BAY_ACTOR", "agent-0")
            .args(["start", INTENT, "--paths", "src/"]),
    );
    assert_eq!(code, 0, "{first}");
    let view = PathBuf::from(first["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/one.rs"), "fn one() {}\n").unwrap();
    let (code, landed) = result_json(
        test_box
            .bay(&view)
            .env("BAY_ACTOR", "agent-0")
            .args(["merge", "--wait"]),
    );
    assert_eq!(code, 0, "{landed}");
    let remote_dir = format!("--git-dir={}", remote.display());
    let changes_before = ref_names(&test_box, &[remote_dir.as_str()], "refs/bay/changes/");

    // Each racer is its own clone, as on its own machine, so only the remote decides.
    let clones: Vec<PathBuf> = (1..=RACERS)
        .map(|n| clone_in_format(&test_box, &remote, &format!("racer-{n}")))
        .collect();
    let children: Vec<std::process::Child> = clones
        .iter()
        .enumerate()
        .map(|(n, clone)| {
            test_box
                .bay_process(clone)
                .env("BAY_ACTOR", format!("agent-{}", n + 1))
                .args(["--json", "start", INTENT, "--new", "--paths", "src/"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let results: Vec<(Option<i32>, serde_json::Value)> = children
        .into_iter()
        .map(|child| {
            let out = child.wait_with_output().unwrap();
            let json: serde_json::Value =
                serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
                    panic!(
                        "a racer printed no result: {}",
                        String::from_utf8_lossy(&out.stderr)
                    )
                });
            (out.status.code(), json)
        })
        .collect();

    // Exactly one winner.
    let winners: Vec<&serde_json::Value> = results
        .iter()
        .filter(|(_, result)| result["decision"] == "ok")
        .map(|(_, result)| result)
        .collect();
    assert_eq!(winners.len(), 1, "{results:#?}");
    let winner = winners[0]["change"].as_str().unwrap();
    assert_ne!(winner, first["change"].as_str().unwrap());

    // Every other racer has a clear refusal naming the winner: never a failure.
    for (code, result) in results
        .iter()
        .filter(|(_, result)| result["decision"] != "ok")
    {
        assert_eq!(*code, Some(10), "{result}");
        assert_eq!(result["decision"], "blocked", "{result}");
        assert_eq!(result["reason"], "CHANGE_SUPERSEDED", "{result}");
        assert_eq!(result["change"], winner, "{result}");
    }

    // The remote holds one claim per generation, and one new change: the winner's.
    let claims = ref_names(&test_box, &[remote_dir.as_str()], "refs/bay/intent/");
    assert_eq!(claims.len(), 2, "{claims:?}");
    assert!(
        claims.iter().any(|claim| claim.ends_with("/1")),
        "{claims:?}"
    );
    assert!(
        claims.iter().any(|claim| claim.ends_with("/2")),
        "{claims:?}"
    );
    let changes_after = ref_names(&test_box, &[remote_dir.as_str()], "refs/bay/changes/");
    let created: Vec<&String> = changes_after.difference(&changes_before).collect();
    assert_eq!(created, [&format!("refs/bay/changes/{winner}")]);

    // A loser keeps no change of its own: what its store holds is what the remote holds.
    for clone in &clones {
        let local_dir = format!("--git-dir={}", clone.join(".git").display());
        let local = ref_names(&test_box, &[local_dir.as_str()], "refs/bay/changes/");
        assert!(
            local.is_subset(&changes_after),
            "{} kept a change the remote never took: {local:?}",
            clone.display()
        );
    }
}
