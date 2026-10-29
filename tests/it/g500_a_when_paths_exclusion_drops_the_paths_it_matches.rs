//! G500 (B1, the limits list of 2026-10-29 § 7). A `when_paths` glob that starts with `!`
//! drops the paths it matches.
//!
//! A glob could only include, so a policy that wanted "every `AGENTS.md` but those under
//! `kanban/`" listed one `AGENTS.md` glob per top-level folder (49 in oz-platform's policy 38),
//! and an `AGENTS.md` in a new top-level folder matched none and skipped its pre-merge lints.
//! Now `when_paths = ["**/AGENTS.md", "!kanban/**"]` says it, read in order as `.gitignore`
//! reads it: the last glob that matches a path decides, so a later include takes a path back.
//! A list that starts with an exclusion is `POLICY_INVALID` (an exclusion drops only what a
//! glob before it takes, so a list of exclusions alone matches nothing), and so is a `!` in a
//! check's `paths`, which a footprint stamp reads as `when_paths` are. A hold needs an
//! approval whose list keeps `.bay/policy.toml` (G368).
//!
//! The guards to mutate, in `src/policy.rs`: the `Some(excluded) => matched &&
//! !glob_matches(excluded, path)` arm of `globs_match` (to `matched || glob_matches(glob,
//! path)`: the excluded file wakes the check; to `false`: the new folder's file wakes nothing),
//! the leading-exclusion refusal and the `strip_prefix('!')` part rule in
//! `validate_when_paths`, the `!` refusal in `validate_materialization`, `CheckEntry::wakes`
//! (back to any glob), and the hold rule in `validate_shape` (back to any glob: an approval
//! whose list drops the policy file is taken as one over it).

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

/// Every `AGENTS.md` outside `kanban/`.
const AGENTS: &str = "when_paths = [\"**/AGENTS.md\", \"!kanban/**\"]\n";

/// One `[[check]]` entry on the shared adapter, with `extra` lines.
fn entry(id: &str, extra: &str) -> String {
    format!(
        "\n[[check]]\nid = \"{id}\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check {id}\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n{extra}"
    )
}

/// The policy: an `agents` check with `agents` as its lines, and a `src` check on `src/`.
fn policy(agents: &str) -> String {
    format!(
        "revision = 1\n{}{}",
        entry("agents", agents),
        entry("src", "when_paths = [\"src/**\"]\n")
    )
}

/// A trunk whose `agents` check fails any candidate with an `AGENTS.md` that says `broken`.
fn trunk(agents: &str) -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let script = check_files(
        &format!(
            "[ \"$1\" = agents ] && grep -rqs --include=AGENTS.md broken . && {{ {failing}; exit 0; }}"
        ),
        &passing_result(1),
        1,
        60,
    )
    .into_iter()
    .find(|(path, _)| path == "scripts/bay-check")
    .unwrap()
    .1;
    test_box.seed_with_check(
        &repo,
        &[
            ("src/a.rs", "fn a() {}\n"),
            ("apps/AGENTS.md", "apps\n"),
            ("kanban/AGENTS.md", "kanban\n"),
        ],
        &[
            ("scripts/bay-check".to_owned(), script),
            (".bay/policy.toml".to_owned(), policy(agents)),
        ],
    );
    (test_box, repo, remote)
}

/// Starts a change writing `broken` to `path` and lands it.
fn land_broken(test_box: &TestBox, repo: &Path, path: &str) -> (i32, serde_json::Value) {
    let dir = format!("{}/", Path::new(path).parent().unwrap().display());
    let started = test_box.bay_ok(repo, &["start", path, "--paths", &dir]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let full = view.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, "broken\n").unwrap();
    result_json(test_box.bay(&view).args(["merge", "--wait"]))
}

