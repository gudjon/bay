//! G589. `bay runner drain` asks a clone's runner to stop between operations; it stops once its
//! post pieces end, and its service starts it again.
//!
//! A runner's service restarts it whenever it ends, so a restart is how a new environment reaches
//! it. A restart by signal ends the runner inside whatever it runs (a fleet finding, 2026-10-30:
//! changing a runner's environment waited for an empty queue, which a busy line never has). Now
//! `bay runner drain` writes one file in the clone's `.git/bay`; the runner keeps landing changes
//! while a post piece runs, starts no new piece, and once none runs takes the file away and
//! exits 0, its result naming the drain and the piece's verdict.
//!
//! Guards to mutate: in `src/executor/drain.rs`, `Drain::asked` always false (the runner never
//! stops), the `!post.idle()` arm of `Drain::point` (it stops while `slow` runs) and its
//! `remove_file` (the file stays, so the restarted runner stops again); in `src/executor.rs`,
//! the drain term of `leaving` (a second `slow` starts on the change that landed meanwhile).

use crate::support;

use std::io::BufRead as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::{TestBox, result_json};

/// A pre check `tests`, and a post check `slow` on `src/**`.
const POLICY: &str = "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n\n[[check]]\nid = \"slow\"\nenvironment = \"env-a\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 150\nwhen_paths = [\"src/**\"]\n";

const HEAD: &str = r#"#!/bin/sh
req=$(cat)
commit=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\1/p')
tree=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\2/p')
"#;

const TAIL: &str = r#"printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"selected":1,"completed":1},"checks":[{"id":"t1","verdict":"pass"}]}' "$commit" "$tree"
"#;

/// The adapter: every check passes with one unit, and `slow` first runs `hold` as the test,
/// outside the sandbox.
fn adapter(hold: &str) -> String {
    [
        HEAD,
        "case \"$req\" in\n*'\"plan\":{\"id\":\"slow\"'*)\n",
        hold,
        ";;\nesac\n",
        TAIL,
    ]
    .concat()
}

/// A child killed when the test ends, so a failed assertion never leaves it running.
struct Reaped(std::process::Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Lets `slow` go when it is dropped, so a failed assertion never leaves it waiting.
struct Release(PathBuf);

impl Drop for Release {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, "");
    }
}

