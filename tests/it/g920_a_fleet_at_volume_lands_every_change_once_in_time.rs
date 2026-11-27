//! G920: a fleet at volume lands every change once, in time (the scale plan § 5, the gate of
//! 0.8.0). An instrument, not a case of the suite: it is ignored, so the gate never runs it, and a
//! person runs it by hand with the parameters below. It writes its report, then fails naming each
//! row that reads red, so a red row is never silent.
//!
//! What it models: a synthetic repository of 4,000 files on an origin made by `bay host`, with a
//! key registry, on this machine. Simulated agents share clones, ten to a clone, as sessions share
//! a checkout; each change touches 1 to 5 files, and 1.4 % of changes touch a file of a change
//! still ahead of them. Each agent follows the skill's script: `bay start`, edit and commit in the
//! view, `bay merge <change>`, then `bay merge <change> --wait` beside its next work, and it takes a
//! refusal's next step. Its clone serves no check environment, so its wait only watches. Runners
//! are `bay runner run` processes, as a service starts them. The pre check takes 2 s on every
//! candidate; the post checks take 30 s and 300 s, each on its own paths and environment.
//! Arrivals come at a rate with a peak: the first 30 s of every 10 minutes at the peak rate, the
//! rest at the rate that keeps the average. One more change breaks the 30 s post check: it is sent
//! at the first arrival a third of the way in, or later, once that check has passed on main, so
//! bisection has a green commit below it. A run of fewer than 20 changes sends none, and its row
//! reads not run.
//!
//! What it reports, one row each, with the plan row a red number names:
//!
//! - lead (S2, S3): request to landing on main, p50 and p90. Green at p50 <= 20 s, p90 <= 60 s.
//!   A change that never landed and was never refused counts as never.
//! - hook (S3): the origin's hook time per push, p99, by a timer wrapped around bay's hook
//!   commands (a push is named by its receiving process, read from Linux's `/proc`). Green at
//!   <= 200 ms.
//! - live refs (S4): refs on the origin after the run, by kind, and per landing to 30,000
//!   landings, a model until a run lands 30,000. Green under 5,000.
//! - status (S4): `bay status` from an agent clone, sampled under load and read after it. Green at
//!   p50 <= 200 ms in both.
//! - census (S10): every submitted change's patch (`git patch-id` of its diff with no context) on
//!   main exactly once. Green at 0 lost and 0 double.
//! - bad change (S2): bisection names the bad change as the red's culprit and a runner undoes it.
//!
//! The JSON report also carries each change's request and landing times, so a red lead shows
//! whether the line grew through the run or each change waited alike, and a lead split read from
//! the runners' logs: for each landing, its batch, why the batch stopped or never formed, its wait
//! for the queue's head, and the head's visit with its check and publish times; for the run, lone
//! landings against batched ones, the mean batch, the stopped reasons and the refused landing
//! pushes. The runners' logs, each line stamped with its second of the run, and the hook timer's
//! log are kept beside the report.
//!
//! Parameters, from the environment: `BAY_LOAD_CHANGES` (60), `BAY_LOAD_RATE` (0.35 a second; 0
//! submits as fast as the agents can, a history run), `BAY_LOAD_PEAK` (1.5 a second),
//! `BAY_LOAD_AGENTS` (10), `BAY_LOAD_RUNNERS` (2), `BAY_LOAD_SECONDS` (no bound: submitting ends at
//! the first of the count and the duration), `BAY_LOAD_SIGNING` (`approvals`, or `all`, under which
//! a runner opens no automatic undo by design) and `BAY_LOAD_OUT` (the JSON report; the summary
//! goes beside it with `.md`, and the logs with `.runner-<n>.log` and `.hook-times.log`). Run it
//! from a release build for the numbers a fleet sees:
//! `cargo test --release --test it g920 -- --ignored --nocapture`.

use crate::support;

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::{BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::talk::fresh_key;
use support::{REGISTRAR, TestBox, key_line};

/// The synthetic tree: 40 directories of 100 files.
const FILES: usize = 4_000;
const PER_DIR: usize = 100;
/// Lines in a file, and the gap between two edits of one file: changes that edit lines three
/// apart merge without a conflict.
const LINES: usize = 96;
const SPACING: usize = 3;
/// The share of changes that touch a file of a change still ahead of them.
const OVERLAP: f64 = 0.014;
/// Each cycle of this many seconds opens with [`PEAK_FOR_S`] at the peak rate.
const PEAK_EVERY_S: f64 = 600.0;
const PEAK_FOR_S: f64 = 30.0;
/// `[red] undo_after_s` of the synthetic repository.
const UNDO_AFTER_S: u64 = 5;
/// How long after the last arrival the run waits for every change to end.
const DRAIN_S: u64 = 1_800;
/// How long after the bad change lands the run waits for its undo to land, and how long past
/// the wait after its culprit is named.
const UNDO_BOUND_S: u64 = 900;
const UNDO_NAMED_BOUND_S: u64 = 300;
/// The fewest arrivals a run sends its bad change among, and how long after the last arrival
/// the bad change still waits for the 30 s post check's first pass on main.
const BAD_FROM_CHANGES: usize = 20;
const BAD_WAITS_S: u64 = DRAIN_S / 2;
/// How long the run waits for a stopped runner's log to close: a check it started may hold it.
const LOG_CLOSE: Duration = Duration::from_secs(10);
/// How often the status sampler reads, and how many reads follow the run.
const STATUS_EVERY: Duration = Duration::from_secs(5);
const READS_AFTER: usize = 10;
/// Agents on one clone.
const AGENTS_PER_CLONE: usize = 10;
/// How many times an agent takes a refusal's next step before it gives the change up.
const MAX_REPAIRS: usize = 3;
/// The landings a run is judged at for live refs.
const LANDINGS_AT: usize = 30_000;
/// The file the bad change adds, and the directory it declares.
const BROKEN: &str = "src/d00/broken.txt";
const BROKEN_DIR: &str = "src/d00";
/// The environment an agent clone declares: no check names it, so an agent never runs a check.
const AGENT_DESK: &str = "agent-desk";
/// The test build's timing overrides: the instrument runs bay with a fleet's timings.
const TEST_TIMINGS: [&str; 3] = [
    "BAY_TEST_SWEPT_S",
    "BAY_TEST_SERVE_STALE_S",
    "BAY_TEST_STATUS_DEADLINE_S",
];

/// The check adapter: `pre` takes 2 s, `post-30` 30 s and `post-300` 300 s, and `post-30` fails on a
/// tree that holds [`BROKEN`].
const CHECK: &str = r#"#!/bin/sh
req=$(cat)
commit=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\1/p')
tree=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\2/p')
case "$1" in
pre) sleep 2 ;;
post-30) sleep 30 ;;
post-300) sleep 300 ;;
esac
verdict=pass
if [ "$1" = post-30 ] && [ -e src/d00/broken.txt ]; then verdict=product_failure; fi
printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"discovered":1,"selected":1,"completed":1},"checks":[{"id":"%s","verdict":"%s"}]}' "$commit" "$tree" "$1" "$verdict"
"#;

/// The wake adapter: every wake is taken, and none goes anywhere.
const WAKE: &str = "#!/bin/sh\ncat > /dev/null\nprintf '{\"schema\":\"bay.wake-result/1\",\"result\":\"woken\",\"reason\":null}'\n";

/// The synthetic repository's config: a runner runs the post checks on main's tip, bisects a
/// red one, undoes its culprit, and batches, as a repository at volume sets it.
fn config() -> String {
    format!(
        "main = \"main\"\n\n[runner]\npost = true\nbisect_post = true\nbatch = {{ max = 8 }}\n\n[red]\nundo_after_s = {UNDO_AFTER_S}\n\n[plugin.wake]\ncommand = \"scripts/bay-wake\"\n"
    )
}

/// The synthetic repository's policy under `signing`.
fn policy(signing: &str) -> String {
    let unsigned = if signing == "approvals" {
        "\n[policy]\nsigning = \"approvals\"\nreuse_unsigned = true\n"
    } else {
        ""
    };
    let check = |id: &str, environment: &str, stage: &str, timeout_s: u32, paths: &str| {
        format!(
            "\n[[check]]\nid = \"{id}\"\nenvironment = \"{environment}\"\n{stage}command = \"scripts/bay-check {id}\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = {timeout_s}\n{paths}"
        )
    };
    format!(
        "revision = 1\n{unsigned}{}{}{}",
        check("pre", "local", "", 60, ""),
        check(
            "post-30",
            "post-30",
            "stage = \"post\"\n",
            120,
            "when_paths = [\"src/d0*/**\", \"src/d1*/**\"]\n"
        ),
        check(
            "post-300",
            "post-300",
            "stage = \"post\"\n",
            600,
            "when_paths = [\"src/d2*/**\", \"src/d3*/**\"]\n"
        ),
    )
}

fn file_path(file: usize) -> String {
    format!("src/d{:02}/f{:03}.txt", file / PER_DIR, file % PER_DIR)
}