/// The ids `not_required` names in a land result.
fn not_required(landed: &serde_json::Value) -> Vec<String> {
    landed["detail"]["not_required"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| row["check"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn an_agents_md_under_an_excluded_folder_wakes_no_check_and_says_why() {
    let (test_box, repo, _remote) = trunk(AGENTS);
    // The file says `broken`, and it lands: the agents check did not run.
    let (code, landed) = land_broken(&test_box, &repo, "kanban/AGENTS.md");
    assert_eq!(code, 0, "the exclusion drops kanban/AGENTS.md: {landed}");
    assert!(
        not_required(&landed).contains(&"agents".to_owned()),
        "{landed}"
    );
    let row = landed["detail"]["not_required"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["check"] == "agents")
        .unwrap();
    let reason = row["reason"].as_str().unwrap();
    assert!(
        reason.contains("when_paths") && reason.contains("!kanban/**"),
        "{reason}"
    );
}

#[test]
fn an_agents_md_in_a_new_top_level_folder_still_wakes_the_check() {
    let (test_box, repo, remote) = trunk(AGENTS);
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);
    // No glob names `fresh/`: `**/AGENTS.md` takes it, and no exclusion drops it.
    let (code, refused) = land_broken(&test_box, &repo, "fresh/AGENTS.md");
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    assert_eq!(
        refused["detail"]["checks"][0]["check"], "agents",
        "{refused}"
    );
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), tip);
}

#[test]
fn a_later_include_takes_a_path_back() {
    let (test_box, repo, remote) =
        trunk("when_paths = [\"**/AGENTS.md\", \"!kanban/**\", \"kanban/AGENTS.md\"]\n");
    // The last glob that matches decides: `kanban/AGENTS.md` is taken back.
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let (code, refused) = land_broken(&test_box, &repo, "kanban/AGENTS.md");
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), tip);
    // A deeper one stays excluded.
    let (code, landed) = land_broken(&test_box, &repo, "kanban/team/AGENTS.md");
    assert_eq!(code, 0, "{landed}");
    assert!(
        not_required(&landed).contains(&"agents".to_owned()),
        "{landed}"
    );
}

#[test]
fn a_list_that_starts_with_an_exclusion_makes_the_policy_invalid() {
    for (bad, says) in [
        ("when_paths = [\"!kanban/**\"]\n", "exclusion"),
        ("when_paths = [\"!kanban/**\", \"!docs/**\"]\n", "exclusion"),
        (
            "when_paths = [\"!kanban/**\", \"**/AGENTS.md\"]\n",
            "exclusion",
        ),
        ("when_paths = [\"**/AGENTS.md\", \"!\"]\n", "when_paths"),
        ("when_paths = [\"**/AGENTS.md\", \"!/\"]\n", "when_paths"),
        (
            "when_paths = [\"**/AGENTS.md\", \"!../up/\"]\n",
            "when_paths",
        ),
        (
            "when_paths = [\"**/AGENTS.md\", \"!kanban/**\", \"!kanban/**\"]\n",
            "twice",
        ),
        (
            "when_paths = [\"**/AGENTS.md\"]\npaths = [\"apps/\", \"!kanban/\"]\n",
            "exclusions",
        ),
    ] {
        let (test_box, repo, _remote) = trunk(bad);
        let (code, refused) = land_broken(&test_box, &repo, "apps/AGENTS.md");
        assert_eq!(code, 10, "{bad}: {refused}");
        assert_eq!(refused["reason"], "POLICY_INVALID", "{bad}: {refused}");
        let why = refused["detail"]["why"].as_str().unwrap_or_default();
        assert!(why.contains(says), "{bad}: {refused}");
    }
}

#[test]
fn an_exclusion_that_matches_nothing_is_allowed() {
    let (test_box, repo, _remote) = trunk("when_paths = [\"**/AGENTS.md\", \"!nowhere/**\"]\n");
    let (code, refused) = land_broken(&test_box, &repo, "kanban/AGENTS.md");
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
}

#[test]
fn a_hold_needs_an_approval_whose_list_keeps_the_policy_file() {
    // The approval covers `.bay/` but drops the policy file: a change that lifts the hold
    // would need no person's yes, so the policy is refused.
    let (test_box, repo, _remote) = trunk(
        "when_paths = [\"**/AGENTS.md\"]\n\n[[check]]\nid = \"policy-approval\"\nenvironment = \"people\"\nkind = \"approval\"\nwhen_paths = [\".bay/\", \"!.bay/policy.toml\"]\n\n[assets]\nhold = [\"matters/\"]\n",
    );
    let (code, refused) = land_broken(&test_box, &repo, "apps/AGENTS.md");
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "POLICY_INVALID", "{refused}");
    assert!(
        refused
            .to_string()
            .contains("only a person's signed yes lifts a hold"),
        "{refused}"
    );
}