/// Whether `done` holds within `limit`.
fn within(limit: Duration, done: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    while !done() {
        if start.elapsed() >= limit {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

/// The first line `out` writes, within two minutes.
fn first_line(out: impl std::io::Read + Send + 'static) -> String {
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::BufReader::new(out).read_line(&mut line);
        let _ = send.send(line);
    });
    receive
        .recv_timeout(Duration::from_secs(120))
        .expect("the runner never stopped")
}

/// Seeds `repo` with the check, the policy and a runner that runs post checks: `slow` adds a
/// line to `started` and waits for `go`.
fn seed(test_box: &TestBox, repo: &Path, started: &Path, go: &Path) {
    let hold = test_box.relay(&format!(
        "echo x >> '{}'; until [ -e '{}' ]; do sleep 0.05; done",
        started.display(),
        go.display()
    ));
    let files = vec![
        ("scripts/bay-check".to_owned(), adapter(&hold)),
        (".bay/policy.toml".to_owned(), POLICY.to_owned()),
        (
            ".bay/config.toml".to_owned(),
            "[runner]\npost = true\nbisect_post = false\n".to_owned(),
        ),
    ];
    test_box.seed_with_check(
        repo,
        &[("src/a.rs", "fn a() {}\n"), ("docs/readme", "docs\n")],
        &files,
    );
}

/// Starts change `name` on `path`, adds a file there, and queues it without waiting: the
/// change's id.
fn queue(test_box: &TestBox, repo: &Path, name: &str, path: &str) -> String {
    let started = test_box.bay_ok(repo, &["start", name, "--paths", path]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(path).join(name), "x\n").unwrap();
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    started["change"].as_str().unwrap().to_owned()
}

/// How many times `slow` started: one line in `started` for each start.
fn starts(started: &Path) -> usize {
    std::fs::read_to_string(started)
        .unwrap_or_default()
        .lines()
        .count()
}

/// The post rows of a runner result.
fn post_rows(result: &Value) -> Vec<&Value> {
    result["detail"]["ran"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["post"].as_array())
        .flatten()
        .collect()
}

#[test]
fn a_drained_runner_stops_between_operations_once_its_post_pieces_end() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let started = test_box.path().join("slow-started");
    let go = test_box.path().join("slow-go");
    seed(&test_box, &repo, &started, &go);
    let one = queue(&test_box, &repo, "one", "src");

    // A runner loop lands `one`, then starts `slow` on main's tip.
    let executor = test_box.clone_of(&remote, "executor");
    let log = test_box.path().join("runner.log");
    let mut child = Reaped(
        test_box
            .bay_process(&executor)
            .args(["--json", "runner", "run", "--interval-s", "1"])
            .env("BAY_DISABLE_AUTOUPDATE", "1")
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap(),
    );
    let release = Release(go);
    let said = || std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        within(Duration::from_secs(60), || started.exists()),
        "slow never runs: {}",
        said()
    );
    assert!(test_box.trunk_change_ids(&remote).contains(&one));

    // A drain, asked while `slow` runs: one file in the clone's `.git/bay`.
    let asked = test_box.bay_ok(&executor, &["runner", "drain"]);
    assert_eq!(asked["decision"], "ok", "{asked}");
    let file = PathBuf::from(asked["detail"]["drain"]["path"].as_str().unwrap());
    assert!(file.exists(), "{asked}");
    assert!(
        file.starts_with(executor.join(".git").join("bay")),
        "{asked}"
    );
    assert!(
        asked["say"]
            .as_str()
            .unwrap()
            .contains("stops between jobs"),
        "{asked}"
    );

    // The runner waits for `slow`, and lands a change on `src` meanwhile.
    let two = queue(&test_box, &repo, "two", "src");
    assert!(
        within(Duration::from_secs(90), || test_box
            .trunk_change_ids(&remote)
            .contains(&two)),
        "two never lands while slow runs: {}",
        said()
    );
    let waits = "waits for 1 post piece(s) to end before it stops, as bay runner drain asked";
    assert!(
        within(Duration::from_secs(60), || said().contains(waits)),
        "the runner never says why it waits: {}",
        said()
    );
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "the runner never stops while slow runs"
    );
    assert!(file.exists(), "the drain is still asked");

    // `slow` ends: the runner starts no `slow` on `two`, stops with the piece's verdict and
    // takes the file away; its own exit is 0, so its service starts it again.
    drop(release);
    let line = first_line(child.0.stdout.take().unwrap());
    let result: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(result["decision"], "ok", "{result}");
    assert!(
        result["detail"]["drained"]["asked_at"].is_string(),
        "the result names the drain: {result}"
    );
    assert!(result["detail"].get("reexec").is_none(), "{result}");
    assert!(
        post_rows(&result)
            .iter()
            .any(|row| row["check"] == "slow" && row["verdict"] == "pass"),
        "the stop carries slow's verdict: {result}"
    );
    let status = child.0.wait().unwrap();
    assert_eq!(status.code(), Some(0), "{}", said());
    assert!(!file.exists(), "the runner took the drain away: {}", said());
    assert_eq!(
        starts(&started),
        1,
        "no slow starts once drained: {}",
        said()
    );
}

#[test]
fn a_drain_is_taken_back_with_cancel() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let started = test_box.path().join("slow-started");
    let go = test_box.path().join("slow-go");
    seed(&test_box, &repo, &started, &go);

    let none = test_box.bay_ok(&repo, &["runner", "drain", "--cancel"]);
    assert_eq!(none["detail"]["cancelled"], false, "{none}");
    assert_eq!(
        none["say"], "No stop was asked of the runner here.",
        "{none}"
    );

    let asked = test_box.bay_ok(&repo, &["runner", "drain"]);
    let file = PathBuf::from(asked["detail"]["drain"]["path"].as_str().unwrap());
    assert!(file.exists(), "{asked}");

    let taken = test_box.bay_ok(&repo, &["runner", "drain", "--cancel"]);
    assert_eq!(taken["detail"]["cancelled"], true, "{taken}");
    assert!(taken["detail"]["drain"].is_null(), "{taken}");
    assert_eq!(
        taken["say"], "The runner here is no longer asked to stop.",
        "{taken}"
    );
    assert!(!file.exists(), "{taken}");
}