fn file_text(file: usize) -> String {
    let path = file_path(file);
    (0..LINES).fold(String::new(), |mut text, line| {
        let _ = writeln!(text, "{path} line {line}");
        text
    })
}

/// The run's parameters.
struct Params {
    changes: usize,
    rate: f64,
    peak: f64,
    agents: usize,
    runners: usize,
    seconds: Option<f64>,
    signing: String,
    out: PathBuf,
}

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key).map_or(default, |text| {
        text.trim()
            .parse()
            .unwrap_or_else(|_| panic!("{key} is not a number: {text}"))
    })
}

impl Params {
    fn from_env() -> Self {
        let signing = std::env::var("BAY_LOAD_SIGNING").unwrap_or_else(|_| "approvals".to_owned());
        assert!(
            signing == "approvals" || signing == "all",
            "BAY_LOAD_SIGNING is approvals or all, not {signing}"
        );
        let out = std::env::var_os("BAY_LOAD_OUT").map_or_else(
            || Path::new(env!("CARGO_TARGET_TMPDIR")).join("g920-load.json"),
            PathBuf::from,
        );
        let params = Self {
            changes: env_or("BAY_LOAD_CHANGES", 60),
            rate: env_or("BAY_LOAD_RATE", 0.35),
            peak: env_or("BAY_LOAD_PEAK", 1.5),
            agents: env_or("BAY_LOAD_AGENTS", 10),
            runners: env_or("BAY_LOAD_RUNNERS", 2),
            seconds: std::env::var_os("BAY_LOAD_SECONDS").map(|_| env_or("BAY_LOAD_SECONDS", 0.0)),
            signing,
            out,
        };
        assert!(params.changes > 0 && params.agents > 0 && params.runners > 0);
        params
    }

    fn json(&self) -> Value {
        json!({
            "changes": self.changes,
            "rate": self.rate,
            "peak": self.peak,
            "agents": self.agents,
            "runners": self.runners,
            "seconds": self.seconds,
            "signing": self.signing,
            "files": FILES,
            "overlap": OVERLAP,
            "undo_after_s": UNDO_AFTER_S,
            "formats": support::formats(),
        })
    }
}

/// A seeded generator, so two runs with one set of parameters make the same changes.
struct Rng(u64);

impl Rng {
    fn step(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// In [0, 1).
    fn unit(&mut self) -> f64 {
        f64::from(u32::try_from(self.step() >> 32).unwrap()) / 4_294_967_296.0
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.step() % u64::try_from(n).unwrap()).unwrap()
    }
}

/// The arrival times, in seconds from the start: exponential gaps at the rate of the moment.
fn arrivals(params: &Params, rng: &mut Rng) -> Vec<f64> {
    if params.rate <= 0.0 {
        return vec![0.0; params.changes];
    }
    let peak = params.peak.max(params.rate);
    let base = ((params.rate * PEAK_EVERY_S - peak * PEAK_FOR_S) / (PEAK_EVERY_S - PEAK_FOR_S))
        .max(params.rate / 10.0);
    let mut at = 0.0;
    let mut times = Vec::new();
    while times.len() < params.changes {
        let rate = if at % PEAK_EVERY_S < PEAK_FOR_S {
            peak
        } else {
            base
        };
        at += -(1.0 - rng.unit()).ln() / rate;
        if params.seconds.is_some_and(|seconds| at >= seconds) {
            break;
        }
        times.push(at);
    }
    times
}

/// One change, as the harness saw it.
#[derive(Default)]
struct Change {
    files: Vec<usize>,
    overlap: bool,
    bad: bool,
    id: Option<String>,
    patch: Option<String>,
    requested: Option<Instant>,
    landed: Option<Instant>,
    outcome: Option<String>,
    refusals: Vec<String>,
    error: Option<String>,
    start_ms: Option<f64>,
    submit_ms: Option<f64>,
}

impl Change {
    /// Whether the harness waits for nothing more of it.
    fn ended(&self) -> bool {
        self.error.is_some() || self.landed.is_some() || self.outcome.is_some()
    }

    /// Whether bay refused it for good, and it never landed: it is neither lost nor late.
    fn refused(&self) -> bool {
        self.landed.is_none()
            && self
                .outcome
                .as_deref()
                .is_some_and(|outcome| outcome.starts_with("refused"))
    }
}

/// What every thread of the run writes.
#[derive(Default)]
struct Book {
    changes: Vec<Change>,
    by_id: HashMap<String, usize>,
    /// The files of changes submitted and not landed, and the change of each.
    in_flight: BTreeMap<usize, usize>,
    slots: Vec<usize>,
    overlaps_missed: usize,
    bad_id: Option<String>,
    undo: Option<(String, Instant)>,
    culprit_named: Option<Instant>,
    undo_named: Option<String>,
    status_load: Vec<f64>,
    status_failures: Vec<String>,
}

/// The fleet: its box, origin, clones and keys.
struct Fleet {
    test_box: TestBox,
    origin: PathBuf,
    hook_log: PathBuf,
    seed_tip: String,
    agent_clones: Vec<PathBuf>,
    agent_keys: Vec<Option<PathBuf>>,
    runner_clones: Vec<PathBuf>,
    refs_before: usize,
}

fn agent_name(agent: usize) -> String {
    format!("agent-{agent:03}")
}

fn runner_name(runner: usize) -> String {
    format!("runner-{runner}")
}

/// Points the origin's hook commands at a timer around bay's own: its `pre-receive` hook, and its
/// `update` hook on a host that still has one (a host judges its trunk rule in `pre-receive`
/// since G933). Each call appends
/// `<push> <event> <start ns> <end ns> <exit>` to the log. Its push is the receiving process (the
/// timer's parent, or its grandparent when Git ran it through a shell) with that process's start
/// time, so a reused pid never joins two pushes.
fn wrap_hook(test_box: &TestBox, origin: &Path) -> PathBuf {
    let log = test_box.path().join("hook-times.log");
    let timer = test_box.path().join("hook-timer");
    let bay = env!("CARGO_BIN_EXE_bay");
    support::write_executable(
        &timer,
        &format!(
            "#!/bin/sh\nstart=$(date +%s%N)\n'{bay}' host hook \"$@\"\ncode=$?\nend=$(date +%s%N)\npush=$PPID\ncase \"$(cat /proc/$push/comm 2>/dev/null)\" in sh|dash|bash) push=$(cut -d' ' -f4 /proc/$push/stat 2>/dev/null) ;; esac\necho \"$push:$(cut -d' ' -f22 /proc/$push/stat 2>/dev/null) $1 $start $end $code\" >> '{log}'\nexit $code\n",
            log = log.display()
        ),
    );
    let mut wrapped = Vec::new();
    for (name, event) in [
        (bay::host::TRUNK_HOOK, "update"),
        (bay::host::SCOPE_HOOK, "pre-receive"),
    ] {
        let key = format!("hook.{name}.command");
        let found = test_box
            .git(origin)
            .args(["config", "--get", &key])
            .output()
            .unwrap();
        if !found.status.success() {
            continue;
        }
        let command = String::from_utf8_lossy(&found.stdout).trim().to_owned();
        assert!(
            command.ends_with(&format!("host hook {event}")),
            "the origin's {event} hook is no longer bay's: {command}"
        );
        test_box.git_bare(
            origin,
            &["config", &key, &format!("'{}' {event}", timer.display())],
        );
        wrapped.push(event);
    }
    assert!(
        wrapped.contains(&"pre-receive"),
        "the origin has no pre-receive hook of bay's to time"
    );
    log
}

impl Fleet {
    #[allow(clippy::too_many_lines)]
    fn new(params: &Params) -> Self {
        let test_box = TestBox::new();
        let origin = test_box.path().join("origin.git");
        test_box.host_init(&origin);
        let hook_log = wrap_hook(&test_box, &origin);
        let seed = test_box.repo_without_remote_any("seed");
        test_box.add_origin(&seed, &origin);
        for file in 0..FILES {
            let path = seed.join(file_path(file));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, file_text(file)).unwrap();
        }
        std::fs::write(seed.join(".bay/config.toml"), config()).unwrap();
        std::fs::write(seed.join(".bay/policy.toml"), policy(&params.signing)).unwrap();
        std::fs::create_dir_all(seed.join("scripts")).unwrap();
        support::write_executable(&seed.join("scripts/bay-check"), CHECK);
        support::write_executable(&seed.join("scripts/bay-wake"), WAKE);
        test_box.git_ok(&seed, &["add", "--all"]);
        test_box.git_ok(&seed, &["commit", "-q", "-m", "synthetic tree"]);
        test_box.git_ok(&seed, &["push", "-q", "origin", "main"]);
        let seed_tip = test_box.git_ok(&seed, &["rev-parse", "HEAD"]);

