//! G410 (a fleet finding, 2026-10-27: the runners' GitHub credential stopped working; a lazy
//! fetch of a request object failed, and healthy changes were refused for it). A request this
//! runner cannot read is its own fault, said and retried: the change is never refused for it,
//! keeps its place, and merges in its turn once the object can be read.
//!
//! Guard to mutate: `src/publish.rs` `verify_request_digest`: a read error is a refusal.

#![cfg(unix)]

use crate::support;

use support::queued::{Check, host};
use support::result_json;

const CONFIG: &str = "[runner]\nlease_s = 30\nheartbeat_s = 1\n";

#[test]
fn a_request_this_runner_cannot_read_is_a_fault_and_the_place_stays() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let test_box = &held.test_box;
    test_box.git_bare(&held.remote, &["config", "uploadpack.allowFilter", "true"]);
    let id = held.queue("one", "src");
    // The runner's clone knows the change's commits and trees, not its blobs; then its
    // promisor is gone, as a remote whose credential stopped working leaves it.
    let executor = test_box.path().join("exec");
    test_box.git_ok(
        test_box.path(),
        &[
            "clone",
            "-q",
            "--filter=blob:none",
            &format!("file://{}", held.remote.display()),
            executor.to_str().unwrap(),
        ],
    );
    test_box.use_keys(&executor, &held.keys);
    let fetched = test_box
        .git(&executor)
        .env("GIT_NO_LAZY_FETCH", "1")
        .args(["fetch", "-q", "origin", "+refs/bay/*:refs/known/*"])
        .output()
        .unwrap();
    assert!(fetched.status.success(), "{fetched:?}");
    // Every blob arrives but the request's, so only the request cannot be read.
    let record = test_box.git_ok(
        &executor,
        &[
            "cat-file",
            "-p",
            &format!("refs/known/changes/{id}:change.json"),
        ],
    );
    let record: serde_json::Value = serde_json::from_str(&record).unwrap();
    let requests: Vec<String> = record["operations"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|op| op["request_id"].as_str())
        .map(|request| {
            test_box
                .git_ok(
                    &executor,
                    &["rev-parse", &format!("{request}:request.json")],
                )
                .trim()
                .to_owned()
        })
        .collect();
    assert!(!requests.is_empty(), "{record}");
    let missing = test_box.git_ok(
        &executor,
        &["rev-list", "--objects", "--all", "--missing=print"],
    );
    let wanted: String = missing
        .lines()
        .filter_map(|line| line.strip_prefix('?'))
        .filter(|oid| !requests.iter().any(|request| request == oid))
        .flat_map(|oid| [oid, "\n"])
        .collect();
    let mut fill = test_box
        .git(&executor)
        .args(["cat-file", "--batch-check"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::io::Write::write_all(fill.stdin.as_mut().unwrap(), wanted.as_bytes()).unwrap();
    assert!(fill.wait().unwrap().success());
    let gone = test_box.path().join("promisor-gone.git");
    for (key, value) in [
        ("remote.origin.promisor", "false"),
        ("remote.lost.url", gone.to_str().unwrap()),
        ("remote.lost.promisor", "true"),
        ("extensions.partialClone", "lost"),
    ] {
        test_box.git_ok(&executor, &["config", key, value]);
    }
    let (_, pass) = result_json(test_box.bay(&executor).args(["runner", "run", "--once"]));
    let released = test_box.git_bare(
        &held.remote,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/bay/queue-done/",
        ],
    );
    assert!(
        released.trim().is_empty(),
        "a place was released for a runner's fault: {released} {pass}"
    );
    assert!(
        !pass.to_string().contains("is refused"),
        "a change was refused for a runner's fault: {pass}"
    );
    assert_eq!(
        test_box.trunk_change_ids(&held.remote),
        Vec::<String>::new(),
        "{pass}"
    );
    // With the object readable again, the change merges in its turn.
    let healthy = held.executor("healthy", support::PRODUCER);
    let merged = held.pass(&healthy);
    assert_eq!(
        test_box.trunk_change_ids(&held.remote),
        vec![id],
        "{merged}"
    );
}
