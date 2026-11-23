//! G878 (a fleet ask, 2026-11-23: a host that deploys main refuses a commit whose author it does
//! not know, and every commit bay makes on main was written by `bay runner`). `[runner] author`
//! in `.bay/config.toml` names the author of the commits bay makes on main. The committer stays
//! bay's, since a signed landing is checked by its committer, and a draft a person wrote keeps
//! its own author.
//!
//! Guard to mutate: the `merge_author` arm in `commit_candidate` (`src/publish.rs`): drop it and
//! main's commit is written by `bay runner`; take it before the draft's own author and a
//! person's branch lands under the configured name.

use std::path::PathBuf;

use crate::support;

use support::{TestBox, result_json, unchecked_files};

const AUTHOR: &str = "Deploy Bot <deploy@example.com>";

fn trunk() -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = unchecked_files();
    files.push((
        ".bay/config.toml".to_owned(),
        format!("[runner]\nauthor = \"{AUTHOR}\"\n"),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    (test_box, repo, remote)
}

#[test]
fn a_configured_author_writes_bays_commit_and_bay_stays_the_committer() {
    let (test_box, repo, remote) = trunk();
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    let author = test_box.git_bare(&remote, &["log", "-1", "--format=%an <%ae>", "main"]);
    assert_eq!(
        author, AUTHOR,
        "main's commit is not written by the configured author"
    );
    let committer = test_box.git_bare(&remote, &["log", "-1", "--format=%cn <%ce>", "main"]);
    assert_eq!(
        committer, "bay runner <executor@bay.invalid>",
        "the committer stays bay's"
    );
}

#[test]
fn a_persons_draft_keeps_its_own_author_under_a_configured_one() {
    let (test_box, repo, remote) = trunk();
    test_box.git_ok(&repo, &["switch", "-q", "-c", "outside"]);
    std::fs::write(repo.join("src/x.rs"), "fn x() {}\n").unwrap();
    test_box.git_ok(&repo, &["add", "--all"]);
    test_box.git_ok(
        &repo,
        &[
            "commit",
            "-q",
            "--author=Ada Writer <ada@example.org>",
            "-m",
            "Add x",
        ],
    );
    test_box.git_ok(&repo, &["push", "-q", "origin", "outside"]);
    test_box.git_ok(&repo, &["switch", "-q", "main"]);
    let started = test_box.bay_ok(&repo, &["start", "CARD-outside", "--from", "outside"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let author = test_box.git_bare(&remote, &["log", "-1", "--format=%an <%ae>", "main"]);
    assert_eq!(author, "Ada Writer <ada@example.org>");
}
