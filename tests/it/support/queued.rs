//! A bay host with a key registry, two producers and a publication queue the test fills, for
//! the throughput G-tests (`crate::throughput`: G256, G257, G258), or one whose evidence counts
//! unsigned (G318: no registry, or `signing = "approvals"`). Its check logs every request
//! it gets, can hold its first run until the test lets it go, and fails a run when the test's
//! shell condition holds in the candidate's tree.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{
    AGENT, EXECUTOR, PRODUCER, REGISTRAR, TestBox, TestKeys, check_files, key_line, passing_result,
    result_json,
};

/// The second producer the registry enrolls, under the test keys' rogue key.
pub const OTHER_PRODUCER: &str = "ci-2";

/// What the host's check does.
pub struct Check<'a> {
    /// `.bay/config.toml`.
    pub config: &'a str,
    /// `.bay/policy.toml`, or `None` for one check `tests` that every change requires.
    pub policy: Option<&'a str>,
    /// The files trunk is seeded with.
    pub seed: &'a [(&'a str, &'a str)],
    /// Whether the first run waits until the test lets it go (`Held::go`).
    pub hold_first: bool,
    /// A shell condition, tested in the candidate's tree with the run's number in `$n`, under
    /// which the run is a `product_failure`; `false` for none.
    pub fails: &'a str,
}

/// What the host's registry signs (N-21, G318).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trust {
    /// A key registry that signs everything and enrols both producers.
    Signed,
    /// No key registry: every record counts unsigned.
    Unsigned,
    /// A key registry of the agent and the registrar only, for a policy that says `[policy]
    /// signing = "approvals"`: check evidence counts unsigned.
    Approvals,
}

/// The host, the agent's clone, and the check's files.
pub struct Held {
    pub test_box: TestBox,
    pub remote: PathBuf,
    pub repo: PathBuf,
    /// One request per line, as the adapter got it.
    pub runs: PathBuf,
    /// Exists once the first run waits.
    pub waiting: PathBuf,
    /// Lets the first run go.
    pub go: PathBuf,
    pub keys: TestKeys,
    pub trust: Trust,
}

/// A bay host whose check does what `check` says. Trunk is seeded before the registry exists:
/// from then on only executors publish.
#[must_use]
pub fn host(check: &Check) -> Held {
    host_under(check, Trust::Signed)
}

/// [`host`], with the registry `trust` names. Every clone names its producer ([`PRODUCER`] or
/// [`OTHER_PRODUCER`]) whatever the trust, so unsigned records keep one writer per name.
#[must_use]
pub fn host_under(check: &Check, trust: Trust) -> Held {
    let test_box = TestBox::new();
    let remote = test_box.path().join("host.git");
    test_box.host_init(&remote);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &remote);
    let runs = test_box.path().join("runs");
    let waiting = test_box.path().join("held");
    let go = test_box.path().join("go");
    let first = if check.hold_first {
        format!(
            "if [ \"$n\" -eq 1 ]; then : > '{}'; until [ -e '{}' ]; do sleep 0.05; done; fi",
            waiting.display(),
            go.display()
        )
    } else {
        ":".to_owned()
    };
    // Any run the test marks (`Held::hold_run`) waits too, until the test lets it go.
    let waits = format!(
        "{first}; if [ -e '{dir}/hold-'\"$n\" ]; then : > '{dir}/held-'\"$n\"; until [ -e '{dir}/go-'\"$n\" ]; do sleep 0.05; done; fi",
        dir = test_box.path().display()
    );
    // The test side of the check (G62): it logs the run, may hold it, and answers its number.
    let relayed = test_box.relay(&format!(
        "exec 3>&1 >/dev/null\n\
         printf '%s\\n' \"$req\" >> '{runs}'; n=$(wc -l < '{runs}' | tr -d ' '); {waits}\n\
         echo \"$n\" >&3",
        runs = runs.display(),
    ));
    let failure = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let before = format!(
        "n=$({relayed})\nif {fails}; then {failure}; exit 0; fi",
        fails = check.fails
    );
    let mut files = check_files(&before, &passing_result(1), 1, 120);
    if let Some(policy) = check.policy {
        files.retain(|(path, _)| path != ".bay/policy.toml");
        files.push((".bay/policy.toml".to_owned(), policy.to_owned()));
    }
    files.push((".bay/config.toml".to_owned(), check.config.to_owned()));
    test_box.seed_with_check(&repo, check.seed, &files);
    let keys = test_box.keys();
    match trust {
        Trust::Signed => {
            test_box.enroll_classes(
                &repo,
                &keys,
                &keys.executor,
                &[],
                &[(PRODUCER, &keys.producer), (OTHER_PRODUCER, &keys.rogue)],
            );
        }
        Trust::Unsigned => {}
        Trust::Approvals => {
            test_box.enroll_files(
                &repo,
                &keys,
                &[
                    ("agent", key_line(AGENT, &keys.agent)),
                    ("registry", key_line(REGISTRAR, &keys.registry)),
                ],
            );
        }
    }
    test_box.use_keys(&repo, &keys);
    Held {
        test_box,
        remote,
        repo,
        runs,
        waiting,
        go,
        keys,
        trust,
    }
}

