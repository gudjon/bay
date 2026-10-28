//! G437. `bay status <change>` says a merged change is on main.
//!
//! A status read of one change says that change's state in one sentence (0.5.26). The sentence
//! for a merged change matched the printed word `merged`, while the status row carries bay's own
//! word, `landed`, until printing; so a merged change read "Done; nothing else is needed."
//! instead of "<intent> (<id>) is on main." (found reading the code, 2026-10-28).
//!
//! The guard to mutate: the `landed` arm in `change_say` (`src/result.rs`); match `merged`
//! again and the sentence is never said.

use crate::support;

use support::{TestBox, result_json, unchecked_files};

#[test]
fn a_status_of_a_merged_change_says_it_is_on_main() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let say = status["say"].as_str().unwrap_or_default();
    assert!(
        say.ends_with(&format!("({id}) is on main.")),
        "a merged change is not said to be on main: {say}"
    );
}
