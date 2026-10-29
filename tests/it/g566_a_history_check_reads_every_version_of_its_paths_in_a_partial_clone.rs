//! G566. A history check reads every version of its paths in a partial clone.
//!
//! A check that declares `inputs = ["history"]` reads Git through a repository of its own run,
//! which borrows the store's objects and has no promisor remote. In a blob-less clone the
//! store lacks every blob it never checked out, so such a check read an older version of a
//! file as absent. Before the check runs, bay fetches from the promisor remote every object of
//! the candidate's history under the check's `when_paths` that the store lacks, and the check
//! reads each version.
//!
//! The guard to mutate: in `src/check.rs`, `prepare` calls
//! `crate::materialize::prefetch_history` for a check that reads history (drop the call, and the
//! check reads the first version as absent and fails); and `missing_history` there lists the
//! versions by the change walk (with a `rev-list --objects` walk, a wildcard directory part
//! names no blob, and the second test reads its first version as absent).
//!
//! A fleet finding, 2026-10-29: a post run (and a bisect step) runs its check on any landing, so
//! `bay::bisect` `alone` sets the check's `when_paths` aside, and the prefetch, which read
//! `when_paths`, fetched nothing: the post check `zero-edits` read an older `venue.json` as a bad
//! object. The guard to mutate: in `src/policy.rs`, `CheckEntry::history_globs` returns only
//! `when_paths` (drop `.or(self.history_paths.as_deref())`), and the third test's post check
//! reads its first version as absent.

use crate::support;

use std::path::PathBuf;

use support::{TestBox, check_files, passing_result, result_json};

/// What the check does before it passes: through `git.dir`, it reads `cfg/v.json` (the path
/// a test names instead) at every commit of the candidate's history that changes it, and fails
/// when one cannot be read or fewer than the two seeded versions are found.
const READ_EVERY_VERSION: &str = r#"gitdir=$(printf '%s' "$req" | sed -n 's/.*"git":{"dir":"\([^"]*\)".*/\1/p')
[ -n "$gitdir" ] || exit 7
read=0
for c in $(GIT_DIR="$gitdir" git rev-list "$commit" -- cfg/v.json); do
  GIT_DIR="$gitdir" GIT_NO_LAZY_FETCH=1 git cat-file -p "$c:cfg/v.json" >/dev/null 2>&1 || exit 7
  read=$((read + 1))
done
[ "$read" -ge 2 ] || exit 8"#;

#[test]
fn a_blobless_history_check_reads_an_older_version_of_its_paths() {
    reads_the_first_version("cfg/*.json", "cfg/v.json");
}

/// A fleet finding, 2026-10-29: a `rev-list --objects` walk with a `:(glob)` pathspec whose
/// wildcard is a directory part lists only the trees of its literal prefix, so it named no blob
/// to fetch, and the check read every older version as absent.
#[test]
fn a_wildcard_directory_part_reads_an_older_version_too() {
    reads_the_first_version("cfg/*/v.json", "cfg/eight/v.json");
}

#[test]
fn a_post_history_check_reads_an_older_version_too() {
    let (test_box, cache, holds_first) =
        landed_in_a_blobless_clone("cfg/*/v.json", "cfg/eight/v.json", Stage::Post);
    assert!(
        !holds_first(&test_box),
        "the pre check, which reads no history, fetched the first version"
    );
    let ran = test_box.bay_ok(&cache, &["runner", "run", "--once"]);
    let text = ran.to_string();
    assert!(
        text.contains("\"check\":\"history\"") && text.contains("\"verdict\":\"pass\""),
        "the post check did not pass: {ran}"
    );
    assert!(
        holds_first(&test_box),
        "the first version was never fetched for the post check: {ran}"
    );
}

/// Where the history check runs: before the merge, or on main after it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Pre,
    Post,
}

/// A blob-less clone runs a history check whose `when_paths` names `path` by `glob`, and the
/// check reads the first version of `path`, which the clone never held.
fn reads_the_first_version(glob: &str, path: &str) {
    let (test_box, _, holds_first) = landed_in_a_blobless_clone(glob, path, Stage::Pre);
    assert!(
        holds_first(&test_box),
        "the first version of {path} was never fetched"
    );
}

/// Whether the clone holds the first version of the path, without fetching it.
type HoldsFirst = Box<dyn Fn(&TestBox) -> bool>;

/// A blob-less clone of a trunk with two versions of `path` lands a change on `src/` through
/// a history check (`stage`) whose `when_paths` names `path` by `glob`: the box, the clone,
/// and whether the clone holds the first version of `path`.
fn landed_in_a_blobless_clone(
    glob: &str,
    path: &str,
    stage: Stage,
) -> (TestBox, PathBuf, HoldsFirst) {
    let test_box = TestBox::new();
    let seed = test_box.repo_any("seed");
    let remote = test_box.path().join("seed-remote.git");
    test_box.git_bare(&remote, &["config", "uploadpack.allowFilter", "true"]);
    let script = READ_EVERY_VERSION.replace("cfg/v.json", path);
    let history = format!("inputs = [\"history\"]\nwhen_paths = [\"{glob}\", \"src/\"]\n");
    let mut files = match stage {
        Stage::Pre => check_files(&script, &passing_result(1), 1, 60),
        // The pre check `tests` reads no history, so the script reads only when given `git`.
        Stage::Post => check_files(
            &format!("if printf '%s' \"$req\" | grep -q '\"git\":{{'; then\n{script}\nfi"),
            &passing_result(1),
            1,
            60,
        ),
    };
    for (name, text) in &mut files {
        if name == ".bay/policy.toml" {
            if stage == Stage::Post {
                text.push_str("\n[[check]]\nid = \"history\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n");
            }
            text.push_str(&history);
        }
    }
    if stage == Stage::Post {
        // The runner runs main's post checks itself.
        files.push((
            ".bay/config.toml".to_owned(),
            "[runner]\npost = true\nbisect_post = false\n".to_owned(),
        ));
    }
    test_box.seed_with_check(
        &seed,
        &[(path, "{\"v\": 1}\n"), ("src/a.rs", "fn a() {}\n")],
        &files,
    );
    let first = test_box.git_ok(&seed, &["rev-parse", &format!("HEAD:{path}")]);
    test_box.seed_trunk(&seed, &[(path, "{\"v\": 2}\n")]);

    let cache = test_box.path().join("cache");
    test_box.git_ok(
        test_box.path(),
        &[
            "clone",
            "-q",
            "--filter=blob:none",
            "--no-checkout",
            &format!("file://{}", remote.display()),
            cache.to_str().unwrap(),
        ],
    );
    let holds = {
        let cache = cache.clone();
        move |test_box: &TestBox, object: &str| {
            test_box
                .git(&cache)
                .env("GIT_NO_LAZY_FETCH", "1")
                .args(["cat-file", "-e", object])
                .output()
                .unwrap()
                .status
                .success()
        }
    };
    let started = test_box.bay_ok(&cache, &["start", "CARD-1", "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    assert!(
        !holds(&test_box, &first),
        "the clone already holds the first version of {path}"
    );

    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    let holds_first: HoldsFirst = Box::new(move |test_box| holds(test_box, &first));
    (test_box, cache, holds_first)
}