        // The registry: a runner key and a producer key for each runner, and under `all` an
        // agent key for each agent, since an agent's request is signed by its own actor.
        let keys = test_box.keys();
        let runner_keys: Vec<(PathBuf, PathBuf)> = (0..params.runners)
            .map(|runner| {
                let name = runner_name(runner);
                (
                    fresh_key(&test_box, &name),
                    fresh_key(&test_box, &format!("{name}-checks")),
                )
            })
            .collect();
        let agent_keys: Vec<Option<PathBuf>> = (0..params.agents)
            .map(|agent| {
                (params.signing == "all").then(|| fresh_key(&test_box, &agent_name(agent)))
            })
            .collect();
        let agents = if params.signing == "all" {
            agent_keys
                .iter()
                .enumerate()
                .map(|(agent, key)| key_line(&agent_name(agent), key.as_ref().unwrap()))
                .collect()
        } else {
            key_line(support::AGENT, &keys.agent)
        };
        let runners: String = runner_keys
            .iter()
            .enumerate()
            .map(|(runner, (key, _))| key_line(&runner_name(runner), key))
            .collect();
        let producers: String = runner_keys
            .iter()
            .enumerate()
            .map(|(runner, (_, key))| key_line(&format!("{}-checks", runner_name(runner)), key))
            .collect();
        test_box.enroll_files(
            &seed,
            &keys,
            &[
                ("agent", agents),
                ("runner", runners),
                ("producer", producers),
                ("registry", key_line(REGISTRAR, &keys.registry)),
            ],
        );

        // Every clone is made before a process pushes (`TestBox::clone_of`), and holds the
        // registry, so a signed run never takes the unsigned path.
        let fetch_registry = |clone: &Path| {
            test_box.git_ok(
                clone,
                &[
                    "fetch",
                    "-q",
                    "origin",
                    "refs/bay/registry:refs/bay/registry",
                ],
            );
        };
        let runner_clones: Vec<PathBuf> = runner_keys
            .iter()
            .enumerate()
            .map(|(runner, (key, producer))| {
                let name = runner_name(runner);
                let clone = test_box.clone_of(&origin, &name);
                for (setting, value) in [
                    ("bay.runnerKey", key.display().to_string()),
                    ("bay.runnerName", name.clone()),
                    ("bay.producerKey", producer.display().to_string()),
                    ("bay.producerName", format!("{name}-checks")),
                ] {
                    test_box.git_ok(&clone, &["config", setting, &value]);
                }
                fetch_registry(&clone);
                clone
            })
            .collect();
        let agent_clones: Vec<PathBuf> = (0..params.agents.div_ceil(AGENTS_PER_CLONE))
            .map(|n| {
                let clone = test_box.clone_of(&origin, &format!("agents-{n}"));
                test_box.git_ok(&clone, &["config", "bay.checkEnvironment", AGENT_DESK]);
                fetch_registry(&clone);
                clone
            })
            .collect();
        let refs_before = live_refs(&test_box, &origin).len();
        std::fs::write(&hook_log, "").unwrap();
        Self {
            test_box,
            origin,
            hook_log,
            seed_tip,
            agent_clones,
            agent_keys,
            runner_clones,
            refs_before,
        }
    }

    /// `bay -C <dir>` as `actor`, with a fleet's timings and, when given, its own agent key.
    fn bay(&self, dir: &Path, actor: &str, key: Option<&Path>) -> Command {
        let mut cmd = self.test_box.bay_process(dir);
        for name in TEST_TIMINGS {
            cmd.env_remove(name);
        }
        cmd.env("BAY_DISABLE_AUTOUPDATE", "1")
            .env("BAY_ACTOR", actor)
            .stdin(Stdio::null());
        if let Some(key) = key {
            cmd.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "bay.agentKey")
                .env("GIT_CONFIG_VALUE_0", key);
        }
        cmd
    }

    fn agent_bay(&self, agent: usize) -> Command {
        self.bay(
            &self.agent_clones[agent / AGENTS_PER_CLONE],
            &agent_name(agent),
            self.agent_keys[agent].as_deref(),
        )
    }

    /// Starts a runner on each runner clone, its output stamped into its log in `logs`.
    fn start_runners(
        &self,
        logs: &[PathBuf],
        began: Instant,
        passed: &Arc<AtomicBool>,
    ) -> Vec<Runner> {
        self.runner_clones
            .iter()
            .zip(logs)
            .enumerate()
            .map(|(runner, (clone, log))| {
                let mut child = self
                    .bay(clone, &runner_name(runner), None)
                    .args(["--json", "runner", "run"])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                let log = Arc::new(Mutex::new(std::fs::File::create(log).unwrap()));
                let readers = vec![
                    stamp(
                        child.stdout.take().unwrap(),
                        Arc::clone(&log),
                        began,
                        Arc::clone(passed),
                    ),
                    stamp(child.stderr.take().unwrap(), log, began, Arc::clone(passed)),
                ];
                Runner { child, readers }
            })
            .collect()
    }

    /// Git in `dir`: its stdout when it succeeds.
    fn git(&self, dir: &Path, args: &[&str]) -> Result<String, String> {
        let out = self.test_box.git(dir).args(args).output().unwrap();
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        } else {
            Err(format!(
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    fn origin_git(&self, args: &[&str]) -> Result<String, String> {
        let dir = format!("--git-dir={}", self.origin.display());
        let mut all = vec![dir.as_str()];
        all.extend(args);
        self.git(self.test_box.path(), &all)
    }
}

/// A runner process, and the threads that stamp its output into its log.
struct Runner {
    child: Child,
    readers: Vec<std::thread::JoinHandle<()>>,
}

impl Runner {
    /// Stops the runner, and waits a moment for its log to close.
    fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let bound = Instant::now() + LOG_CLOSE;
        while Instant::now() < bound
            && !self
                .readers
                .iter()
                .all(std::thread::JoinHandle::is_finished)
        {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Copies each line of `stream` into `log`, after the second of the run it came at, and sets
/// `passed` at the first line that says the 30 s post check passed.
fn stamp(
    stream: impl std::io::Read + Send + 'static,
    log: Arc<Mutex<std::fs::File>>,
    began: Instant,
    passed: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stream);
        let mut bytes = Vec::new();
        while reader
            .read_until(b'\n', &mut bytes)
            .is_ok_and(|read| read > 0)
        {
            let line = String::from_utf8_lossy(&bytes);
            let line = line.trim_end();
            if post_30_passed(line) {
                passed.store(true, Ordering::Relaxed);
            }
            let at = began.elapsed().as_secs_f64();
            let _ = writeln!(log.lock().unwrap(), "{at:.3} {line}");
            bytes.clear();
        }
    })
}

/// Whether a runner's journal line says the 30 s post check passed: `post post-30 on <tip>: pass`,
/// among the runs it names.
fn post_30_passed(line: &str) -> bool {
    line.split("; ").any(|run| {
        run.split_once("post post-30 on ")
            .and_then(|(_, rest)| rest.split_once(": "))
            .is_some_and(|(_, verdict)| verdict.starts_with("pass"))
    })
}

/// Runs a bay command with `--json`: its exit code, its result and its time in milliseconds.
fn run_json(mut cmd: Command, args: &[&str]) -> (i32, Value, f64) {
    let began = Instant::now();
    let out = cmd.arg("--json").args(args).output().unwrap();
    let ms = millis(began.elapsed());
    let value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        json!({
            "decision": "unreadable",
            "stdout": String::from_utf8_lossy(&out.stdout),
            "stderr": String::from_utf8_lossy(&out.stderr),
        })
    });
    (out.status.code().unwrap_or(-1), value, ms)
}

fn millis(elapsed: Duration) -> f64 {
    elapsed.as_secs_f64() * 1000.0
}

fn float(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

/// The `per_mille` nearest-rank percentile of `values`.
fn percentile(values: &[f64], per_mille: usize) -> Option<f64> {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (per_mille * sorted.len()).div_ceil(1000);
    sorted.get(rank.max(1) - 1).copied()
}

/// A percentile as the summary shows it, in `unit`.
fn shown(value: Option<f64>, unit: &str) -> String {
    match value {
        Some(value) if value.is_finite() => format!("{value:.1} {unit}"),
        Some(_) => "never".to_owned(),
        None => "none".to_owned(),
    }
}

/// A value JSON can hold: a percentile that is never is null.
fn number(value: Option<f64>) -> Value {
    value
        .filter(|value| value.is_finite())
        .map_or(Value::Null, |value| json!(value))
}

/// The p50 and p90 of `values`, as JSON.
fn spread(values: &[f64]) -> Value {
    json!({
        "samples": values.len(),
        "p50": number(percentile(values, 500)),
        "p90": number(percentile(values, 900)),
    })
}

/// The patch ids `git patch-id --stable` reads in `git <args>`'s output, with the commit of each.
fn patch_ids(fleet: &Fleet, dir: &Path, args: &[&str]) -> Vec<(String, String)> {
    let diff = fleet.test_box.git(dir).args(args).output().unwrap();
    assert!(diff.status.success(), "git {args:?}: {diff:?}");
    let mut child = fleet
        .test_box
        .git(dir)
        .args(["patch-id", "--stable"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        std::io::Write::write_all(&mut stdin, &diff.stdout).unwrap();
    });
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(patch, commit)| (patch.to_owned(), commit.to_owned()))
        .collect()
}

