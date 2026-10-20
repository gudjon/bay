//! G171's cost harness: Git spawns and Git time per bay command, for the Git on `PATH`, per
//! object and ref format and repository size. Numbers before any optimisation.
//!
//! ```text
//! git_cost [--bay <path>] [--commits <n>] [--files <n>] [--formats <object>/<ref>,...]
//!          [--keep] [--json]
//! ```
//!
//! For each format (default `sha1/files,sha256/reftable`, the two `BAY_TEST_FORMATS` runs) it
//! builds a scratch box as the integration tests do (a temp `HOME`, a global Git config that
//! holds only the box's choice of no check sandbox), a bare trunk host, a work repository whose
//! trunk holds `--files` files over `--commits` commits and one required check, and an
//! executor clone. Then it runs a fixed scenario (`start`, `land`, `executor run --once`,
//! `status`, `status --fleet`, `watch`, `evidence missing`), each with `BAY_TEST_GIT_TRACE`
//! naming a file of its own, and reports per command its git spawns, their total time and
//! its wall time. It prints one table, or with `--json` one JSON document.
//!
//! `--bay` defaults to `target/debug/bay`. It must be a test build: a binary
//! `scripts/release.sh` built writes no trace, and the harness says so. A spawn's time is as
//! bay sees it, from spawn to the exit bay notices, so it includes bay's wait between polls.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

use serde_json::{Value, json};

const USAGE: &str = "usage: git_cost [--bay <path>] [--commits <n>] [--files <n>] \
                     [--formats <object>/<ref>,...] [--keep] [--json]";

/// The one required check of the scratch trunk, as the integration tests write it.
const POLICY: &str = "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 3600\n";

/// The check adapter: one passing unit on the candidate it was asked about.
const CHECK: &str = r#"#!/bin/sh
req=$(cat)
commit=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"}.*/\1/p')
tree=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"}.*/\2/p')
printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"discovered":1,"selected":1,"completed":1},"checks":[{"id":"t1","verdict":"pass"}]}' "$commit" "$tree"
"#;

struct Options {
    bay: PathBuf,
    commits: usize,
    files: usize,
    formats: Vec<(String, String)>,
    keep: bool,
    json: bool,
}

