//! The relay between a sandboxed check and its test (G62). A check reads and writes nothing
//! outside its scratch, so a test that coordinates with its check (a run log, a marker, a trunk
//! move while the check runs) does not hand it a path of the box. The check writes a request
//! into its own `HOME` (inside the scratch, which the sandbox binds), and the test, outside
//! the sandbox, finds it there, runs the request's shell as itself in the box, and writes the
//! answer back beside it. The check waits for the answer, so whatever the test does happens
//! while the check runs, as it did when the check acted itself.
//!
//! A request is `$HOME/.bay-relay/<pid>.<n>/q/` (`script`, `req`, `commit`, `tree`, `args`),
//! made whole by a rename; the answer is `<pid>.<n>/r/` (`status`, `out`), also made whole by a
//! rename. `<pid>` is the asking shell's, so no later check names a request as an earlier one
//! did: a check of the same candidate gets the same scratch path again.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// The shell function a check script defines to ask its test: `bay_relay [args] <<'END'`
/// runs the here-document as the test, outside the sandbox, with `$req`, `$commit`, `$tree`
/// and the args as `$1`…; it prints the shell's stdout and returns its status. Nothing goes to
/// the check's stderr: bay reads that for a path outside the sandbox.
pub const FUNCTION: &str = r#"bay_relay() {
  _r="$HOME/.bay-relay"; mkdir -p "$_r" 2>/dev/null; _n=0
  until mkdir "$_r/$$.$_n" 2>/dev/null; do _n=$((_n + 1)); done
  _d="$_r/$$.$_n"; _q="$_d/q.tmp"; mkdir "$_q" 2>/dev/null
  cat > "$_q/script"
  printf '%s' "${req-}" > "$_q/req"; printf '%s' "${commit-}" > "$_q/commit"
  printf '%s' "${tree-}" > "$_q/tree"
  : > "$_q/args"; for _a in "$@"; do printf '%s\n' "$_a" >> "$_q/args"; done
  mv "$_q" "$_d/q" 2>/dev/null
  _i=0
  until [ -e "$_d/r" ]; do
    sleep 0.01; _i=$((_i + 1)); [ "$_i" -lt 12000 ] || return 125
  done
  cat "$_d/r/out"; return "$(cat "$_d/r/status")"
}
"#;

/// The check-side shell that runs `snippet` as the test, outside the sandbox, passing the
/// check's own `"$@"`: the function, then the call with `snippet` as a quoted here-document.
#[must_use]
pub fn call(snippet: &str) -> String {
    format!("{FUNCTION}bay_relay \"$@\" <<'BAY_RELAY_END'\n{snippet}\nBAY_RELAY_END\n")
}

/// The test's side: a thread that serves every request found under the box's git dirs until
/// it is dropped.
pub struct Watcher {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    /// Serves requests under `root`, running each with `env` (the box's environment).
    pub fn start(root: PathBuf, env: Vec<(String, String)>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let env = Arc::new(env);
        let thread = std::thread::spawn(move || {
            // Every request served or being served, by its name, which no later request takes:
            // each is served once. A second serve would run the check's shell twice: a doubled
            // run log line, a second trunk move. A test finding (2026-11-19, batch tests under
            // load): a request listed before its answer was served again after it, while its
            // check, which had read the answer, was having its scratch removed; the second
            // serve read what was left of the request and logged a run with no request in it.
            let mut served = HashSet::new();
            while !stopped.load(Ordering::SeqCst) {
                for request in requests(&root) {
                    if !served.insert(request.clone()) {
                        continue;
                    }
                    let (root, env) = (root.clone(), Arc::clone(&env));
                    std::thread::spawn(move || serve(&root, &env, &request));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The git dirs under `root` that hold bay's state: a bare repository or a `.git`, at most a
/// few levels down, never inside one.
fn git_dirs(dir: &Path, depth: u32, found: &mut Vec<PathBuf>) {
    if dir.join("HEAD").is_file() && dir.join("bay").is_dir() {
        found.push(dir.to_path_buf());
        return;
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            git_dirs(&entry.path(), depth - 1, found);
        }
    }
}

/// Every unanswered request: `<scratch root>/<candidate>/check-<n>/home/.bay-relay/<n>` for a
/// check (the box's `HOME` is `<root>/home`, so each repository's scratch root is under
/// `<root>/home/.local/state/bay/check/`, `check::scratch_root`), and
/// `<git dir>/bay/inputs-run/<run>/home/.bay-relay/<n>` for an input provider.
fn requests(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    git_dirs(root, 4, &mut dirs);
    let children = |dir: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default()
    };
    let mut homes = Vec::new();
    for scratch_root in children(&root.join("home/.local/state/bay/check")) {
        for candidate in children(&scratch_root) {
            for check in children(&candidate) {
                homes.push(check.join("home"));
            }
        }
    }
    for git_dir in dirs {
        for run in children(&git_dir.join("bay").join("inputs-run")) {
            homes.push(run.join("home"));
        }
    }
    homes
        .iter()
        .flat_map(|home| children(&home.join(".bay-relay")))
        .filter(|request| request.join("q").is_dir() && !request.join("r").exists())
        .collect()
}

fn serve(root: &Path, env: &[(String, String)], request: &Path) {
    let q = request.join("q");
    let read = |name: &str| std::fs::read_to_string(q.join(name));
    // A request read in part is never run: its check is gone with its scratch.
    let (Ok(script), Ok(args), Ok(req), Ok(commit), Ok(tree)) = (
        read("script"),
        read("args"),
        read("req"),
        read("commit"),
        read("tree"),
    ) else {
        return;
    };
    let args: Vec<String> = args.lines().map(str::to_owned).collect();
    let out = Command::new("sh")
        .arg("-c")
        .arg(&script)
        .arg("sh")
        .args(&args)
        .current_dir(root)
        .env_clear()
        .envs(env.iter().map(|(name, value)| (name, value)))
        .env("req", req)
        .env("commit", commit)
        .env("tree", tree)
        .stdin(Stdio::null())
        .output();
    let (status, stdout) = match out {
        Ok(out) => {
            if !out.stderr.is_empty() {
                eprintln!(
                    "relay: {}: {}",
                    script.trim(),
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            (out.status.code().unwrap_or(1), out.stdout)
        }
        Err(error) => {
            eprintln!("relay: {error}");
            (1, Vec::new())
        }
    };
    let answer = request.join("r.tmp");
    // The check's scratch may be gone (it was killed, or its run ended): nothing to answer.
    if std::fs::create_dir_all(&answer).is_err()
        || std::fs::write(answer.join("out"), stdout).is_err()
        || std::fs::write(answer.join("status"), status.to_string()).is_err()
    {
        return;
    }
    let _ = std::fs::rename(&answer, request.join("r"));
}