/// Every ref on the origin.
fn live_refs(test_box: &TestBox, origin: &Path) -> Vec<String> {
    test_box
        .git_bare(origin, &["for-each-ref", "--format=%(refname)"])
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Picks the files of the next change from the files no change in flight holds, but for an
/// overlapping change, whose first file is one a change in flight holds, and returns its number.
/// The bad change draws nothing, so the others are the same with it or without it.
fn choose(book: &mut Book, rng: &mut Rng, bad: bool) -> usize {
    let no = book.changes.len();
    let mut change = Change {
        bad,
        ..Change::default()
    };
    if !bad {
        let count = 1 + rng.below(5);
        if rng.unit() < OVERLAP {
            if book.in_flight.is_empty() {
                book.overlaps_missed += 1;
            } else {
                let at = rng.below(book.in_flight.len());
                change.files.push(*book.in_flight.keys().nth(at).unwrap());
                change.overlap = true;
            }
        }
        while change.files.len() < count {
            let file = rng.below(FILES);
            if !book.in_flight.contains_key(&file) && !change.files.contains(&file) {
                change.files.push(file);
            }
        }
        for file in &change.files {
            book.in_flight.insert(*file, no);
        }
    }
    book.changes.push(change);
    no
}

/// Ends change `no`'s hold on its files.
fn release(book: &mut Book, no: usize) {
    let files = book.changes[no].files.clone();
    for file in files {
        if book.in_flight.get(&file) == Some(&no) {
            book.in_flight.remove(&file);
        }
    }
}

/// Records why change `no` never reached a submit.
fn never_submitted(book: &Mutex<Book>, no: usize, why: String) {
    let mut book = book.lock().unwrap();
    book.changes[no].error = Some(why);
    release(&mut book, no);
}

/// Replaces the line of `file` its next edit takes, in the view.
fn edit_file(book: &Mutex<Book>, view: &Path, file: usize, no: usize) {
    let slot = {
        let mut book = book.lock().unwrap();
        let slot = book.slots[file];
        book.slots[file] += 1;
        slot
    };
    let line = (slot % (LINES / SPACING)) * SPACING;
    let path = view.join(file_path(file));
    let text = std::fs::read_to_string(&path).unwrap();
    let edited = text
        .lines()
        .enumerate()
        .fold(String::new(), |mut out, (at, old)| {
            if at == line {
                let _ = writeln!(out, "{} edited by change {no}", file_path(file));
            } else {
                let _ = writeln!(out, "{old}");
            }
            out
        });
    std::fs::write(&path, edited).unwrap();
}

/// One change by one agent, by the skill's script: start, edit and commit in the view, submit.
/// Returns the change id and the submit's result, or `None` when it never reached a submit.
fn author(fleet: &Fleet, book: &Mutex<Book>, agent: usize, no: usize) -> Option<(String, Value)> {
    let (files, bad) = {
        let book = book.lock().unwrap();
        (book.changes[no].files.clone(), book.changes[no].bad)
    };
    let paths: Vec<String> = if bad {
        vec![BROKEN_DIR.to_owned()]
    } else {
        files.iter().map(|file| file_path(*file)).collect()
    };
    let intent = format!("load change {no}");
    let mut args = vec!["start", intent.as_str(), "--paths"];
    args.extend(paths.iter().map(String::as_str));
    let (code, started, start_ms) = run_json(fleet.agent_bay(agent), &args);
    let (Some(id), Some(view)) = (
        started["change"].as_str().filter(|_| code == 0),
        started["detail"]["view"].as_str(),
    ) else {
        never_submitted(book, no, format!("start: {started}"));
        return None;
    };
    let id = id.to_owned();
    let view = PathBuf::from(view);
    let base = match fleet.git(&view, &["rev-parse", "HEAD"]) {
        Ok(base) => base,
        Err(why) => {
            never_submitted(book, no, why);
            return None;
        }
    };
    let edited = if bad {
        std::fs::write(view.join(BROKEN), "broken\n").unwrap();
        vec![BROKEN.to_owned()]
    } else {
        for file in &files {
            edit_file(book, &view, *file, no);
        }
        paths
    };
    let mut add = vec!["add", "--"];
    add.extend(edited.iter().map(String::as_str));
    if let Err(why) = fleet
        .git(&view, &add)
        .and_then(|_| fleet.git(&view, &["commit", "-q", "-m", &intent]))
    {
        never_submitted(book, no, why);
        return None;
    }
    let patch = patch_ids(fleet, &view, &["diff", "-U0", &base, "HEAD"])
        .into_iter()
        .next()
        .map(|(patch, _)| patch);
    {
        let mut book = book.lock().unwrap();
        book.by_id.insert(id.clone(), no);
        if bad {
            book.bad_id = Some(id.clone());
        }
        let change = &mut book.changes[no];
        change.id = Some(id.clone());
        change.patch = patch;
        change.start_ms = Some(start_ms);
        change.requested = Some(Instant::now());
    }
    let (_, submitted, submit_ms) = run_json(fleet.agent_bay(agent), &["merge", &id]);
    book.lock().unwrap().changes[no].submit_ms = Some(submit_ms);
    Some((id, submitted))
}

/// Waits for change `no`'s outcome as its agent does, beside its next work, and takes a refusal's
/// next step: a refusal that lifts by itself is submitted again after a moment, and one that
/// allows `refresh` takes newer main (`bay start <change>`) and is submitted again.
fn wait(
    fleet: &Fleet,
    book: &Mutex<Book>,
    agent: usize,
    no: usize,
    id: &str,
    submitted: Value,
    deadline: Instant,
) {
    let mut result = submitted;
    let mut repairs = 0;
    let outcome = loop {
        match result["decision"].as_str().unwrap_or("unreadable") {
            "merged" => break "merged".to_owned(),
            "queued" | "ok" => {
                let left = deadline.saturating_duration_since(Instant::now()).as_secs();
                if left == 0 {
                    break "queued at the end of the drain".to_owned();
                }
                result = run_json(
                    fleet.agent_bay(agent),
                    &["merge", id, &format!("--wait={left}")],
                )
                .1;
            }
            "blocked" | "failed" => {
                let reason = result["reason"].as_str().unwrap_or("none").to_owned();
                book.lock().unwrap().changes[no]
                    .refusals
                    .push(reason.clone());
                let lifts = matches!(
                    result["retry_when"].as_str(),
                    Some("by_itself" | "after_wait" | "after_new_main")
                );
                let refresh = result["allowed"]
                    .as_array()
                    .is_some_and(|allowed| allowed.iter().any(|action| action == "refresh"));
                if repairs >= MAX_REPAIRS || !(lifts || refresh) || Instant::now() >= deadline {
                    break format!("refused {reason}");
                }
                repairs += 1;
                if lifts {
                    std::thread::sleep(Duration::from_secs(2));
                } else {
                    run_json(fleet.agent_bay(agent), &["start", id]);
                }
                result = run_json(fleet.agent_bay(agent), &["merge", id]).1;
            }
            _ => break format!("unreadable result: {result}"),
        }
    };
    let mut book = book.lock().unwrap();
    book.changes[no].outcome = Some(outcome);
    release(&mut book, no);
}

/// Watches main on the origin and records when each change lands, and the undo of the bad one.
fn poll_main(fleet: &Fleet, book: &Mutex<Book>, stop: &AtomicBool) {
    let mut last = fleet.seed_tip.clone();
    while !stop.load(Ordering::Relaxed) {
        if let Ok(tip) = fleet.origin_git(&["rev-parse", "refs/heads/main"])
            && tip != last
            && let Ok(log) = fleet.origin_git(&[
                "log",
                "--format=%(trailers:key=Change-Id,valueonly,separator=%x2c)%x1f%(trailers:key=Reverts,valueonly,separator=%x2c)",
                &format!("{last}..{tip}"),
            ])
        {
            let now = Instant::now();
            let mut book = book.lock().unwrap();
            for line in log.lines() {
                let (change, reverts) = line.split_once('\x1f').unwrap_or((line, ""));
                let change = change.trim();
                if let Some(&no) = book.by_id.get(change) {
                    book.changes[no].landed.get_or_insert(now);
                    release(&mut book, no);
                }
                if book
                    .bad_id
                    .as_deref()
                    .is_some_and(|bad| reverts.contains(bad))
                {
                    book.undo.get_or_insert((change.to_owned(), now));
                }
            }
            last = tip;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Whether a red row of `bay status` names `culprit` as its culprit.
fn names_culprit(red: &Value, culprit: &str) -> bool {
    [
        &red["culprit"],
        &red["culprit"]["change"],
        &red["undo"]["culprit"],
    ]
    .iter()
    .any(|named| named.as_str() == Some(culprit))
}

/// Reads `bay status` from the first agent's clone every [`STATUS_EVERY`], timing it, and records
/// when main's red names the bad change as its culprit, and the undo it names.
fn sample_status(fleet: &Fleet, book: &Mutex<Book>, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        let next = Instant::now() + STATUS_EVERY;
        let (code, status, ms) = run_json(fleet.agent_bay(0), &["status"]);
        let now = Instant::now();
        let mut book = book.lock().unwrap();
        if code == 0 {
            book.status_load.push(ms);
        } else {
            book.status_failures.push(status.to_string());
        }
        if let Some(bad) = book.bad_id.clone() {
            for red in status["detail"]["red"].as_array().into_iter().flatten() {
                if names_culprit(red, &bad) {
                    book.culprit_named.get_or_insert(now);
                    if let Some(undo) = red["undo"]["change"].as_str() {
                        book.undo_named = Some(undo.to_owned());
                    }
                }
            }
        }
        drop(book);
        while Instant::now() < next && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// The hook time of each push in milliseconds, the calls of one push summed, and each call's
/// time by event.
fn hook_times(log: &Path) -> (Vec<f64>, BTreeMap<String, Vec<f64>>) {
    let mut pushes: BTreeMap<String, f64> = BTreeMap::new();
    let mut by_event: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for line in std::fs::read_to_string(log).unwrap_or_default().lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let [push, event, start, end, _code] = words.as_slice() else {
            continue;
        };
        let (Ok(start), Ok(end)) = (start.parse::<u64>(), end.parse::<u64>()) else {
            continue;
        };
        let ms = millis(Duration::from_nanos(end.saturating_sub(start)));
        *pushes.entry((*push).to_owned()).or_default() += ms;
        by_event.entry((*event).to_owned()).or_default().push(ms);
    }
    (pushes.into_values().collect(), by_event)
}

/// A row's verdict: a row the run does not exercise is not run, neither green nor red.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Green,
    Red,
    NotRun,
}

impl Verdict {
    fn of(green: bool) -> Self {
        if green { Self::Green } else { Self::Red }
    }

    /// The word the JSON report holds.
    fn word(self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Red => "red",
            Self::NotRun => "not_run",
        }
    }

    /// The word the summary shows.
    fn shown(self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Red => "RED",
            Self::NotRun => "not run",
        }
    }
}

/// One row of the gate.
struct Row {
    name: &'static str,
    plan: &'static str,
    value: String,
    target: &'static str,
    verdict: Verdict,
    detail: Value,
}

impl Row {
    fn json(&self) -> Value {
        json!({
            "row": self.name,
            "plan": self.plan,
            "value": self.value,
            "target": self.target,
            "verdict": self.verdict.word(),
            "detail": self.detail,
        })
    }
}

/// What the run measured beside the book.
struct Reads {
    elapsed_s: f64,
    submit_s: f64,
    status_after: Vec<f64>,
    queue_after: Vec<f64>,
    refs: Vec<String>,
    main_patches: Vec<(String, String)>,
    hook_pushes: Vec<f64>,
    hook_events: BTreeMap<String, Vec<f64>>,
    runner_undo_lines: Vec<String>,
    journal: Journal,
}

fn lead_row(book: &Book, reads: &Reads) -> Row {
    let submitted: Vec<&Change> = book
        .changes
        .iter()
        .filter(|c| c.requested.is_some())
        .collect();
    let leads: Vec<f64> = submitted
        .iter()
        .filter(|c| !c.refused())
        .map(|c| match (c.requested, c.landed) {
            (Some(requested), Some(landed)) => {
                landed.saturating_duration_since(requested).as_secs_f64()
            }
            _ => f64::INFINITY,
        })
        .collect();
    let (p50, p90) = (percentile(&leads, 500), percentile(&leads, 900));
    Row {
        name: "lead",
        plan: "S2, S3",
        value: format!("p50 {} / p90 {}", shown(p50, "s"), shown(p90, "s")),
        target: "p50 <= 20 s, p90 <= 60 s",
        verdict: Verdict::of(p50.is_some_and(|v| v <= 20.0) && p90.is_some_and(|v| v <= 60.0)),
        detail: json!({
            "p50_s": number(p50),
            "p90_s": number(p90),
            "max_s": number(percentile(&leads, 1000)),
            "landed": leads.iter().filter(|v| v.is_finite()).count(),
            "never_landed": leads.iter().filter(|v| !v.is_finite()).count(),
            "rate_per_s": float(submitted.len()) / reads.submit_s.max(1.0),
        }),
    }
}

fn hook_row(reads: &Reads) -> Row {
    let p99 = percentile(&reads.hook_pushes, 990);
    let by_event: serde_json::Map<String, Value> = reads
        .hook_events
        .iter()
        .map(|(event, times)| {
            let mut times_json = spread(times);
            times_json["p99"] = number(percentile(times, 990));
            (event.clone(), times_json)
        })
        .collect();
    Row {
        name: "hook",
        plan: "S3",
        value: format!(
            "p99 {} over {} pushes",
            shown(p99, "ms"),
            reads.hook_pushes.len()
        ),
        target: "p99 <= 200 ms",
        verdict: Verdict::of(p99.is_some_and(|v| v <= 200.0)),
        detail: json!({
            "p50_ms": number(percentile(&reads.hook_pushes, 500)),
            "p99_ms": number(p99),
            "max_ms": number(percentile(&reads.hook_pushes, 1000)),
            "pushes": reads.hook_pushes.len(),
            "by_event_ms": by_event,
        }),
    }
}

fn refs_row(fleet: &Fleet, book: &Book, reads: &Reads) -> Row {
    let landings = book.changes.iter().filter(|c| c.landed.is_some()).count()
        + usize::from(book.undo.is_some());
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    for name in &reads.refs {
        let kind: Vec<&str> = name.split('/').take(3).collect();
        *kinds.entry(kind.join("/")).or_default() += 1;
    }
    let grown = reads.refs.len().saturating_sub(fleet.refs_before);
    let per_landing = float(grown) / float(landings.max(1));
    let projected = float(fleet.refs_before) + per_landing * float(LANDINGS_AT);
    let measured = landings >= LANDINGS_AT;
    Row {
        name: "live refs",
        plan: "S4",
        value: if measured {
            format!("{} after {landings} landings", reads.refs.len())
        } else {
            format!(
                "{} after {landings} landings; {projected:.0} at {LANDINGS_AT} (model)",
                reads.refs.len()
            )
        },
        target: "< 5,000 after 30,000 landings",
        verdict: Verdict::of(if measured {
            reads.refs.len() < 5_000
        } else {
            projected < 5_000.0
        }),
        detail: json!({
            "live": reads.refs.len(),
            "before_run": fleet.refs_before,
            "landings": landings,
            "per_landing": per_landing,
            "at_30000": projected,
            "basis": if measured { "measured" } else { "model: refs per landing so far" },
            "by_kind": kinds,
        }),
    }
}

fn status_row(book: &Book, reads: &Reads) -> Row {
    let load = percentile(&book.status_load, 500);
    let after = percentile(&reads.status_after, 500);
    Row {
        name: "status",
        plan: "S4",
        value: format!(
            "p50 {} / p90 {} under load; p50 {} after",
            shown(load, "ms"),
            shown(percentile(&book.status_load, 900), "ms"),
            shown(after, "ms")
        ),
        target: "p50 <= 200 ms",
        verdict: Verdict::of(load.is_some_and(|v| v <= 200.0) && after.is_some_and(|v| v <= 200.0)),
        detail: json!({
            "under_load_ms": spread(&book.status_load),
            "after_ms": spread(&reads.status_after),
            "failures": book.status_failures.len(),
            "first_failure": book.status_failures.first(),
            "queue_after_ms": spread(&reads.queue_after),
        }),
    }
}

fn census_row(book: &Book, reads: &Reads) -> Row {
    let mut on_main: HashMap<&str, usize> = HashMap::new();
    for (patch, _) in &reads.main_patches {
        *on_main.entry(patch.as_str()).or_default() += 1;
    }
    let submitted: Vec<&Change> = book
        .changes
        .iter()
        .filter(|c| c.requested.is_some())
        .collect();
    let refused: Vec<&Change> = submitted.iter().copied().filter(|c| c.refused()).collect();
    let mut lost = Vec::new();
    let mut double = Vec::new();
    for change in &submitted {
        let count = change
            .patch
            .as_deref()
            .map_or(0, |patch| on_main.get(patch).copied().unwrap_or(0));
        let id = change.id.clone().unwrap_or_default();
        if count == 0 && !change.refused() {
            lost.push(id);
        } else if count > 1 {
            double.push(id);
        }
    }
    Row {
        name: "census",
        plan: "S10",
        value: format!(
            "{} lost, {} double of {} submitted; {} refused",
            lost.len(),
            double.len(),
            submitted.len(),
            refused.len()
        ),
        target: "0 lost and 0 double",
        verdict: Verdict::of(lost.is_empty() && double.is_empty() && !submitted.is_empty()),
        detail: json!({
            "submitted": submitted.len(),
            "lost": lost,
            "double": double,
            "refused": refused
                .iter()
                .map(|c| json!({"change": c.id, "outcome": c.outcome, "refusals": c.refusals}))
                .collect::<Vec<_>>(),
            "main_commits": reads.main_patches.len(),
        }),
    }
}

/// The bad change's row. A runner opens an automatic undo only for a culprit the evidence pins,
/// so its undo on main names the culprit too, when no status read saw the red in between. A run
/// of fewer than [`BAD_FROM_CHANGES`] arrivals (`planned` false) sends no bad change.
fn bad_change_row(book: &Book, reads: &Reads, planned: bool) -> Row {
    let bad = book.changes.iter().find(|c| c.bad);
    let landed = bad.and_then(|c| c.landed);
    let since = |at: Option<Instant>| {
        landed
            .zip(at)
            .map(|(landed, at)| at.saturating_duration_since(landed).as_secs_f64())
    };
    let undone = book.undo.as_ref().map(|undo| undo.1);
    let named_s = since(book.culprit_named.or(undone));
    let undone_s = since(undone);
    Row {
        name: "bad change",
        plan: "S2",
        value: match (bad, landed, named_s, undone_s) {
            (None, ..) if !planned => {
                format!("not sent: fewer than {BAD_FROM_CHANGES} changes")
            }
            (None, ..) => "never sent: the 30 s post check never passed on main".to_owned(),
            (_, None, ..) => "the bad change never landed".to_owned(),
            (_, _, None, _) => "bisection never named it".to_owned(),
            (_, _, Some(named), None) => format!("named at {named:.0} s; never undone"),
            (_, _, Some(named), Some(undone)) => {
                format!("named at {named:.0} s; undone at {undone:.0} s, after landing")
            }
        },
        target: "named by bisection and undone by a runner",
        verdict: if bad.is_none() && !planned {
            Verdict::NotRun
        } else {
            Verdict::of(named_s.is_some() && undone_s.is_some())
        },
        detail: json!({
            "change": bad.and_then(|c| c.id.clone()),
            "post_30_first_pass_s": number(reads.journal.post_30_passed_s),
            "outcome": bad.and_then(|c| c.outcome.clone()),
            "culprit_named_s": number(named_s),
            "culprit_seen_in_status": book.culprit_named.is_some(),
            "undo": book.undo.as_ref().map(|undo| undo.0.clone()),
            "undo_named": book.undo_named,
            "undone_s": number(undone_s),
            "undo_after_s": UNDO_AFTER_S,
            "runner_undo_lines": reads.runner_undo_lines,
        }),
    }
}

/// The one-screen summary.
fn summary(params: &Params, book: &Book, reads: &Reads, rows: &[Row], split: &str) -> String {
    let submitted = book
        .changes
        .iter()
        .filter(|c| c.requested.is_some())
        .count();
    let errors = book.changes.iter().filter(|c| c.error.is_some()).count();
    let overlaps = book.changes.iter().filter(|c| c.overlap).count();
    let (object_format, ref_format) = support::formats();
    let mut text = String::new();
    let _ = writeln!(
        text,
        "# bay load test (G920)\n\nbay {} ({} build), {object_format}/{ref_format}, signing = {}.",
        env!("CARGO_PKG_VERSION"),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        params.signing
    );
    let _ = writeln!(
        text,
        "{submitted} changes submitted over {:.0} s ({:.2} a second; rate {}, peak {}), {} agents, {} runners, {overlaps} overlapping, {errors} never submitted; the run took {:.0} s.\n",
        reads.submit_s,
        float(submitted) / reads.submit_s.max(1.0),
        params.rate,
        params.peak,
        params.agents,
        params.runners,
        reads.elapsed_s
    );
    let _ = writeln!(text, "| row | value | target | verdict | plan |");
    let _ = writeln!(text, "|---|---|---|---|---|");
    for row in rows {
        let _ = writeln!(
            text,
            "| {} | {} | {} | {} | {} |",
            row.name,
            row.value,
            row.target,
            row.verdict.shown(),
            row.plan
        );
    }
    text.push('\n');
    text.push_str(split);
    text
}

/// The report's `run` block.
fn run_json_block(book: &Book, reads: &Reads, arrivals: usize) -> Value {
    let errors: Vec<&String> = book
        .changes
        .iter()
        .filter_map(|c| c.error.as_ref())
        .collect();
    let mut refusals: BTreeMap<&str, usize> = BTreeMap::new();
    for reason in book.changes.iter().flat_map(|c| c.refusals.iter()) {
        *refusals.entry(reason.as_str()).or_default() += 1;
    }
    let starts: Vec<f64> = book.changes.iter().filter_map(|c| c.start_ms).collect();
    let submits: Vec<f64> = book.changes.iter().filter_map(|c| c.submit_ms).collect();
    json!({
        "elapsed_s": reads.elapsed_s,
        "submit_s": reads.submit_s,
        "arrivals": arrivals,
        "submitted": book.changes.iter().filter(|c| c.requested.is_some()).count(),
        "never_submitted": errors.len(),
        "first_errors": errors.iter().take(5).collect::<Vec<_>>(),
        "overlapping": book.changes.iter().filter(|c| c.overlap).count(),
        "overlaps_missed": book.overlaps_missed,
        "refusals": refusals,
        "start_ms": spread(&starts),
        "submit_ms": spread(&submits),
    })
}

/// Each change's times in seconds from the start, and what became of it.
fn timeline(book: &Book, journal: &Journal, began: Instant) -> Vec<Value> {
    let at = |instant: Option<Instant>| {
        instant.map(|instant| instant.saturating_duration_since(began).as_secs_f64())
    };
    book.changes
        .iter()
        .enumerate()
        .map(|(no, change)| {
            let landing = change
                .id
                .as_deref()
                .and_then(|id| journal.landed.get(id))
                .map_or(Value::Null, |landed| {
                    let split = landed.split(change, began);
                    json!({
                        "runner": runner_name(landed.runner),
                        "head": landed.head,
                        "member": landed.member,
                        "batch": journal.batch(&landed.head),
                        "batch_note": landed.batch_note,
                        "head_taken_s": number(split.head_taken),
                        "queue_wait_s": number(split.queue_wait),
                        "visit_s": landed.visit_s,
                        "check_s": number(split.check),
                        "publish_s": number(split.publish),
                        "head_cycle_s": number(split.head_cycle),
                        "pushes_refused": journal.refused.get(&landed.head).copied().unwrap_or(0),
                    })
                });
            json!({
                "no": no,
                "change": change.id,
                "files": change.files.len(),
                "overlap": change.overlap,
                "bad": change.bad,
                "requested_s": at(change.requested),
                "landed_s": at(change.landed),
                "outcome": change.outcome,
                "refusals": change.refusals,
                "error": change.error,
                "landing": landing,
            })
        })
        .collect()
}

/// Waits, after every change ended, for the bad change's undo: it comes after its post check, its
/// bisection and the wait, and the run gives it a bound.
fn await_undo(book: &Mutex<Book>) {
    loop {
        let book = book.lock().unwrap();
        let done = match book.changes.iter().find(|c| c.bad).and_then(|c| c.landed) {
            None => true,
            Some(landed) => {
                book.undo.is_some()
                    || landed.elapsed() > Duration::from_secs(UNDO_BOUND_S)
                    || book.culprit_named.is_some_and(|named| {
                        named.elapsed() > Duration::from_secs(UNDO_AFTER_S + UNDO_NAMED_BOUND_S)
                    })
            }
        };
        drop(book);
        if done {
            return;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Times `READS_AFTER` reads of `bay <args>` from the first agent's clone.
fn timed_reads(fleet: &Fleet, args: &[&str]) -> Vec<f64> {
    (0..READS_AFTER)
        .filter_map(|_| {
            let (code, _, ms) = run_json(fleet.agent_bay(0), args);
            (code == 0).then_some(ms)
        })
        .collect()
}

/// One landing as a runner's log tells it.
#[derive(Clone)]
struct Landed {
    runner: usize,
    /// The second of the run the head's line came at.
    at_s: f64,
    /// The operation whose visit landed it: its own, or the head of the batch it joined.
    head: String,
    member: bool,
    /// The head's visit, and each of its steps, in whole seconds as the line gives them.
    visit_s: Option<f64>,
    steps: BTreeMap<String, f64>,
    /// The head's last batch line before it landed: the batch's size and why it stopped, or why
    /// it had none.
    batch_note: Option<String>,
}

/// Where one landed change's lead went, each in seconds: when the head was taken, in seconds of
/// the run, and how long each part took. The queue wait and the head cycle add up to the lead.
struct Split {
    head_taken: Option<f64>,
    queue_wait: Option<f64>,
    head_cycle: Option<f64>,
    check: Option<f64>,
    publish: Option<f64>,
}

impl Landed {
    /// Where `change`'s lead went, when this is its landing: its wait for the queue's head, and
    /// the head's cycle from its taking to the landing on main, with the visit's check and publish.
    /// A landing's line comes after main moved, so the cycle ends at main.
    fn split(&self, change: &Change, began: Instant) -> Split {
        let at = |instant: Option<Instant>| {
            instant.map(|instant| instant.saturating_duration_since(began).as_secs_f64())
        };
        let head_taken = self.visit_s.map(|visit| self.at_s - visit);
        let step = |name: &str| {
            (!self.steps.is_empty()).then(|| self.steps.get(name).copied().unwrap_or(0.0))
        };
        Split {
            head_taken,
            queue_wait: head_taken
                .zip(at(change.requested))
                .map(|(taken, requested)| taken - requested),
            head_cycle: head_taken
                .zip(at(change.landed))
                .map(|(taken, seen)| seen - taken),
            check: step("check"),
            publish: step("publish"),
        }
    }
}

/// What the runners' logs say, for the lead split.
#[derive(Default)]
struct Journal {
    /// Each change's landing, by change id, and each landing head's, by operation.
    landed: HashMap<String, Landed>,
    heads: HashMap<String, Landed>,
    /// The changes each head landed beside itself.
    members: HashMap<String, usize>,
    /// Why each batch plan stopped, and why each visit that planned none had none.
    stopped: BTreeMap<String, usize>,
    no_batch: BTreeMap<String, usize>,
    cuts: usize,
    /// The landing pushes the origin refused, by operation.
    refused: HashMap<String, usize>,
    post_30_passed_s: Option<f64>,
}

impl Journal {
    /// Reads the runners' logs as one, in the order of their stamps: a batch line comes before
    /// the landing it is about, a batch's members come after their head, and a change's first
    /// `merged` line is its landing, since a later visit of a landed change says `merged` again.
    fn read(logs: &[PathBuf]) -> Self {
        let texts: Vec<String> = logs
            .iter()
            .map(|log| std::fs::read_to_string(log).unwrap_or_default())
            .collect();
        let mut lines: Vec<(f64, usize, &str)> = Vec::new();
        for (runner, text) in texts.iter().enumerate() {
            for raw in text.lines() {
                if let Some((stamp, line)) = raw.split_once(' ')
                    && let (Ok(at_s), Some(line)) =
                        (stamp.parse::<f64>(), line.strip_prefix("bay runner: "))
                {
                    lines.push((at_s, runner, line));
                }
            }
        }
        lines.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut journal = Self::default();
        let mut notes = HashMap::new();
        for (at_s, runner, line) in lines {
            journal.read_line(runner, at_s, line, &mut notes);
        }
        journal
    }

    /// Reads one journal line of `runner`, stamped `at_s`. `notes` holds each operation's last
    /// batch line until it lands.
    fn read_line<'a>(
        &mut self,
        runner: usize,
        at_s: f64,
        line: &'a str,
        notes: &mut HashMap<&'a str, String>,
    ) {
        if post_30_passed(line) {
            self.post_30_passed_s.get_or_insert(at_s);
            return;
        }
        let Some((op, rest)) = line.split_once(' ') else {
            return;
        };
        if let Some(plan) = rest.strip_prefix("batch of ") {
            if plan.contains(" cut to ") {
                self.cuts += 1;
            } else if let Some((size, stopped)) = plan.split_once(", stopped: ") {
                let reason: String = stopped
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                let how = size.split_once(' ').map_or(size, |(_, how)| how);
                *self.stopped.entry(format!("{reason} ({how})")).or_default() += 1;
                notes.insert(op, format!("batch of {size}, stopped: {reason}"));
            }
        } else if let Some(why) = rest.strip_prefix("no batch: ") {
            *self.no_batch.entry(why.to_owned()).or_default() += 1;
            notes.insert(op, format!("no batch: {why}"));
        } else if rest.starts_with("landing push refused") {
            *self.refused.entry(op.to_owned()).or_default() += 1;
        } else if let Some((change, _)) = op.rsplit_once('.')
            && !self.landed.contains_key(change)
        {
            let words = rest
                .strip_prefix('[')
                .and_then(|named| named.split_once("] "))
                .map_or(rest, |(_, words)| words);
            let Some(after) = words.strip_prefix("merged") else {
                return;
            };
            if let Some(head) = after
                .strip_prefix(", in ")
                .and_then(|batch| batch.strip_suffix("'s batch"))
            {
                *self.members.entry(head.to_owned()).or_default() += 1;
                if let Some(landed) = self.heads.get(head) {
                    let joined = Landed {
                        member: true,
                        ..landed.clone()
                    };
                    self.landed.insert(change.to_owned(), joined);
                }
            } else {
                let (visit_s, steps) = visit_times(after);
                let landed = Landed {
                    runner,
                    at_s,
                    head: op.to_owned(),
                    member: false,
                    visit_s,
                    steps,
                    batch_note: notes.remove(op),
                };
                self.heads.insert(op.to_owned(), landed.clone());
                self.landed.insert(change.to_owned(), landed);
            }
        }
    }

    /// The size of the batch `head` landed: itself and its members.
    fn batch(&self, head: &str) -> usize {
        1 + self.members.get(head).copied().unwrap_or(0)
    }
}

/// A visit's seconds and the seconds of each step, from the rest of its journal line after the
/// decision: `(<N>s: <step> <S>s, <step> <S>s [<git>]; git: …)`, or `(<N>s; git: …)` for one step.
fn visit_times(tail: &str) -> (Option<f64>, BTreeMap<String, f64>) {
    let mut steps = BTreeMap::new();
    let Some((_, inner)) = tail.split_once(" (") else {
        return (None, steps);
    };
    let digits: String = inner.chars().take_while(char::is_ascii_digit).collect();
    let Ok(seconds) = digits.parse::<f64>() else {
        return (None, steps);
    };
    let Some(list) = inner[digits.len()..].strip_prefix("s: ") else {
        return (Some(seconds), steps);
    };
    let mut depth = 0_usize;
    let mut start = 0;
    let mut items = Vec::new();
    for (at, c) in list.char_indices() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' if depth > 0 => depth -= 1,
            ',' | ';' | ')' if depth == 0 => {
                items.push(&list[start..at]);
                start = at + 1;
                if c != ',' {
                    break;
                }
            }
            _ => {}
        }
    }
    for item in items {
        let mut words = item.split_whitespace();
        if let (Some(name), Some(secs)) = (words.next(), words.next())
            && let Some(Ok(secs)) = secs.strip_suffix('s').map(str::parse::<f64>)
        {
            *steps.entry(name.to_owned()).or_default() += secs;
        }
    }
    (Some(seconds), steps)
}

/// The parts of the landed changes' leads, in seconds: the queue wait, the head cycle, the head's
/// visit, its check and its publish; and how many landed changes no log tells of.
fn lead_parts(book: &Book, journal: &Journal, began: Instant) -> ([Vec<f64>; 5], usize) {
    let mut parts: [Vec<f64>; 5] = Default::default();
    let mut unread = 0;
    for change in book.changes.iter().filter(|c| c.landed.is_some()) {
        let Some(landed) = change.id.as_deref().and_then(|id| journal.landed.get(id)) else {
            unread += 1;
            continue;
        };
        let split = landed.split(change, began);
        let values = [
            split.queue_wait,
            split.head_cycle,
            landed.visit_s,
            split.check,
            split.publish,
        ];
        for (part, value) in parts.iter_mut().zip(values) {
            part.extend(value);
        }
    }
    (parts, unread)
}

/// The run's lead split, as JSON and as the summary's lines: where the landed changes' leads
/// went, and how the runners batched.
fn lead_split(book: &Book, journal: &Journal, began: Instant) -> (Value, String) {
    let (parts, unread) = lead_parts(book, journal, began);
    let refused: usize = journal.refused.values().sum();
    let mut sizes: BTreeMap<usize, usize> = BTreeMap::new();
    for head in journal.heads.keys() {
        *sizes.entry(journal.batch(head)).or_default() += 1;
    }
    let lone = sizes.get(&1).copied().unwrap_or(0);
    let joined = sizes.iter().filter(|(size, _)| **size > 1);
    let batches: usize = joined.clone().map(|(_, heads)| heads).sum();
    let in_batches: usize = joined.map(|(size, heads)| size * heads).sum();
    let mean = float(lone + in_batches) / float(journal.heads.len().max(1));
    let mut lines: Vec<f64> = journal.heads.values().map(|landed| landed.at_s).collect();
    lines.sort_by(f64::total_cmp);
    let between: Vec<f64> = lines.windows(2).map(|pair| pair[1] - pair[0]).collect();
    let mut by_runner: BTreeMap<String, usize> = BTreeMap::new();
    for landed in journal.landed.values() {
        *by_runner.entry(runner_name(landed.runner)).or_default() += 1;
    }
    let mut value = json!({
        "landings": lone + in_batches,
        "lone": lone,
        "in_batches": in_batches,
        "batches": batches,
        "mean_batch": mean,
        "batch_sizes": sizes,
        "stopped": journal.stopped,
        "no_batch": journal.no_batch,
        "batches_cut": journal.cuts,
        "landing_pushes_refused": refused,
        "by_runner": by_runner,
        "landed_not_in_a_log": unread,
        "between_landings_s": spread(&between),
    });
    let names = [
        "queue_wait_s",
        "head_cycle_s",
        "head_visit_s",
        "check_s",
        "publish_s",
    ];
    for (name, part) in names.into_iter().zip(&parts) {
        value[name] = spread(part);
    }
    let histogram = |counts: &BTreeMap<String, usize>| {
        if counts.is_empty() {
            "none".to_owned()
        } else {
            let said: Vec<String> = counts.iter().map(|(why, n)| format!("{why} {n}")).collect();
            said.join(", ")
        }
    };
    let mut text = String::new();
    let _ = writeln!(
        text,
        "Lead split, from the runners' logs: {} landings, {lone} lone and {in_batches} in {batches} batches (mean batch {mean:.2}); {} landing pushes refused, {} batches cut, {unread} landed changes in no log.\n",
        lone + in_batches,
        refused,
        journal.cuts,
    );
    let _ = writeln!(text, "| part of the lead | p50 | p90 |\n|---|---|---|");
    let labels = [
        "request to head taken (queue wait)",
        "head taken to landed on main (head cycle)",
        "the head's visit, by its line",
        "its check",
        "its publish",
    ];
    for (label, part) in labels
        .into_iter()
        .zip(&parts)
        .chain([("between two landings", &between)])
    {
        let _ = writeln!(
            text,
            "| {label} | {} | {} |",
            shown(percentile(part, 500), "s"),
            shown(percentile(part, 900), "s")
        );
    }
    let _ = writeln!(
        text,
        "\nBatches stopped: {}. No batch: {}.",
        histogram(&journal.stopped),
        histogram(&journal.no_batch)
    );
    (value, text)
}

/// The last lines of the runners' logs that speak of an undo.
fn runner_undo_lines(logs: &[PathBuf]) -> Vec<String> {
    let mut lines: Vec<String> = logs
        .iter()
        .flat_map(|log| {
            std::fs::read_to_string(log)
                .unwrap_or_default()
                .lines()
                .filter(|line| line.contains("undo"))
                .map(|line| line.chars().take(400).collect::<String>())
                .collect::<Vec<_>>()
        })
        .collect();
    let keep = lines.len().saturating_sub(5);
    lines.split_off(keep)
}

#[test]
#[ignore = "an instrument: run by hand with BAY_LOAD_* (the scale plan § 5)"]
#[allow(clippy::too_many_lines)]
fn a_fleet_at_volume_lands_every_change_once_in_time() {
    let params = Params::from_env();
    let fleet = Fleet::new(&params);
    let mut rng = Rng(0x0920_5ca1_e0f1_ee75);
    let times = arrivals(&params, &mut rng);
    let bad_from = (times.len() >= BAD_FROM_CHANGES).then_some(times.len() / 3);
    let book = Mutex::new(Book {
        slots: vec![0; FILES],
        ..Book::default()
    });
    let stop = AtomicBool::new(false);
    if let Some(parent) = params.out.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    // The runners' logs go beside the report, so they outlive the box.
    let logs: Vec<PathBuf> = (0..params.runners)
        .map(|runner| {
            params
                .out
                .with_extension(format!("{}.log", runner_name(runner)))
        })
        .collect();
    let post_30_passed = Arc::new(AtomicBool::new(false));
    let began = Instant::now();
    let runners = fleet.start_runners(&logs, began, &post_30_passed);
    let last_arrival = Duration::from_secs_f64(times.last().copied().unwrap_or(0.0));
    let deadline = began + last_arrival + Duration::from_secs(DRAIN_S);

    let (jobs, inbox) = mpsc::channel::<usize>();
    let inbox = Mutex::new(inbox);
    std::thread::scope(|scope| {
        scope.spawn(|| poll_main(&fleet, &book, &stop));
        scope.spawn(|| sample_status(&fleet, &book, &stop));
        for agent in 0..params.agents {
            let (fleet, book, inbox) = (&fleet, &book, &inbox);
            scope.spawn(move || {
                loop {
                    let next = inbox.lock().unwrap().recv();
                    let Ok(no) = next else { break };
                    if Instant::now() >= deadline {
                        never_submitted(book, no, "not started before the drain ended".to_owned());
                        continue;
                    }
                    if let Some((id, submitted)) = author(fleet, book, agent, no) {
                        scope.spawn(move || wait(fleet, book, agent, no, &id, submitted, deadline));
                    }
                }
            });
        }
        // The bad change goes at the first arrival from a third of the way in once the 30 s post
        // check has passed on main, so bisection has a green commit below it.
        let mut bad_sent = bad_from.is_none();
        for (index, at) in times.iter().enumerate() {
            let due = began + Duration::from_secs_f64(*at);
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
            if !bad_sent
                && bad_from.is_some_and(|from| index >= from)
                && post_30_passed.load(Ordering::Relaxed)
            {
                jobs.send(choose(&mut book.lock().unwrap(), &mut rng, true))
                    .unwrap();
                bad_sent = true;
            }
            jobs.send(choose(&mut book.lock().unwrap(), &mut rng, false))
                .unwrap();
        }
        let bad_bound = began + last_arrival + Duration::from_secs(BAD_WAITS_S);
        while !bad_sent && Instant::now() < bad_bound {
            if post_30_passed.load(Ordering::Relaxed) {
                jobs.send(choose(&mut book.lock().unwrap(), &mut rng, true))
                    .unwrap();
                bad_sent = true;
            } else {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        drop(jobs);
        while Instant::now() < deadline {
            let all_ended = book.lock().unwrap().changes.iter().all(Change::ended);
            if all_ended {
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        await_undo(&book);
        stop.store(true, Ordering::Relaxed);
    });
    let elapsed_s = began.elapsed().as_secs_f64();

    // Reads after the run, while the runners still run their post work.
    let status_after = timed_reads(&fleet, &["status"]);
    let queue_after = timed_reads(&fleet, &["queue"]);
    for runner in runners {
        runner.stop();
    }
    let hook_kept = params.out.with_extension("hook-times.log");
    std::fs::copy(&fleet.hook_log, &hook_kept).unwrap();
    let book = book.into_inner().unwrap();
    let submit_s = book
        .changes
        .iter()
        .filter_map(|c| c.requested)
        .max()
        .map_or(0.0, |last| {
            last.saturating_duration_since(began).as_secs_f64()
        });
    // Both sides read their patch ids in their own repository, in its object format.
    let range = format!("{}..main", fleet.seed_tip);
    let main_patches = patch_ids(
        &fleet,
        &fleet.origin,
        &[
            "log",
            "--first-parent",
            "--diff-merges=first-parent",
            "-p",
            "-U0",
            &range,
        ],
    );
    let (hook_pushes, hook_events) = hook_times(&fleet.hook_log);
    let reads = Reads {
        elapsed_s,
        submit_s,
        status_after,
        queue_after,
        refs: live_refs(&fleet.test_box, &fleet.origin),
        main_patches,
        hook_pushes,
        hook_events,
        runner_undo_lines: runner_undo_lines(&logs),
        journal: Journal::read(&logs),
    };
    let rows = [
        lead_row(&book, &reads),
        hook_row(&reads),
        refs_row(&fleet, &book, &reads),
        status_row(&book, &reads),
        census_row(&book, &reads),
        bad_change_row(&book, &reads, bad_from.is_some()),
    ];
    let (split, split_text) = lead_split(&book, &reads.journal, began);
    let summary = summary(&params, &book, &reads, &rows, &split_text);
    let report = json!({
        "schema": "bay.load-report/1",
        "bay": {
            "version": env!("CARGO_PKG_VERSION"),
            "build": if cfg!(debug_assertions) { "debug" } else { "release" },
        },
        "params": params.json(),
        "run": run_json_block(&book, &reads, times.len()),
        "rows": rows.iter().map(Row::json).collect::<Vec<_>>(),
        "lead_split": split,
        "logs": {"runners": logs, "hook": hook_kept},
        "changes": timeline(&book, &reads.journal, began),
    });
    std::fs::write(&params.out, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    std::fs::write(params.out.with_extension("md"), &summary).unwrap();
    eprintln!("{summary}\nreport: {}", params.out.display());

    let mut red: Vec<String> = rows
        .iter()
        .filter(|row| row.verdict == Verdict::Red)
        .map(|row| format!("{} ({}): {}", row.name, row.plan, row.value))
        .collect();
    let errors: Vec<&String> = book
        .changes
        .iter()
        .filter_map(|c| c.error.as_ref())
        .collect();
    if let Some(first) = errors.first() {
        red.push(format!(
            "{} changes never submitted, the first: {first}",
            errors.len()
        ));
    }
    assert!(red.is_empty(), "red rows:\n{}\n\n{summary}", red.join("\n"));
}
