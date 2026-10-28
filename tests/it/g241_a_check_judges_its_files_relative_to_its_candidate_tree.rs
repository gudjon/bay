//! G241. A check judges its files relative to its candidate tree, and the tree's path holds no
//! `.git`.
//!
//! bay materializes the candidate at `<scratch root>/<candidate id>/check-<n>/candidate/`, the
//! scratch root being the repository's own in bay's state directory (`check::scratch_root`), and
//! the check starts with that tree as its working directory. Neither its absolute path nor a path
//! inside the tree holds `/.git`: a tool may leave out every path that does (TypeScript keeps no
//! symlink there, so a pnpm workspace could not name its own packages' types: `TS2742`, 105 units
//! of a fleet's `world-fast`, 2026-10-28, while bay made trees under `<common dir>/bay/check/`).
//! An adapter that leaves out files by their path parts still judges them relative to its
//! working directory, and a clean candidate passes.
//!
//! The guards to mutate: the scratch of a head run (`publish::run_policy`) and of post work
//! (`bisect::run_at`), each from `check::scratch_root`.

use crate::support;

use std::path::PathBuf;

use support::{TestBox, check_files, passing_result, result_json};

/// The adapter's judgement before its result: it runs in the documented tree, whose path holds
/// no `/.git` (the test TypeScript applies), it sees the candidate's files there, and not one of
/// the paths relative to it holds a `.git` part.
const RELATIVE_JUDGE: &str = r#"here=$(pwd -P)
case "$here" in
  */.git*) echo "the check runs under a path that holds /.git: $here" >&2; exit 3 ;;
  */.local/state/bay/check/*/*/check-0/candidate) ;;
  *) echo "the check runs outside its documented tree: $here" >&2; exit 3 ;;
esac
[ -f src/a.rs ] || { echo "the working directory is not the candidate" >&2; exit 3; }
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
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert_eq!(test_box.trunk_change_ids(&remote).len(), 1);
}

/// A post check `suite` that runs the same judgement, on the paths under `src/`.
const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

/// The check's judgement of where it runs: `bad=yes` under a path that holds `/.git`. A post
/// check that only exited would be `unknown`, which bay never counts as red, so this one
/// answers a product failure there.
const WHERE_JUDGE: &str = r#"case "$(pwd -P)" in */.git*) bad=yes ;; *) bad=no ;; esac"#;

#[test]
fn a_post_check_runs_in_a_tree_whose_path_holds_no_git() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let pass = passing_result(1);
    let fail = pass.replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    assert_ne!(pass, fail, "the failing result must differ: {fail}");
    let result = format!("if [ \"$bad\" = no ]; then {pass}; else {fail}; fi");
    let mut files = check_files(WHERE_JUDGE, &result, 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(POST);
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\npost = true\nbisect_post = true\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let submit = |name: &str| -> String {
        let started = test_box.bay_ok(&repo, &["start", name, "--paths", "src"]);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(format!("src/{name}.rs")), "fn x() {}\n").unwrap();
        let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
        assert_eq!(code, 0, "{merged}");
        id
    };
    submit("one");
    // The post work answers `suite` on main's tip; a tree under `/.git` answers it red.
    let ran = test_box.bay_ok(&repo, &["runner", "run", "--once"]);
    // A red post check on `src/` would refuse the next change on it (MAIN_RED).
    let two = submit("two");
    assert!(
        test_box.trunk_change_ids(&remote).contains(&two),
        "a change after the post work did not land, so main was red: {ran}"
    );
}
