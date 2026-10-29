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

/// A blob-less clone runs a history check whose `when_paths` names `path` by `glob`, and the
/// check reads the first version of `path`, which the clone never held.
fn reads_the_first_version(glob: &str, path: &str) {
    let test_box = TestBox::new();
    let seed = test_box.repo_any("seed");
    let remote = test_box.path().join("seed-remote.git");
    test_box.git_bare(&remote, &["config", "uploadpack.allowFilter", "true"]);
    let script = READ_EVERY_VERSION.replace("cfg/v.json", path);
    let mut files = check_files(&script, &passing_result(1), 1, 60);
    for (name, text) in &mut files {
        if name == ".bay/policy.toml" {
            text.push_str("inputs = [\"history\"]\nwhen_paths = [\"");
            text.push_str(glob);
            text.push_str("\", \"src/\"]\n");
        }
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
    let holds = |object: &str| {
        test_box
            .git(&cache)
            .env("GIT_NO_LAZY_FETCH", "1")
            .args(["cat-file", "-e", object])
            .output()
            .unwrap()
            .status
            .success()
    };
    let started = test_box.bay_ok(&cache, &["start", "CARD-1", "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    assert!(
        !holds(&first),
        "the clone already holds the first version of {path}"
    );

    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert!(
        holds(&first),
        "the first version of {path} was never fetched"
    );
}
