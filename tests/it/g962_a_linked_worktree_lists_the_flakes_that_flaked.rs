//! G962 (owner ruling 2026-11-28): a listing read in a linked worktree, not in its store's own
//! checkout, keeps of the flake rows the ones that flaked, and of each promotion target its
//! decision; `detail.trimmed` counts what it left out and names the checkout that lists it whole.
//! The own checkout lists every row, and `flow` stays whole in both.
//!
//! Guards to mutate, in `src/status.rs` `trim_outside_own_checkout` (each makes the case fail):
//! - `if change.is_some() || crate::saved::own_checkout(dir).is_some() {` to `if true {` (a
//!   linked worktree lists every flake row).
//! - the same line to `if change.is_some() {` (the own checkout is trimmed too).
//! - `rows.retain(|row| row["flaky"].as_u64().is_some_and(|flaky| flaky > 0));` to
//!   `rows.retain(|_| true);` (rows that never flaked stay).

// The landing runs a shell check adapter, so it runs on Unix only.
#![cfg(unix)]

use crate::g606_a_status_judges_red_from_the_last_status::land;
use crate::support;

use support::{TestBox, check_files, passing_result};

#[test]
fn a_linked_worktree_lists_the_flakes_that_flaked_and_names_the_checkout_that_lists_all() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let files = check_files("true", &passing_result(1), 1, 60);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    land(&test_box, &repo, "src/b.rs");

    let whole = test_box.bay_ok(&repo, &["status"]);
    let rows = whole["detail"]["flakes"]
        .as_array()
        .unwrap_or_else(|| panic!("no flake rows: {whole}"));
    assert!(
        !rows.is_empty() && rows.iter().all(|row| row["flaky"] == 0),
        "{whole}"
    );
    assert!(whole["detail"].get("trimmed").is_none(), "{whole}");

    let linked = test_box.path().join("linked");
    test_box.git_ok(
        &repo,
        &["worktree", "add", "--detach", linked.to_str().unwrap()],
    );
    let short = test_box.bay_ok(&linked, &["status"]);
    assert_eq!(short["detail"]["flakes"], serde_json::json!([]), "{short}");
    assert_eq!(
        short["detail"]["trimmed"]["flakes_quiet"],
        rows.len(),
        "{short}"
    );
    let own = std::fs::canonicalize(&repo).unwrap();
    assert_eq!(
        short["detail"]["trimmed"]["whole_in"],
        own.display().to_string(),
        "{short}"
    );
    assert!(short["detail"]["flow"].is_object(), "{short}");
}