fn main() -> ExitCode {
    let options = match parse(std::env::args().skip(1).collect()) {
        Ok(options) => options,
        Err(why) => {
            eprintln!("git_cost: {why}\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    match run(&options) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(why) => {
            eprintln!("git_cost: {why}");
            ExitCode::FAILURE
        }
    }
}

fn parse(args: Vec<String>) -> Result<Options, String> {
    let mut options = Options {
        bay: Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/bay"),
        commits: 100,
        files: 1000,
        formats: Vec::new(),
        keep: false,
        json: false,
    };
    let mut formats = "sha1/files,sha256/reftable".to_owned();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        let count = |text: String| match text.parse::<usize>() {
            Ok(n) if n > 0 => Ok(n),
            _ => Err(format!("{text:?} is not a positive count")),
        };
        match arg.as_str() {
            "--bay" => options.bay = PathBuf::from(value()?),
            "--commits" => options.commits = count(value()?)?,
            "--files" => options.files = count(value()?)?,
            "--formats" => formats = value()?,
            "--keep" => options.keep = true,
            "--json" => options.json = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    for format in formats.split(',') {
        let (object, refs) = format
            .split_once('/')
            .ok_or_else(|| format!("{format} is not <object>/<ref>"))?;
        options.formats.push((object.to_owned(), refs.to_owned()));
    }
    if !options.bay.is_file() {
        return Err(format!(
            "no bay at {} (build it, or name it with --bay)",
            options.bay.display()
        ));
    }
    Ok(options)
}

fn run(options: &Options) -> Result<String, String> {
    let git_version = output(Command::new("git").arg("--version"))?;
    let bay_version = output(Command::new(&options.bay).arg("--version"))?;
    let mut runs = Vec::new();
    for (object, refs) in &options.formats {
        let scratch = Scratch::new(options.keep)?;
        let started = Instant::now();
        let place = setup(&scratch, options, object, refs)?;
        let setup_s = started.elapsed().as_secs();
        let steps = scenario(&scratch, &options.bay, &place)?;
        if steps.iter().all(|step| step.spawns == 0) {
            return Err(format!(
                "{} wrote no trace: it is a release build, or not one with G171's trace",
                options.bay.display()
            ));
        }
        runs.push(json!({
            "object_format": object,
            "ref_format": refs,
            "commits": options.commits,
            "files": options.files,
            "setup_s": setup_s,
            "scratch": options.keep.then(|| scratch.root().display().to_string()),
            "steps": steps.iter().map(Step::json).collect::<Vec<_>>(),
        }));
    }
    let report = json!({
        "harness": "git_cost",
        "git": git_version,
        "bay": bay_version,
        "os": std::env::consts::OS,
        "runs": runs,
    });
    if options.json {
        return serde_json::to_string_pretty(&report).map_err(|error| error.to_string());
    }
    Ok(table(&report))
}

/// A scratch box: its directory, `HOME` and global Git config. Removed on drop unless kept.
struct Scratch {
    root: PathBuf,
    _guard: Option<tempfile::TempDir>,
}

impl Scratch {
    fn new(keep: bool) -> Result<Self, String> {
        let dir = tempfile::Builder::new()
            .prefix("bay-git-cost-")
            .tempdir()
            .map_err(|error| error.to_string())?;
        std::fs::create_dir_all(dir.path().join("home")).map_err(|error| error.to_string())?;
        std::fs::create_dir_all(dir.path().join("trace")).map_err(|error| error.to_string())?;
        // The box's choice to run checks with no sandbox (ruling R14), as in the tests: a
        // sandbox's cost is not Git's.
        std::fs::write(
            dir.path().join("global.gitconfig"),
            "[bay]\n\texecutorSandbox = none\n",
        )
        .map_err(|error| error.to_string())?;
        if keep {
            return Ok(Self {
                root: dir.keep(),
                _guard: None,
            });
        }
        Ok(Self {
            root: dir.path().to_path_buf(),
            _guard: Some(dir),
        })
    }

    fn root(&self) -> &Path {
        &self.root
    }

    /// `program` with this box's environment and nothing else of the caller's but `PATH`.
    fn command(&self, program: &Path) -> Command {
        let mut cmd = Command::new(program);
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.root().join("home"))
            .env("GIT_CONFIG_GLOBAL", self.root().join("global.gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", self.root())
            .env("GIT_AUTHOR_NAME", "bay cost")
            .env("GIT_AUTHOR_EMAIL", "cost@bay.invalid")
            .env("GIT_COMMITTER_NAME", "bay cost")
            .env("GIT_COMMITTER_EMAIL", "cost@bay.invalid")
            .env("LANG", "C.UTF-8")
            .env("BAY_DISABLE_AUTOUPDATE", "1");
        cmd
    }

    fn git(&self, dir: &Path, args: &[&str]) -> Result<String, String> {
        let mut cmd = self.command(Path::new("git"));
        cmd.current_dir(dir).args(args);
        output(&mut cmd)
    }
}

/// The repositories of one run.
struct Place {
    repo: PathBuf,
    executor: PathBuf,
}

fn setup(scratch: &Scratch, options: &Options, object: &str, refs: &str) -> Result<Place, String> {
    let root = scratch.root();
    let remote = root.join("trunk.git");
    let repo = root.join("repo");
    let formats = [
        format!("--object-format={object}"),
        format!("--ref-format={refs}"),
    ];
    let init = |dir: &Path, bare: bool| -> Result<String, String> {
        let mut args = vec![
            "init",
            "-q",
            "-b",
            "main",
            formats[0].as_str(),
            formats[1].as_str(),
        ];
        if bare {
            args.push("--bare");
        }
        let dir = dir.to_str().ok_or("a scratch path is not UTF-8")?;
        args.push(dir);
        scratch.git(root, &args)
    };
    init(&remote, true)?;
    init(&repo, false)?;
    let url = remote.to_str().ok_or("a scratch path is not UTF-8")?;
    scratch.git(&repo, &["remote", "add", "origin", url])?;
    scratch.git(
        &repo,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    )?;
    let stream = history(options.commits, options.files);
    let mut cmd = scratch.command(Path::new("git"));
    cmd.current_dir(&repo).args(["fast-import", "--quiet"]);
    output_with(&mut cmd, &stream)?;
    scratch.git(&repo, &["reset", "-q", "--hard"])?;
    scratch.git(&repo, &["push", "-q", "origin", "main"])?;
    let executor = root.join("executor");
    let to = executor.to_str().ok_or("a scratch path is not UTF-8")?;
    scratch.git(root, &["clone", "-q", url, to])?;
    Ok(Place { repo, executor })
}

/// A `git fast-import` stream: a first commit with `files` files, the check and the config,
/// then `commits - 1` commits that each change one file.
fn history(commits: usize, files: usize) -> Vec<u8> {
    let mut stream = Vec::new();
    let blob = |stream: &mut Vec<u8>, mode: &str, path: &str, text: &str| {
        let _ = writeln!(
            stream,
            "M {mode} inline {path}\ndata {}\n{text}",
            text.len()
        );
    };
    let header = |stream: &mut Vec<u8>, n: usize| {
        let message = format!("commit {n}");
        let _ = writeln!(
            stream,
            "commit refs/heads/main\ncommitter bay cost <cost@bay.invalid> {} +0000\ndata {}\n{message}",
            1_700_000_000 + n,
            message.len()
        );
    };
    header(&mut stream, 1);
    blob(
        &mut stream,
        "100644",
        ".bay/config.toml",
        "trunk = \"main\"\n",
    );
    blob(&mut stream, "100644", ".bay/policy.toml", POLICY);
    blob(&mut stream, "100755", "scripts/bay-check", CHECK);
    for file in 0..files {
        let text = format!("pub fn f{file}() -> usize {{\n    {file}\n}}\n");
        blob(&mut stream, "100644", &path_of(file), &text);
    }
    for n in 2..=commits {
        header(&mut stream, n);
        let file = n % files;
        let text = format!("pub fn f{file}() -> usize {{\n    {n}\n}}\n");
        blob(&mut stream, "100644", &path_of(file), &text);
    }
    stream
}

/// File `n`'s path: a hundred files to a directory.
fn path_of(n: usize) -> String {
    format!("src/d{:05}/f{n:07}.rs", n / 100)
}

/// One bay command of the scenario, with its trace read.
struct Step {
    name: &'static str,
    exit: i32,
    decision: String,
    wall_us: u64,
    spawns: usize,
    git_us: u64,
    processes: usize,
    by_subcommand: BTreeMap<String, (usize, u64)>,
}

impl Step {
    fn json(&self) -> Value {
        let subs: serde_json::Map<String, Value> = self
            .by_subcommand
            .iter()
            .map(|(sub, (spawns, us))| (sub.clone(), json!({"spawns": spawns, "us": us})))
            .collect();
        json!({
            "command": self.name,
            "exit": self.exit,
            "decision": self.decision,
            "git_spawns": self.spawns,
            "git_us": self.git_us,
            "wall_us": self.wall_us,
            "processes": self.processes,
            "by_subcommand": subs,
        })
    }
}

fn scenario(scratch: &Scratch, bay: &Path, place: &Place) -> Result<Vec<Step>, String> {
    let repo = place.repo.as_path();
    // The watch cursor before anything happens, outside the measure.
    let (_, before) = bay_json(scratch, bay, repo, &["watch"], None)?;
    let cursor = before["detail"]["cursor"]
        .as_str()
        .ok_or_else(|| format!("bay watch returned no cursor: {before}"))?
        .to_owned();
    let mut steps = Vec::new();
    let (start, started) = step(
        scratch,
        bay,
        repo,
        "start",
        &["start", "COST-1", "--paths", "src/"],
    )?;
    steps.push(start);
    let view = started["detail"]["view"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| format!("bay start made no view: {started}"))?;
    std::fs::write(view.join("src/new.rs"), "pub fn new() {}\n")
        .map_err(|error| error.to_string())?;
    let plan: [(&'static str, &Path, Vec<&str>); 6] = [
        ("land", view.as_path(), vec!["land"]),
        (
            "executor run --once",
            place.executor.as_path(),
            vec!["executor", "run", "--once"],
        ),
        ("status", repo, vec!["status"]),
        ("status --fleet", repo, vec!["status", "--fleet"]),
        ("watch", repo, vec!["watch", "--since", cursor.as_str()]),
        (
            "evidence missing",
            repo,
            vec![
                "evidence",
                "missing",
                "--kind",
                "review",
                "--producer",
                "reviewer",
            ],
        ),
    ];
    for (name, dir, args) in plan {
        steps.push(step(scratch, bay, dir, name, &args)?.0);
    }
    Ok(steps)
}

/// Runs `bay -C <dir> <args> --json` with the trace on, and reads its trace.
fn step(
    scratch: &Scratch,
    bay: &Path,
    dir: &Path,
    name: &'static str,
    args: &[&str],
) -> Result<(Step, Value), String> {
    let trace = scratch
        .root()
        .join("trace")
        .join(format!("{}.jsonl", name.replace([' ', '-'], "_")));
    let started = Instant::now();
    let (exit, result) = bay_json(scratch, bay, dir, args, Some(&trace))?;
    let wall_us = micros(started);
    let text = std::fs::read_to_string(&trace).unwrap_or_default();
    let mut by_subcommand: BTreeMap<String, (usize, u64)> = BTreeMap::new();
    let mut pids = std::collections::BTreeSet::new();
    for line in text.lines() {
        let line: Value = serde_json::from_str(line)
            .map_err(|error| format!("{}: {error}: {line}", trace.display()))?;
        let entry = by_subcommand
            .entry(line["sub"].as_str().unwrap_or_default().to_owned())
            .or_default();
        entry.0 += 1;
        entry.1 += line["us"].as_u64().unwrap_or_default();
        pids.insert(line["pid"].as_u64().unwrap_or_default());
    }
    let step = Step {
        name,
        exit,
        decision: result["decision"].as_str().unwrap_or("?").to_owned(),
        wall_us,
        spawns: by_subcommand.values().map(|(spawns, _)| spawns).sum(),
        git_us: by_subcommand.values().map(|(_, us)| us).sum(),
        processes: pids.len(),
        by_subcommand,
    };
    Ok((step, result))
}

fn bay_json(
    scratch: &Scratch,
    bay: &Path,
    dir: &Path,
    args: &[&str],
    trace: Option<&Path>,
) -> Result<(i32, Value), String> {
    let mut cmd = scratch.command(bay);
    cmd.arg("-C").arg(dir).args(args).arg("--json");
    if let Some(trace) = trace {
        cmd.env("BAY_TEST_GIT_TRACE", trace);
    }
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("bay did not run: {error}"))?;
    let result = serde_json::from_slice(&out.stdout).map_err(|error| {
        format!(
            "bay {} printed no result ({error}): {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        )
    })?;
    Ok((out.status.code().unwrap_or(-1), result))
}

fn micros(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// `us` as milliseconds with one decimal.
fn millis(us: u64) -> String {
    format!("{}.{}", us / 1000, us % 1000 / 100)
}

/// The report as one table per run.
fn table(report: &Value) -> String {
    let mut text = format!(
        "{} · {} · {}\n",
        report["git"].as_str().unwrap_or_default(),
        report["bay"].as_str().unwrap_or_default(),
        report["os"].as_str().unwrap_or_default()
    );
    for run in report["runs"].as_array().into_iter().flatten() {
        let _ = writeln!(
            text,
            "\n{}/{} · {} commits · {} files · setup {} s",
            run["object_format"].as_str().unwrap_or_default(),
            run["ref_format"].as_str().unwrap_or_default(),
            run["commits"],
            run["files"],
            run["setup_s"]
        );
        text.push_str(
            "command              exit decision   spawns     git ms    wall ms  most git time\n",
        );
        for step in run["steps"].as_array().into_iter().flatten() {
            let _ = writeln!(
                text,
                "{:<20} {:>4} {:<10} {:>6} {:>10} {:>10}  {}",
                step["command"].as_str().unwrap_or_default(),
                step["exit"],
                step["decision"].as_str().unwrap_or_default(),
                step["git_spawns"],
                millis(step["git_us"].as_u64().unwrap_or_default()),
                millis(step["wall_us"].as_u64().unwrap_or_default()),
                costliest(&step["by_subcommand"])
            );
        }
        if let Some(kept) = run["scratch"].as_str() {
            let _ = writeln!(text, "kept: {kept}");
        }
    }
    text.trim_end().to_owned()
}

/// The three subcommands that took the most git time: `name ×spawns`.
fn costliest(subs: &Value) -> String {
    let mut subs: Vec<(&String, u64, u64)> = subs
        .as_object()
        .into_iter()
        .flatten()
        .map(|(sub, cost)| {
            (
                sub,
                cost["us"].as_u64().unwrap_or_default(),
                cost["spawns"].as_u64().unwrap_or_default(),
            )
        })
        .collect();
    subs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    subs.iter()
        .take(3)
        .map(|(sub, _, spawns)| format!("{sub} ×{spawns}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Runs `cmd` and returns its trimmed stdout, or its stderr as the error.
fn output(cmd: &mut Command) -> Result<String, String> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("{cmd:?} did not run: {error}"))?;
    if !out.status.success() {
        return Err(format!(
            "{cmd:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// [`output`] with `input` on stdin, written while the child runs.
fn output_with(cmd: &mut Command, input: &[u8]) -> Result<String, String> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{cmd:?} did not run: {error}"))?;
    let mut stdin = child.stdin.take().ok_or("no stdin pipe")?;
    let input = input.to_vec();
    let feeder = std::thread::spawn(move || stdin.write_all(&input));
    let out = child
        .wait_with_output()
        .map_err(|error| format!("{cmd:?}: {error}"))?;
    let fed = feeder.join().map_err(|_| "the stdin feeder failed")?;
    if !out.status.success() {
        return Err(format!(
            "{cmd:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    fed.map_err(|error| format!("{cmd:?}: {error}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}
