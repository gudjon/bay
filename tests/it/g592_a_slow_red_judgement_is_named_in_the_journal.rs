//! G592 (a fleet finding, 2026-10-30: two heads' `red` steps took 2209 s and 2213 s side by
//! side, where the step's p50 is 3 s, and no record said whether the judgement of main's red
//! took the time). A judgement of main's red that takes `SLOW_RED_S` or more is named in the
//! journal: the change, its seconds, the landings it read and how (in full or from the last
//! judgement), how many other judgements ran beside it, and where Linux counts it, how long its
//! thread ran and waited for a CPU. A faster one, or one served from the cache, says nothing.
//!
//! Guards to mutate: `journaled` (`src/publish.rs`) without its line (no line names the
//! judgement), and `Cost::full` set from `known.is_some()` (`src/red.rs`: a first judgement in
//! a process reads "from the last judgement").

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::TestBox;

/// A pre check `tests` that always passes and a post check `suite` on `src/**`.
const POLICY: &str = "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

/// The adapter: `suite` fails while `src/broken` is there; every other check passes.
const ADAPTER: &str = r#"#!/bin/sh
req=$(cat)
commit=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\1/p')
tree=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\2/p')
verdict=pass
case "$req" in
*'"plan":{"id":"suite"'*) [ -f src/broken ] && verdict=product_failure ;;
esac
printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"selected":1,"completed":1},"checks":[{"id":"t","verdict":"%s"}]}' "$commit" "$tree" "$verdict"
"#;

/// Starts change `name` on the folder of `file`, writes it, and merges it, waiting, with
/// `slow` as the journal's bound when given: its id and the merge's journal. Asserts it lands.
fn lands(
    test_box: &TestBox,
    repo: &Path,
    name: &str,
    file: &str,
    slow: Option<&str>,
) -> (String, String) {
    let folder = format!("{}/", file.split('/').next().unwrap());
    let started = test_box.bay_ok(repo, &["start", name, "--paths", &folder]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let path = view.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("{name}\n")).unwrap();
    let mut merge = test_box.bay(&view);
    merge.args(["--json", "merge", "--wait"]);
    if let Some(slow) = slow {
        merge.env("BAY_TEST_SLOW_RED_S", slow);
    }
    let output = merge.output().unwrap();
    let journal = String::from_utf8_lossy(&output.stderr).into_owned();
    let merged: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("{}\n{journal}", String::from_utf8_lossy(&output.stdout)));
    assert_eq!(
        merged["decision"], "merged",
        "{name} lands: {merged}\n{journal}"
    );
    (started["change"].as_str().unwrap().to_owned(), journal)
}

/// The journal's lines that name a judgement of main's red.
fn judgements(journal: &str) -> Vec<&str> {
    journal
        .lines()
        .filter(|line| line.contains(" red judgement took "))
        .collect()
}

#[test]
fn a_slow_red_judgement_is_named_in_the_journal() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let files = vec![
        ("scripts/bay-check".to_owned(), ADAPTER.to_owned()),
        (".bay/policy.toml".to_owned(), POLICY.to_owned()),
        (
            ".bay/config.toml".to_owned(),
            "[runner]\npost = true\nbisect_post = false\n".to_owned(),
        ),
    ];
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);

    // `suite` passes on `zero`, and `two` turns it red.
    lands(&test_box, &repo, "zero", "src/zero.rs", None);
    let ran = test_box.bay_ok(&repo, &["runner", "run", "--once"]);
    let (_, journal) = lands(&test_box, &repo, "two", "src/broken", None);
    assert_eq!(judgements(&journal), Vec::<&str>::new(), "{ran}");
    test_box.bay_ok(&repo, &["runner", "run", "--once"]);
    let red = test_box.bay_ok(&repo, &["status"])["detail"]["red"].clone();
    assert_eq!(red.as_array().map(Vec::len), Some(1), "suite is red: {red}");

    // A change off the red's paths lands. With a bound of 0 s its judgement is named: the
    // first in its process, so in full.
    let (docs, journal) = lands(&test_box, &repo, "docs", "docs/d.md", Some("0"));
    let named = judgements(&journal);
    assert!(!named.is_empty(), "the judgement is named: {journal}");
    let line = named[0];
    assert!(
        line.starts_with(&format!("bay runner: {docs} red judgement took ")),
        "{line}"
    );
    assert!(line.contains(" landings in full; "), "{line}");
    assert!(line.contains(" other judgements ran beside it"), "{line}");
    if std::path::Path::new("/proc/thread-self/schedstat").exists() {
        assert!(line.contains("s for a CPU"), "{line}");
    }

    // Under the release's bound, a judgement that takes under a minute says nothing.
    let (_, journal) = lands(&test_box, &repo, "more", "docs/e.md", None);
    assert_eq!(judgements(&journal), Vec::<&str>::new(), "{journal}");
}
