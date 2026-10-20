//! The relay between a sandboxed check and its test (G62). A check reads and writes nothing
//! outside its scratch, so a test that coordinates with its check (a run log, a marker, a trunk
//! move while the check runs) does not hand it a path of the box. The check writes a request
//! into its own `HOME` (inside the scratch, which the sandbox binds), and the test, outside
//! the sandbox, finds it there, runs the request's shell as itself in the box, and writes the
//! answer back beside it. The check waits for the answer, so whatever the test does happens
//! while the check runs, as it did when the check acted itself.
//!
//! A request is `$HOME/.bay-relay/<n>/q/` (`script`, `req`, `commit`, `tree`, `args`), made
//! whole by a rename; the answer is `<n>/r/` (`status`, `out`), also made whole by a rename.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// The shell function a check script defines to ask its test: `bay_relay [args] <<'END'`
/// runs the here-document as the test, outside the sandbox, with `$req`, `$commit`, `$tree`
/// and the args as `$1`…; it prints the shell's stdout and returns its status. Nothing goes to
/// the check's stderr: bay reads that for a path outside the sandbox.
pub const FUNCTION: &str = r#"bay_relay() {
  _r="$HOME/.bay-relay"; mkdir -p "$_r" 2>/dev/null; _n=0
  until mkdir "$_r/$_n" 2>/dev/null; do _n=$((_n + 1)); done
  _q="$_r/$_n/q.tmp"; mkdir "$_q" 2>/dev/null
  cat > "$_q/script"
  printf '%s' "${req-}" > "$_q/req"; printf '%s' "${commit-}" > "$_q/commit"
  printf '%s' "${tree-}" > "$_q/tree"
  : > "$_q/args"; for _a in "$@"; do printf '%s\n' "$_a" >> "$_q/args"; done
  mv "$_q" "$_r/$_n/q" 2>/dev/null
  _i=0
  until [ -e "$_r/$_n/r" ]; do
    sleep 0.01; _i=$((_i + 1)); [ "$_i" -lt 12000 ] || return 125
  done
  cat "$_r/$_n/r/out"; return "$(cat "$_r/$_n/r/status")"
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
        let in_flight = Arc::new(Mutex::new(HashSet::new()));
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                for request in requests(&root) {
                    if !in_flight.lock().unwrap().insert(request.clone()) {
                        continue;
                    }
                    // The list was read before this insert, and a request served since then
                    // has left `in_flight` already: its answer is renamed into place first, so
                    // one that is answered (or gone with its scratch) is not served twice. A
                    // second serve would run the check's shell twice: a doubled run log line,
                    // a second trunk move.
                    if request.join("r").exists() || !request.join("q").is_dir() {
                        in_flight.lock().unwrap().remove(&request);
                        continue;
                    }
                    let (root, env, in_flight) =
                        (root.clone(), Arc::clone(&env), Arc::clone(&in_flight));
                    std::thread::spawn(move || {
                        serve(&root, &env, &request);
                        in_flight.lock().unwrap().remove(&request);
                    });
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

/// Every unanswered request: `<git dir>/bay/check/<candidate>/check-<n>/home/.bay-relay/<n>`
/// for a check, `<git dir>/bay/inputs-run/<run>/home/.bay-relay/<n>` for an input provider.
fn requests(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    git_dirs(root, 4, &mut dirs);
    let children = |dir: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default()
    };
    let mut homes = Vec::new();
    for git_dir in dirs {
        let bay = git_dir.join("bay");
        for candidate in children(&bay.join("check")) {
            for check in children(&candidate) {
                homes.push(check.join("home"));
            }
        }
        for run in children(&bay.join("inputs-run")) {
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
    let read = |name: &str| std::fs::read_to_string(q.join(name)).unwrap_or_default();
    let script = read("script");
    let args: Vec<String> = read("args").lines().map(str::to_owned).collect();
    let out = Command::new("sh")
        .arg("-c")
        .arg(&script)
        .arg("sh")
        .args(&args)
        .current_dir(root)
        .env_clear()
        .envs(env.iter().map(|(name, value)| (name, value)))
        .env("req", read("req"))
        .env("commit", read("commit"))
        .env("tree", read("tree"))
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
