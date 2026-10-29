//! G542 (#123). A runner moves to a new bay binary only once its post pieces end, and drives the
//! queue meanwhile.
//!
//! With its post work on threads of its own (G541), a runner whose binary changed could move
//! with a post check still running: that run would be lost, and its environment would wait for
//! a later runner to start it again. Now a runner whose binary changed starts no post piece,
//! keeps landing the queue's changes, and moves once every running piece ended; the result that
//! names the move carries the piece's verdict.
//!
//! Guards to mutate: in `src/executor.rs`, the `!post.idle()` arm in `reexec_point` (the runner
//! moves while `slow` runs) and the `post.idle()` term of the pass's stop between operations (no
//! operation runs while the runner waits, and `two` never lands); in `src/bisect.rs` `plan`,
//! seed `taken` empty instead of with `busy` (a later pass starts a second `slow`).

// The new binary is a shell script, and the move is an `exec`: Unix only.
#![cfg(unix)]

use crate::support;

use std::io::BufRead as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use support::{TestBox, result_json};

use crate::release_fixture::RetryBusy as _;

const NEWER: &str = "9.0.0";

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

/// The records of check `id` on the remote, about any commit.
fn records(test_box: &TestBox, remote: &Path, id: &str) -> Vec<Value> {
    let refs = test_box.git_bare(
        remote,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/notes/bay/evidence/",
        ],
    );
    let mut out = Vec::new();
    for notes_ref in refs.lines() {
        let blobs = test_box.git_bare(
            remote,
            &["ls-tree", "-r", "--format=%(objectname)", notes_ref],
        );
        for blob in blobs.lines() {
            let text = test_box.git_bare(remote, &["cat-file", "blob", blob]);
            let records: Vec<Value> = serde_json::from_str(&text).unwrap();
            out.extend(
                records
                    .into_iter()
                    .filter(|record| record["check"]["id"] == id),
            );
        }
    }
    out
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
        .expect("the runner never named its move")
}

/// Puts a new bay at `installed`: it answers as bay `NEWER`, takes the runner's command, and,
/// once started in the runner's place, writes its arguments to `$HOME/ran-<NEWER>` and exits 1.
fn install_newer(installed: &Path) {
    let newer = installed.with_extension("new");
    std::fs::write(
        &newer,
        format!(
            "#!/bin/sh\ncase \"$*\" in\n--version) echo 'bay {NEWER}' ;;\n*--help*) exit 0 ;;\n*) printf '%s\\n' \"$*\" > \"$HOME/ran-{NEWER}\"; exit 1 ;;\nesac\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&newer, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&newer, installed).unwrap();
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
    std::fs::read_to_string(started).unwrap().lines().count()
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

/// The installed bay running a runner loop on `executor`, its stderr in `log`.
fn runner(test_box: &TestBox, installed: &Path, executor: &Path, log: &Path) -> Reaped {
    let path = format!(
        "{}:{}:/bin:/usr/bin",
        test_box.fakebin.display(),
        test_box.local_bin().display()
    );
    Reaped(
        Command::new(installed)
            .arg("-C")
            .arg(executor)
            .args(["--json", "runner", "run", "--interval-s", "1"])
            .env_clear()
            .env("PATH", path)
            .env("HOME", &test_box.home)
            .env("GIT_CONFIG_GLOBAL", &test_box.global_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", test_box.path())
            .env(
                "BAY_TEST_USERNS_RESTRICT",
                test_box.path().join("userns-restrict"),
            )
            .env("BAY_DISABLE_AUTOUPDATE", "1")
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(log).unwrap())
            .spawn_busy(),
    )
}

#[test]
fn a_runner_moves_to_a_new_binary_only_after_its_post_pieces_end() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let started = test_box.path().join("slow-started");
    let go = test_box.path().join("slow-go");
    seed(&test_box, &repo, &started, &go);
    let one = queue(&test_box, &repo, "one", "src");

    // bay as installed runs a runner loop: it lands `one`, then starts `slow` on main's tip.
    let executor = test_box.clone_of(&remote, "executor");
    std::fs::create_dir_all(test_box.local_bin()).unwrap();
    let installed = test_box.local_bin().join("bay");
    std::fs::copy(env!("CARGO_BIN_EXE_bay"), &installed).unwrap();
    let log = test_box.path().join("runner.log");
    let mut child = runner(&test_box, &installed, &executor, &log);
    let release = Release(go);
    let said = || std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        within(Duration::from_secs(60), || started.exists()),
        "slow never runs: {}",
        said()
    );
    assert!(test_box.trunk_change_ids(&remote).contains(&one));

    // Passes run before the new bay comes; none starts a second `slow`: a piece holds `env-a`.
    std::thread::sleep(Duration::from_secs(3));

    // A new bay at the install path, while `slow` runs.
    install_newer(&installed);

    // The runner waits for `slow`, and lands a change meanwhile.
    let two = queue(&test_box, &repo, "two", "docs");
    assert!(
        within(Duration::from_secs(90), || test_box
            .trunk_change_ids(&remote)
            .contains(&two)),
        "two never lands while slow runs: {}",
        said()
    );
    let waits = format!("waits for 1 post piece(s) to end before it moves to bay {NEWER}");
    assert!(
        within(Duration::from_secs(60), || said().contains(&waits)),
        "the runner never says why it waits: {}",
        said()
    );
    assert_eq!(starts(&started), 1, "one slow at a time: {}", said());
    let ran_new = test_box.home.join(format!("ran-{NEWER}"));
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "the runner still runs"
    );
    assert!(!ran_new.exists(), "the runner never moved while slow runs");
    assert_eq!(records(&test_box, &remote, "slow"), Vec::<Value>::new());

    // `slow` ends; the runner names the move with its verdict, and the new bay runs.
    drop(release);
    let line = first_line(child.0.stdout.take().unwrap());
    let result: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(result["decision"], "ok", "{result}");
    assert_eq!(result["detail"]["reexec"]["to"], NEWER, "{result}");
    assert!(
        post_rows(&result)
            .iter()
            .any(|row| row["check"] == "slow" && row["verdict"] == "pass"),
        "the move carries slow's verdict: {result}"
    );
    assert_eq!(records(&test_box, &remote, "slow").len(), 1, "{result}");
    assert!(
        within(Duration::from_secs(60), || ran_new.exists()),
        "the new bay never runs as the runner: {}",
        said()
    );
    assert_eq!(
        std::fs::read_to_string(&ran_new).unwrap().trim(),
        format!("-C {} --json runner run --interval-s 1", executor.display())
    );
    let status = child.0.wait().unwrap();
    assert_eq!(status.code(), Some(1), "the new bay's own exit");
}
