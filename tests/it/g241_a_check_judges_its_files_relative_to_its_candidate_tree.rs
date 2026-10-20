//! G241. A check judges its files relative to its candidate tree.
//!
//! bay materializes the candidate at `<git common dir>/bay/check/<candidate id>/check-<n>/candidate/`
//! (`INTERFACES` § 3), and the check starts with that tree as its working directory. The tree's
//! absolute path usually holds a `.git` part: the common dir is `.git` in a plain clone, and no
//! place a repository may live in rules one out. A path inside the tree never holds one, because
//! bay refuses to write a tree path with a `.git` part. So an adapter that leaves out files by
//! their path parts judges them relative to its working directory, and a clean candidate passes.

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

/// The adapter's judgement before its result: it runs in the documented tree, whose absolute
/// path holds a `.git` part, it sees the candidate's files there, and not one of the paths
/// relative to it holds a `.git` part.
const RELATIVE_JUDGE: &str = r#"here=$(pwd -P)
case "$here" in
  */.git/bay/check/*/check-0/candidate) ;;
  *) echo "the check runs outside its documented tree: $here" >&2; exit 3 ;;
esac
[ -f src/a.rs ] && [ -f src/new.rs ] || { echo "the working directory is not the candidate" >&2; exit 3; }
bad=$(find . -type f | sed 's|^\./||' | grep -E '(^|/)\.git(/|$)')
[ -z "$bad" ] || { echo "a path in the tree holds a .git part: $bad" >&2; exit 3; }"#;

#[test]
fn an_adapter_that_leaves_out_git_parts_relative_to_its_tree_passes_a_clean_candidate() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files(RELATIVE_JUDGE, &passing_result(1), 1, 60),
    );
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "landed", "{landed}");
    assert_eq!(test_box.trunk_change_ids(&remote).len(), 1);
}
