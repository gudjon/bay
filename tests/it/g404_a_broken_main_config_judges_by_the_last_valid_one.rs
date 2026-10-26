//! G404 (B2 for the config; a fleet finding, 2026-10-26: `ahead = 32` merged to main by hand
//! refused every bay command on every box for eight minutes, and the runners gave back the
//! queue's places, until a person merged the fix around bay). While the config on main does not
//! load, bay judges by the newest one on main's first-parent line that does: a change still
//! merges, and its result names the config used and why main's does not load.
//!
//! Guard to mutate: `src/config.rs` `Config::at` returning `why` without looking back.

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

const CONFIG: &str = ".bay/config.toml";

#[test]
fn a_change_merges_by_the_last_valid_config_and_says_so() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let files = check_files("true", &passing_result(1), 1, 60);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let outside = test_box.clone_of(&remote, "outside");
    let valid = std::fs::read_to_string(outside.join(CONFIG)).unwrap();
    std::fs::write(outside.join(CONFIG), format!("not_a_bay_key = 1\n{valid}")).unwrap();
    test_box.git_ok(
        &outside,
        &["commit", "-q", "-am", "a config bay cannot load"],
    );
    test_box.git_ok(&outside, &["push", "-q", "origin", "HEAD:main"]);
    let broken = test_box.git_ok(&outside, &["rev-parse", "HEAD"]);

    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    let used = &landed["detail"]["config_used"];
    assert_eq!(used["broken_at"], broken.as_str(), "{landed}");
    assert_ne!(used["used"], broken.as_str(), "{landed}");
    assert!(
        used["why"]
            .as_str()
            .is_some_and(|why| why.contains("not_a_bay_key")),
        "{landed}"
    );
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}

/// B3: a change that edits the config into one bay cannot load is refused before it waits in
/// line, naming why, and main never takes it.
///
/// Guard to mutate: `src/publish.rs` `config_gate` returning `Ok(None)`.
#[test]
fn a_change_that_breaks_the_config_is_refused_before_it_waits() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let files = check_files("true", &passing_result(1), 1, 60);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let main_before = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let started = test_box.bay_ok(&repo, &["start", "CONFIG-1", "--paths", ".bay/"]);
    let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let valid = std::fs::read_to_string(view.join(CONFIG)).unwrap();
    std::fs::write(view.join(CONFIG), format!("not_a_bay_key = 1\n{valid}")).unwrap();

    let (_, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(refused["reason"], "POLICY_INVALID", "{refused}");
    assert!(
        refused["detail"]["why"]
            .as_str()
            .is_some_and(|why| why.contains("not_a_bay_key")),
        "{refused}"
    );
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", "main"]),
        main_before
    );
}
