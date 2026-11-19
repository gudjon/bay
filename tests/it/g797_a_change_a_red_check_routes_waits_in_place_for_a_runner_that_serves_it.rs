//! G797 (a fleet measurement, 2026-11-19: while main was red on a post check, runners that did
//! not serve its environment claimed three changes on its paths, found no worker, and released
//! their places; each rejoined the back of the line, and one waited 17 minutes for its next
//! claim). While main is red on a post check, a change on its paths runs that check before its
//! merge (G677), so a runner must serve the check's environment to take the change. A runner
//! that does not leaves it unclaimed and in its place to a live runner that does, which takes
//! it at once, as it would leave a change whose own checks it cannot run.
//!
//! Guard to mutate, in `src/publish.rs`: `served_elsewhere` reading the policy without the red
//! checks (the runner that does not serve the red check claims the change, finds no worker and
//! releases its place).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::queued::{Check, Held, Trust, host_under, ran_for};
use support::{PRODUCER, check_files, passing_result, result_json};

/// The post check `web` on `web/**`, in an environment of its own.
const POST: &str = "\n[[check]]\nid = \"web\"\nenvironment = \"post-env\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"web/**\"]\n";

/// Post check `web` answers `verdict` on main's tip, as its own box records it (G244).
fn answer(held: &Held, broad: &Path, verdict: &str) {
    let test_box = &held.test_box;
    test_box.git_ok(broad, &["fetch", "-q", "origin"]);
    let tip = test_box.git_bare(&held.remote, &["rev-parse", "main"]);
    let (code, written) = result_json(test_box.bay(broad).args([
        "evidence",
        "record",
        "--kind",
        "check",
        "--check",
        "web",
        "--environment",
        "post-env",
        "--verdict",
        verdict,
        "--version",
        "web",
        "--commit",
        tip.trim(),
    ]));
    assert_eq!(code, 0, "{written}");
}

/// A runner's clone named `name`, which declares `environments`.
fn runner(held: &Held, name: &str, environments: &[&str]) -> PathBuf {
    let clone = held.executor(name, PRODUCER);
    held.test_box
        .git_ok(&clone, &["config", "bay.runnerName", name]);
    held.test_box.git_ok(
        &clone,
        &["config", "bay.producerName", &format!("{name}-checks")],
    );
    for environment in environments {
        held.test_box.git_ok(
            &clone,
            &["config", "--add", "bay.checkEnvironment", environment],
        );
    }
    clone
}

/// The subjects of change `id`'s chain on the host, oldest first.
fn chain(held: &Held, id: &str) -> Vec<String> {
    held.test_box
        .git_bare(
            &held.remote,
            &[
                "log",
                "--reverse",
                "--format=%s",
                &format!("refs/bay/changes/{id}"),
            ],
        )
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_change_a_red_check_routes_waits_in_place_for_a_runner_that_serves_it() {
    // A pre-merge check in `local` on src/, and the post check `web` in `post-env` on web/.
    let policy = check_files("x", &passing_result(1), 1, 120)
        .into_iter()
        .find(|(path, _)| path == ".bay/policy.toml")
        .map(|(_, text)| {
            let text = text.replacen("timeout_s", "when_paths = [\"src/**\"]\ntimeout_s", 1);
            format!("{text}{POST}")
        })
        .unwrap();
    let held = host_under(
        &Check {
            config: "[runner]\nlease_s = 30\nheartbeat_s = 1\n",
            policy: Some(&policy),
            seed: &[
                ("src/a.rs", "fn a() {}\n"),
                ("web/a.ts", "a\n"),
                ("docs/a.md", "a\n"),
            ],
            hold_first: false,
            fails: "false",
        },
        // Unsigned, so the post records the web box writes count.
        Trust::Unsigned,
    );
    // Runner A serves `local` only; runner B serves `local` and `post-env`.
    let a = runner(&held, "exec-a", &["local"]);
    let b = runner(&held, "exec-b", &["local", "post-env"]);
    let broad = held.test_box.clone_of(&held.remote, "web-box");
    let key = support::talk::fresh_key(&held.test_box, "web-1");
    held.test_box
        .git_ok(&broad, &["config", "bay.producerName", "web-1"]);
    held.test_box.git_ok(
        &broad,
        &["config", "bay.producerKey", key.to_str().unwrap()],
    );

    // GIVEN `broke`, on web/, turned `web` red after it passed on the landing before
    let base = held.queue_with("base", &["docs"], &[("docs/base.md", "base\n")]);
    let pass = held.pass(&a);
    assert!(
        held.test_box.trunk_change_ids(&held.remote).contains(&base),
        "{pass}"
    );
    answer(&held, &broad, "pass");
    let broke = held.queue_with("broke", &["web"], &[("web/broke.ts", "broke\n")]);
    let pass = held.pass(&a);
    assert!(
        held.test_box
            .trunk_change_ids(&held.remote)
            .contains(&broke),
        "{pass}"
    );
    answer(&held, &broad, "product_failure");

    // AND runner B is live: its presence names `post-env`
    let pass = held.pass(&b);
    assert!(
        pass["detail"]["presence"]["pushed"] == true,
        "runner B published no presence: {pass}"
    );

    // WHEN a change on src/ and web/, which runs `x` and the red `web` before its merge,
    // reaches runner A first
    let on_web = held.queue_with(
        "on-web",
        &["src", "web"],
        &[("src/more.rs", "fn more() {}\n"), ("web/more.ts", "more\n")],
    );
    let pass = held.pass(&a);

    // THEN runner A leaves it in its place, unclaimed, for a runner that serves `post-env`
    assert_eq!(
        ran_for(&pass, &on_web)["reason"],
        "CHECK_UNAVAILABLE",
        "{pass}"
    );
    let written = chain(&held, &on_web);
    assert!(
        !written
            .iter()
            .any(|line| line.starts_with("claim ") || line.starts_with("unavailable ")),
        "runner A took a change whose red check it cannot run: {written:?}"
    );
    let released = held.test_box.git_bare(
        &held.remote,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/bay/queue-done/",
        ],
    );
    let places = held.test_box.git_bare(
        &held.remote,
        &["for-each-ref", "--format=%(refname)", "refs/bay/queue/"],
    );
    let place = places
        .lines()
        .find(|name| {
            held.test_box
                .git_bare(&held.remote, &["show", &format!("{name}:entry.json")])
                .contains(on_web.as_str())
        })
        .and_then(|name| name.rsplit('/').next())
        .unwrap_or_else(|| panic!("no place names {on_web}: {places}"));
    assert!(
        !released
            .lines()
            .any(|line| line.ends_with(&format!("/{place}"))),
        "the change lost its place: {released}"
    );

    // AND runner B takes it on its next pass, and it lands
    let pass = held.pass(&b);
    assert_eq!(ran_for(&pass, &on_web)["decision"], "merged", "{pass}");
    let written = chain(&held, &on_web);
    let claims: Vec<&String> = written
        .iter()
        .filter(|line| line.starts_with("claim "))
        .collect();
    assert_eq!(claims.len(), 1, "{written:?}");
    assert!(claims[0].contains("by exec-b"), "{written:?}");
}
