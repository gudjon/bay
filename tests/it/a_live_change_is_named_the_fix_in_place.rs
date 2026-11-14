//! BACKLOG 234 (a fleet finding, 2026-11-03: bnaq3w46 waited 26 minutes on main's red on its
//! paths while it carried that red's repair; `bay start bnaq3w46 --fixes scripts-fast` was
//! refused, "--after, --fixes, --lane and --reverts open a new change", so the repair moved to a
//! new change at the back of the line, and the approver's waiver did not move with it).
//! `bay start <change> --fixes <change|check>` names a live change the repair in place: the fix
//! is judged as an opening start judges it (G161), written into the change's record, and read
//! from there by every later pass and by the merge, which carries `Fixes:` and answers the red.
//!
//! Guards to mutate: `src/start.rs` `names_change` opening a new change for `--fixes` (the start
//! is refused, as before); `resume` writing no fix (the merge carries no `Fixes:`, and main stays
//! red); `resume` judging no fix (a name main's policy has no post check by is written).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::talk::fresh_key;
use support::{TestBox, check_files, passing_result, result_json};

const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

/// A trunk whose post check `suite` runs elsewhere, as producer `suite-1`, and failed on the
/// landing of change `broke`: main is red on `src/`. The box, the repository, the remote and
/// `broke`.
fn red_trunk() -> (TestBox, PathBuf, PathBuf, String) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(POST);
        }
    }
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let suite_box = test_box.clone_of(&remote, "suite-box");
    let key = fresh_key(&test_box, "suite-1");
    test_box.git_ok(&suite_box, &["config", "bay.producerName", "suite-1"]);
    test_box.git_ok(
        &suite_box,
        &["config", "bay.producerKey", key.to_str().unwrap()],
    );
    let (broke, view) = start_with(&test_box, &repo, "CARD-234-BROKE", "src/broke.rs");
    let landing = merged(&test_box, &view)["detail"]["commit"]
        .as_str()
        .unwrap()
        .to_owned();
    let (code, written) = result_json(test_box.bay(&suite_box).args([
        "evidence",
        "record",
        "--kind",
        "check",
        "--check",
        "suite",
        "--environment",
        "local",
        "--verdict",
        "product_failure",
        "--commit",
        &landing,
    ]));
    assert_eq!(code, 0, "{written}");
    let red = red(&test_box, &repo);
    assert_eq!(red[0]["change"], json!(broke), "{red}");
    (test_box, repo, remote, broke)
}

/// Starts `card` on `src/` and writes `file` in its view: the change and its view.
fn start_with(test_box: &TestBox, repo: &Path, card: &str, file: &str) -> (String, PathBuf) {
    let started = test_box.bay_ok(repo, &["start", card, "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(file), format!("// {card}\n")).unwrap();
    (id, view)
}

fn merged(test_box: &TestBox, view: &Path) -> Value {
    let (code, landed) = result_json(test_box.bay(view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    landed
}

fn red(test_box: &TestBox, repo: &Path) -> Value {
    test_box.bay_ok(repo, &["status"])["detail"]["red"].clone()
}

fn changes(test_box: &TestBox, remote: &Path) -> String {
    test_box.git_bare(
        remote,
        &["for-each-ref", "--format=%(refname)", "refs/bay/changes/"],
    )
}

#[test]
fn a_live_change_named_the_fix_of_a_red_merges_under_its_own_id_and_answers_it() {
    let (test_box, repo, remote, _) = red_trunk();
    // GIVEN a live change on the red's paths that turns out to be its repair
    let (id, view) = start_with(&test_box, &repo, "CARD-234", "src/fix.rs");
    let before = changes(&test_box, &remote);

    // WHEN it is named the fix of the red check
    let named = test_box.bay_ok(&repo, &["start", &id, "--fixes", "suite"]);

    // THEN it is the same change in the same view, and the remote holds no new change
    assert_eq!(named["change"], json!(id), "{named}");
    assert_eq!(named["detail"]["fix_added"], json!("suite"), "{named}");
    assert_eq!(
        named["detail"]["view"],
        json!(view.display().to_string()),
        "{named}"
    );
    assert_eq!(changes(&test_box, &remote), before);
    // AND its merge carries the fix and answers the red.
    let landed = merged(&test_box, &view);
    assert_eq!(landed["change"], json!(id), "{landed}");
    let commit = landed["detail"]["commit"].as_str().unwrap();
    let message = test_box.git_bare(&remote, &["log", "-1", "--format=%B", commit]);
    assert!(message.contains("Fixes: suite"), "{message}");
    assert_eq!(red(&test_box, &repo), json!([]));
}

#[test]
fn a_fix_main_cannot_name_is_refused_and_nothing_is_written() {
    let (test_box, repo, remote, broke) = red_trunk();
    let (id, _) = start_with(&test_box, &repo, "CARD-234-NONE", "src/fix.rs");
    let chain = format!("refs/bay/changes/{id}");
    let head = test_box.git_bare(&remote, &["rev-parse", &chain]);

    // WHEN the change is named the fix of a name main's policy has no post check by
    let (code, refused) =
        result_json(
            test_box
                .bay(&repo)
                .args(["start", &id, "--fixes", "no-such-check"]),
        );

    // THEN the start is refused, naming the post checks main has, and the chain is as it was
    assert_ne!(code, 0, "{refused}");
    let why = refused.to_string();
    assert!(
        why.contains("no post check by that name") && why.contains("suite"),
        "{refused}"
    );
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", &chain]), head);

    // AND a change named the fix of one thing is not named the fix of another.
    test_box.bay_ok(&repo, &["start", &id, "--fixes", &broke]);
    let (code, refused) = result_json(test_box.bay(&repo).args(["start", &id, "--fixes", "suite"]));
    assert_ne!(code, 0, "{refused}");
    assert!(
        refused
            .to_string()
            .contains(&format!("already fixes {broke}")),
        "{refused}"
    );
}