impl Held {
    /// The requests the adapter got, one per run.
    #[must_use]
    pub fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.runs)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// Starts change `file` on `<dir>/`, adds `<dir>/<file>.rs` in its view, submits it and
    /// puts it in the queue behind every place already there, as an executor would. Returns
    /// its change id.
    #[must_use]
    pub fn queue(&self, file: &str, dir: &str) -> String {
        let path = format!("{dir}/{file}.rs");
        let text = format!("fn {file}() {{}}\n");
        self.queue_with(file, &[dir], &[(path.as_str(), text.as_str())])
    }

    /// Starts change `intent` on the directories `dirs`, writes `files` (path, text) in its
    /// view, submits it and puts it in the queue behind every place already there. Returns its
    /// change id.
    #[must_use]
    pub fn queue_with(&self, intent: &str, dirs: &[&str], files: &[(&str, &str)]) -> String {
        let paths: Vec<String> = dirs.iter().map(|dir| format!("{dir}/")).collect();
        let mut args = vec!["start", intent, "--paths"];
        args.extend(paths.iter().map(String::as_str));
        let started = self.test_box.bay_ok(&self.repo, &args);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        for (path, text) in files {
            let full = view.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, text).unwrap();
        }
        let (code, submitted) = result_json(self.test_box.bay(&view).arg("merge"));
        assert_eq!(code, 0, "{submitted}");
        let op_id = submitted["op_id"].as_str().unwrap().to_owned();
        let signer = bay::signing::Signer {
            key: self.keys.executor.clone(),
            name: EXECUTOR.to_owned(),
        };
        // An unsigned host's reservations are unsigned, as an executor there writes them.
        let signer = (self.trust == Trust::Signed).then_some(&signer);
        let opened = bay::repo::Repo::open(&self.repo).unwrap();
        bay::queue::join(&opened, signer, &op_id, &id).unwrap();
        id
    }

    /// An executor's clone of the host, writing evidence as `producer` ([`PRODUCER`] or
    /// [`OTHER_PRODUCER`]).
    #[must_use]
    pub fn executor(&self, name: &str, producer: &str) -> PathBuf {
        let clone = self.test_box.clone_of(&self.remote, name);
        self.test_box.use_keys(&clone, &self.keys);
        if producer != PRODUCER {
            self.test_box.git_ok(
                &clone,
                &[
                    "config",
                    "bay.producerKey",
                    self.keys.rogue.to_str().unwrap(),
                ],
            );
            self.test_box
                .git_ok(&clone, &["config", "bay.producerName", producer]);
        }
        clone
    }

    /// One `executor run --once` in `clone`, in the background.
    #[must_use]
    pub fn spawn(&self, clone: &Path) -> std::process::Child {
        self.test_box
            .bay_process(clone)
            .args(["--json", "runner", "run", "--once"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// One `executor run --once` in `clone`: its result.
    #[must_use]
    pub fn pass(&self, clone: &Path) -> Value {
        self.test_box.bay_ok(clone, &["runner", "run", "--once"])
    }

    /// Waits until the first run holds.
    pub fn wait_held(&self) {
        let start = Instant::now();
        while !self.waiting.exists() {
            assert!(
                start.elapsed() < Duration::from_secs(120),
                "the first run never started"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Lets the first run go.
    pub fn go(&self) {
        std::fs::write(&self.go, "").unwrap();
    }

    /// Makes run `n` wait, once it starts, until [`Held::go_run`] lets it go.
    pub fn hold_run(&self, n: u32) {
        std::fs::write(self.test_box.path().join(format!("hold-{n}")), "").unwrap();
    }

    /// Waits until run `n` holds.
    pub fn wait_held_run(&self, n: u32) {
        let held = self.test_box.path().join(format!("held-{n}"));
        let start = Instant::now();
        while !held.exists() {
            assert!(
                start.elapsed() < Duration::from_secs(120),
                "run {n} never started"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Lets run `n` go.
    pub fn go_run(&self, n: u32) {
        std::fs::write(self.test_box.path().join(format!("go-{n}")), "").unwrap();
    }

    /// The tree of `rev` on the host.
    #[must_use]
    pub fn tree(&self, rev: &str) -> String {
        self.test_box
            .git_bare(&self.remote, &["rev-parse", &format!("{rev}^{{tree}}")])
    }

    /// The records `producer` holds on `tree` on the host.
    #[must_use]
    pub fn records(&self, producer: &str, tree: &str) -> Vec<Value> {
        let notes = self.test_box.git_bare(
            &self.remote,
            &[
                "notes",
                &format!("--ref=refs/notes/bay/evidence/{producer}"),
                "show",
                tree,
            ],
        );
        serde_json::from_str(&notes).unwrap()
    }
}

/// The result of a background pass, once it ended.
#[must_use]
pub fn finished(child: std::process::Child) -> Value {
    let out = child.wait_with_output().unwrap();
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stdout)))
}

/// The result a pass holds for `change`.
#[must_use]
pub fn ran_for<'a>(pass: &'a Value, change: &str) -> &'a Value {
    pass["detail"]["ran"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["change"] == change)
        .unwrap_or_else(|| panic!("no result for {change}: {pass}"))
}
