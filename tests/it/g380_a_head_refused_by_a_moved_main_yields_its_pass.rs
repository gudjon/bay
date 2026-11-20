//! G380 (a fleet finding, 2026-10-25: on a main that others pushed to every few minutes, one
//! queue head rebuilt twelve times in one runner pass, and no other change was given a queue
//! place for an hour). A head refused by a moved main yields its pass.
//!
//! The runner's head, refused because main moved, gives its claim back and keeps its place,
//! so the pass goes on to every other change, which is given its place. The next pass runs the
//! head again, and with `[runner] batch` the changes behind it land in the same push.
//!
//! Guards to mutate, in `src/publish.rs`: `after_trunk_moved`'s `yield_head` arm (the head
//! rebuilds in place: the second case fails); `batch`'s `Err` arm that yields (a batch refused
//! by a moved main is a fault, and its members land one by one: the first case fails).

// Each case runs shell scripts (adapters, the relay), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use support::{TestBox, check_files, passing_result, result_json};

/// A repository whose main moves just before each of the runner's first three merging pushes,
/// `batch` set or not, two changes submitted for the runner, and the runner's passes until both
/// land (at most eight). Returns the box, the remote, the changes and every pass's `detail.ran`.
fn moving_main(batch: bool) -> (TestBox, PathBuf, Vec<String>, Vec<serde_json::Value>) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let other = test_box.path().join("other");
    let count = test_box.path().join("pushes");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str("\n[policy]\nreuse_unsigned = true\n");
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        format!(
            "[runner]\nlease_s = 30\nheartbeat_s = 1\n{}",
            if batch { "batch = { max = 2 }\n" } else { "" }
        ),
    ));
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("docs/b.md", "b\n")],
        &files,
    );
    test_box.clone_of(&remote, "other");

    // Two changes, each submitted for the runner and given no place yet.
    let mut ids = Vec::new();
    for n in 0..2 {
        let started = test_box.bay_ok(&repo, &["start", &format!("CARD-{n}"), "--paths", "src/"]);
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(format!("src/f{n}.rs")), "fn f() {}\n").unwrap();
        let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
        assert_eq!(code, 0, "{queued}");
        ids.push(started["change"].as_str().unwrap().to_owned());
    }
    let runner = test_box.clone_of(&remote, "runner");
    // Main moves from another clone in the runner's `pre-push` hook, on each of its first three
    // pushes to main: after the check answered, so the runner never sees the move while it
    // checks (G796), and the merging push is refused by a moved main.
    let hook = runner.join(".git/hooks/pre-push");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\n\
             grep -q ' refs/heads/main ' || exit 0\n\
             echo push >> '{count}'; n=$(wc -l < '{count}' | tr -d ' ')\n\
             [ \"$n\" -le 3 ] || exit 0\n\
             unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_PREFIX\n\
             ( cd '{other}' && git pull -q --rebase && echo \"$n\" > docs/move-$n.md && git add docs && git -c user.name=t -c user.email=t@t commit -q -m \"move $n\" && git push -q origin main ) >/dev/null 2>&1\n\
             exit 0\n",
            count = count.display(),
            other = other.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let mut passes = Vec::new();
    for _ in 0..8 {
        let (code, ran) = result_json(test_box.bay(&runner).args(["runner", "run", "--once"]));
        assert_eq!(code, 0, "{ran}");
        passes.push(ran["detail"]["ran"].clone());
        let landed = test_box.trunk_change_ids(&remote);
        if ids.iter().all(|id| landed.contains(id)) {
            break;
        }
    }
    (test_box, remote, ids, passes)
}

/// Whether a pass ran an operation that yielded.
fn yielded(passes: &[serde_json::Value]) -> bool {
    passes
        .iter()
        .filter_map(serde_json::Value::as_array)
        .flatten()
        .any(|run| run["detail"]["yielded"] == true)
}

#[test]
fn a_head_on_a_moving_main_lets_the_queue_in_and_lands_in_one_batch() {
    let (test_box, remote, ids, passes) = moving_main(true);
    assert!(yielded(&passes), "{passes:?}");
    let landed = test_box.trunk_change_ids(&remote);
    assert!(ids.iter().all(|id| landed.contains(id)), "{landed:?}");
    let batched = test_box.git_bare(
        &remote,
        &[
            "log",
            "-2",
            "--format=%(trailers:key=Bay-Batch,valueonly)",
            "main",
        ],
    );
    assert_eq!(
        batched.lines().filter(|line| !line.is_empty()).count(),
        2,
        "the two changes did not land in one batch: {batched:?}"
    );
}

#[test]
fn without_a_batch_the_head_still_gives_the_pass_back() {
    let (test_box, remote, ids, passes) = moving_main(false);
    assert!(yielded(&passes), "the head rebuilt in place: {passes:?}");
    let landed = test_box.trunk_change_ids(&remote);
    assert!(ids.iter().all(|id| landed.contains(id)), "{landed:?}");
}

/// A fleet finding (2026-10-26): a yielded head waited twenty minutes, nobody holding it, while
/// every runner pre-checked changes behind it until its pass ended. A head nobody holds is run
/// again between a runner's operations, in the same pass.
///
/// Guard to mutate, in `src/executor.rs`: the unheld-head arm of `abandoned_head` (remove it).
#[test]
fn a_yielded_head_runs_again_within_the_same_pass() {
    let (_test_box, _remote, _ids, passes) = moving_main(false);
    let again = passes
        .iter()
        .filter_map(serde_json::Value::as_array)
        .any(|ran| {
            ran.iter().enumerate().any(|(at, run)| {
                run["detail"]["yielded"] == true
                    && ran[at + 1..]
                        .iter()
                        .any(|later| later["op_id"] == run["op_id"])
            }) || ran.iter().enumerate().any(|(at, run)| {
                run["op_id"].is_string()
                    && ran[at + 1..].iter().any(|later| {
                        later["op_id"] == run["op_id"] && later["detail"]["yielded"] == true
                    })
            })
        });
    assert!(
        again,
        "a head nobody held waited for the next pass: {passes:?}"
    );
}

/// G553 (a fleet finding, 2026-10-29: twelve changes were released "after a fault" two to seven
/// times each in 90 minutes, and their records did not say why). A batch member given back when
/// main moved under the batch says so in its change's record.
///
/// Guard to mutate, in `src/publish.rs`: the `cause` in `release_after_fault`'s message.
#[test]
fn a_batch_member_given_back_says_why() {
    let (test_box, remote, ids, _passes) = moving_main(true);
    let released: Vec<String> = ids
        .iter()
        .flat_map(|id| {
            test_box
                .git_bare(
                    &remote,
                    &["log", "--format=%s", &format!("refs/bay/changes/{id}")],
                )
                .lines()
                .filter(|line| line.contains("after a fault"))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(
        !released.is_empty(),
        "no member was given back, so this case tests nothing"
    );
    assert!(
        released
            .iter()
            .all(|line| line.ends_with("after a fault: main moved under the batch")),
        "{released:?}"
    );
}
