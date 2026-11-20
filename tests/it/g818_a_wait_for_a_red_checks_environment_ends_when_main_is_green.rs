//! G818 (a fleet finding, 2026-11-20: a change waited for a box serving both its own checks'
//! environment and that of a post check main was red on (G677). Main went green on that check,
//! and the change waited on for hours: a runner judging a recorded wait (G431, G797) counted
//! every post check the change woke, red or not). A recorded wait for a worker keeps only the
//! environments still required: a post check's only while main is red on it. Once main is green
//! on it, a runner that serves the change's own checks takes the change, and it merges.
//!
//! Guard to mutate, in `src/publish.rs`: `required_environments` counting every post check the
//! change wakes, red or not (the change waits on after main is green).

#![cfg(unix)]

use crate::support;

use std::path::Path;

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

/// Main's red checks, as `bay status --all` reads them from `dir`.
fn red(held: &Held, dir: &Path) -> serde_json::Value {
    // A plain status fetches what `--all` reads.
    result_json(held.test_box.bay(dir).arg("status"));
    let (_, status) = result_json(held.test_box.bay(dir).args(["status", "--all"]));
    assert!(status["detail"]["red"].is_array(), "{status}");
    status["detail"]["red"].clone()
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
fn a_wait_for_a_red_checks_environment_ends_when_main_is_green_on_it() {
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
    // The one runner serves `local` only; no live runner serves `post-env`.
    let a = held.executor("exec-a", PRODUCER);
    held.test_box
        .git_ok(&a, &["config", "bay.runnerName", "exec-a"]);
    held.test_box
        .git_ok(&a, &["config", "bay.producerName", "exec-a-checks"]);
    held.test_box
        .git_ok(&a, &["config", "--add", "bay.checkEnvironment", "local"]);
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
    assert_ne!(red(&held, &broad), serde_json::json!([]), "main is not red");

    // AND a change on src/ and web/ that waits for a worker for `post-env`
    let on_web = held.queue_with(
        "on-web",
        &["src", "web"],
        &[("src/more.rs", "fn more() {}\n"), ("web/more.ts", "more\n")],
    );
    let pass = held.pass(&a);
    assert_eq!(
        ran_for(&pass, &on_web)["reason"],
        "CHECK_UNAVAILABLE",
        "{pass}"
    );
    assert!(
        chain(&held, &on_web)
            .iter()
            .any(|line| line.starts_with("unavailable ") && line.contains("post-env")),
        "{:?}",
        chain(&held, &on_web)
    );

    // WHEN main goes green on `web`: a later landing's run passes
    let docs = held.queue_with("docs", &["docs"], &[("docs/more.md", "more\n")]);
    let pass = held.pass(&a);
    assert!(
        held.test_box.trunk_change_ids(&held.remote).contains(&docs),
        "{pass}"
    );
    answer(&held, &broad, "pass");
    assert_eq!(
        red(&held, &broad),
        serde_json::json!([]),
        "main is still red"
    );

    // THEN the runner that serves `local` takes the change on its next pass, and it merges
    let pass = held.pass(&a);
    let on_main = held.test_box.trunk_change_ids(&held.remote);
    assert!(
        on_main.contains(&on_web),
        "{on_web} did not merge: {pass}\n{:?}",
        chain(&held, &on_web)
    );
}
