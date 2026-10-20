//! Test boxes: a temp HOME, a global Git config that holds only the box's choice of no check
//! sandbox (ruling R14), and a `PATH` whose `git` and `claude` the test controls. Every other
//! git operation runs the real git.

#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use assert_cmd::Command;
use tempfile::TempDir;

pub mod relay;
pub mod talk;

/// What a new test box's global Git config holds: the box's choice of no check sandbox.
pub const UNSANDBOXED_CHOICE: &str = "[bay]\n\texecutorSandbox = none\n";

/// The config a new work repository starts with.
pub const MINIMAL_CONFIG: &str = "trunk = \"main\"\n";

pub struct TestBox {
    /// Serves the relay of this box's checks (G62); started by the first [`TestBox::relay`].
    relay: std::sync::OnceLock<relay::Watcher>,
    root: TempDir,
    pub fakebin: PathBuf,
    pub home: PathBuf,
    pub global_config: PathBuf,
    real_git: PathBuf,
}

impl TestBox {
    #[must_use]
    pub fn new() -> Self {
        let root = TempDir::new().unwrap();
        let fakebin = root.path().join("fakebin");
        let home = root.path().join("home");
        std::fs::create_dir_all(&fakebin).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let global_config = root.path().join("global.gitconfig");
        // The box's choice to run checks with no sandbox where it has none (ruling R14), so
        // the suite runs on a machine without one; a sandbox case clears it.
        std::fs::write(&global_config, UNSANDBOXED_CHOICE).unwrap();
        let out = StdCommand::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap();
        let real_git = PathBuf::from(String::from_utf8(out.stdout).unwrap().trim());
        let test_box = Self {
            relay: std::sync::OnceLock::new(),
            root,
            fakebin,
            home,
            global_config,
            real_git,
        };
        test_box.write_git_wrapper(None);
        // This machine's sudo is never asked: the box has no passwordless sudo unless a case
        // says so (doctor's key-custody row probes it).
        test_box.fake_sudo(false);
        test_box
    }

    /// Check-script shell that runs `snippet` as this test, in the box and outside the check's
    /// sandbox, while the check waits ([`relay`]). A check sees nothing of the box (G62), so a
    /// run log, a marker or a trunk move the test needs goes through here, never through a
    /// path of the box in the script.
    pub fn relay(&self, snippet: &str) -> String {
        self.relay.get_or_init(|| {
            let mut env: Vec<(String, String)> = vec![
                ("PATH".to_owned(), self.path_var()),
                ("HOME".to_owned(), self.home.display().to_string()),
                (
                    "GIT_CONFIG_GLOBAL".to_owned(),
                    self.global_config.display().to_string(),
                ),
                ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
                (
                    "GIT_CEILING_DIRECTORIES".to_owned(),
                    self.path().display().to_string(),
                ),
            ];
            env.push(("LANG".to_owned(), "C.UTF-8".to_owned()));
            relay::Watcher::start(self.path().to_path_buf(), env)
        });
        relay::call(snippet)
    }

    /// Puts a `sudo` on PATH that logs its arguments to `<root>/sudo.log` and succeeds when
    /// `passwordless`, else fails as `sudo -n` does when it would ask for a password.
    pub fn fake_sudo(&self, passwordless: bool) {
        let log = self.path().join("sudo.log");
        let code = i32::from(!passwordless);
        write_executable(
            &self.fakebin.join("sudo"),
            &format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\nexit {code}\n",
                log.display()
            ),
        );
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.root.path()
    }

    /// Hides this machine's check sandbox from bay: a `bwrap` on `PATH` that fails its probe,
    /// so a check finds no sandbox (ruling R14). On Linux only; macOS's `sandbox-exec` is at a
    /// fixed path.
    pub fn no_sandbox(&self) {
        write_executable(&self.fakebin.join("bwrap"), "#!/bin/sh\nexit 1\n");
    }

    /// Sets or clears this box's choice to run checks with no sandbox, the global Git config
    /// `bay.executorSandbox = none` (ruling R14). A new test box has made that choice, so a
    /// machine without a sandbox still runs the suite's checks.
    pub fn choose_unsandboxed(&self, chosen: bool) {
        let mut cmd = self.git(self.path());
        cmd.args(["config", "--global"]);
        if chosen {
            cmd.args(["bay.executorSandbox", "none"]);
        } else {
            cmd.args(["--unset-all", "bay.executorSandbox"]);
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success() || !chosen, "{out:?}");
    }

    /// `git --version` answers `version`; every other call runs the real git.
    pub fn fake_git_version(&self, version: &str) {
        self.write_git_wrapper(Some(version));
    }

    fn write_git_wrapper(&self, version: Option<&str>) {
        let real = self.real_git.display();
        let script = match version {
            None => format!("#!/bin/sh\nexec \"{real}\" \"$@\"\n"),
            Some(version) => format!(
                "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = --version ]; then echo \"git version {version}\"; exit 0; fi\ndone\nexec \"{real}\" \"$@\"\n"
            ),
        };
        write_executable(&self.fakebin.join("git"), &script);
    }

    /// Puts a `claude` that reports `version` on PATH, or removes it.
    pub fn fake_claude(&self, version: Option<&str>) {
        let path = self.fakebin.join("claude");
        match version {
            Some(version) => write_executable(
                &path,
                &format!("#!/bin/sh\necho \"{version} (Claude Code)\"\n"),
            ),
            None => {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    #[must_use]
    pub fn local_bin(&self) -> PathBuf {
        self.home.join(".local").join("bin")
    }

    fn path_var(&self) -> String {
        format!(
            "{}:{}:/bin:/usr/bin",
            self.fakebin.display(),
            self.local_bin().display()
        )
    }

    /// A git command with this box's environment.
    #[must_use]
    pub fn git(&self, dir: &Path) -> StdCommand {
        let mut cmd = StdCommand::new(self.fakebin.join("git"));
        cmd.current_dir(dir);
        self.apply_env(&mut cmd);
        cmd
    }

    pub fn git_ok(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.git(dir).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// `bay -C <dir>` with this box's environment.
    #[must_use]
    pub fn bay(&self, dir: &Path) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bay"));
        cmd.arg("-C").arg(dir);
        cmd.env_clear()
            .env("PATH", self.path_var())
            .env("HOME", &self.home)
            .env("GIT_CONFIG_GLOBAL", &self.global_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", self.path());
        cmd
    }

    /// `bay -C <dir>` as a plain process, for running in the background.
    #[must_use]
    pub fn bay_process(&self, dir: &Path) -> StdCommand {
        let mut cmd = StdCommand::new(env!("CARGO_BIN_EXE_bay"));
        cmd.arg("-C").arg(dir);
        cmd.env_clear()
            .env("PATH", self.path_var())
            .env("HOME", &self.home)
            .env("GIT_CONFIG_GLOBAL", &self.global_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", self.path());
        cmd
    }

    fn apply_env(&self, cmd: &mut StdCommand) {
        cmd.env_clear()
            .env("PATH", self.path_var())
            .env("HOME", &self.home)
            .env("GIT_CONFIG_GLOBAL", &self.global_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", self.path())
            .env("GIT_AUTHOR_NAME", "bay test")
            .env("GIT_AUTHOR_EMAIL", "test@bay.invalid")
            .env("GIT_COMMITTER_NAME", "bay test")
            .env("GIT_COMMITTER_EMAIL", "test@bay.invalid");
    }

    /// A bare repository at `<root>/<name>`.
    pub fn bare(&self, name: &str, object_format: &str, ref_format: &str) -> PathBuf {
        let dir = self.path().join(name);
        self.git_ok(
            self.path(),
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                &format!("--object-format={object_format}"),
                &format!("--ref-format={ref_format}"),
                dir.to_str().unwrap(),
            ],
        );
        dir
    }

    /// [`Self::bare`] in the formats of [`formats`].
    pub fn bare_any(&self, name: &str) -> PathBuf {
        let (object_format, ref_format) = formats();
        self.bare(name, &object_format, &ref_format)
    }

    /// [`Self::repo`] in the formats of [`formats`], for a test that holds for every format.
    pub fn repo_any(&self, name: &str) -> PathBuf {
        let (object_format, ref_format) = formats();
        self.repo(name, &object_format, &ref_format)
    }

    /// [`Self::repo_without_remote`] in the formats of [`formats`].
    pub fn repo_without_remote_any(&self, name: &str) -> PathBuf {
        let (object_format, ref_format) = formats();
        self.repo_without_remote(name, &object_format, &ref_format)
    }

    /// `bay host init <path>` in the object format of [`formats`]. `host init` defaults to sha1
    /// and does not yet follow the local repository's format (queued for the idiom pass), so a
    /// host and its repositories must be given one format.
    pub fn host_init(&self, path: &Path) -> serde_json::Value {
        let (object_format, _) = formats();
        self.bay_ok(
            self.path(),
            &[
                "host",
                "init",
                path.to_str().unwrap(),
                "--object-format",
                &object_format,
            ],
        )
    }

    /// A work repository at `<root>/<name>` with `origin` pointing at a fresh bare remote.
    pub fn repo(&self, name: &str, object_format: &str, ref_format: &str) -> PathBuf {
        let remote = self.bare(&format!("{name}-remote.git"), object_format, ref_format);
        let dir = self.repo_without_remote(name, object_format, ref_format);
        self.add_origin(&dir, &remote);
        dir
    }

    /// Adds `origin` at `url` to `repo` and, as a clone would, names its default branch
    /// `main`: the bootstrap bay finds the trunk by (ruling: config location).
    pub fn add_origin(&self, repo: &Path, url: &Path) {
        self.git_ok(repo, &["remote", "add", "origin", url.to_str().unwrap()]);
        self.git_ok(
            repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
    }

    pub fn repo_without_remote(
        &self,
        name: &str,
        object_format: &str,
        ref_format: &str,
    ) -> PathBuf {
        let dir = self.path().join(name);
        self.git_ok(
            self.path(),
            &[
                "init",
                "-q",
                "-b",
                "main",
                &format!("--object-format={object_format}"),
                &format!("--ref-format={ref_format}"),
                dir.to_str().unwrap(),
            ],
        );
        // bay never runs on a default config (ruling: config location): every work
        // repository starts with a minimal one, which the first seed commits to trunk, where
        // bay reads it.
        Self::bay_config(&dir, MINIMAL_CONFIG);
        dir
    }

    /// Commits `text` as `.bay/config.toml` on the remote's trunk, on top of its tip (or as its
    /// first commit), and fetches it: bay reads the config only there (ruling: config
    /// location). The work tree and its index stay as they are.
    pub fn commit_config(&self, repo: &Path, text: &str) -> String {
        let _ = self
            .git(repo)
            .args(["fetch", "-q", "origin", "main"])
            .output();
        let parent = self
            .git(repo)
            .args([
                "rev-parse",
                "-q",
                "--verify",
                "refs/remotes/origin/main^{commit}",
            ])
            .output()
            .unwrap();
        let parent = String::from_utf8(parent.stdout).unwrap().trim().to_owned();
        let scratch = self.path().join(".commit-config");
        std::fs::create_dir_all(&scratch).unwrap();
        let file = scratch.join("config.toml");
        std::fs::write(&file, text).unwrap();
        let blob = self.git_ok(repo, &["hash-object", "-w", file.to_str().unwrap()]);
        let index = scratch.join("index");
        let _ = std::fs::remove_file(&index);
        let with_index = |args: &[&str]| {
            let out = self
                .git(repo)
                .env("GIT_INDEX_FILE", &index)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        if !parent.is_empty() {
            with_index(&["read-tree", &parent]);
        }
        let entry = format!("100644,{blob},.bay/config.toml");
        with_index(&["update-index", "--add", "--cacheinfo", &entry]);
        let tree = with_index(&["write-tree"]);
        let mut args = vec!["commit-tree", tree.as_str(), "-m", "config"];
        if !parent.is_empty() {
            args.extend(["-p", parent.as_str()]);
        }
        let commit = self.git_ok(repo, &args);
        self.git_ok(
            repo,
            &["push", "-q", "origin", &format!("{commit}:refs/heads/main")],
        );
        self.git_ok(repo, &["update-ref", "refs/remotes/origin/main", &commit]);
        commit
    }

    /// Writes `<repo>/.bay/config.toml`.
    pub fn bay_config(repo: &Path, text: &str) {
        std::fs::create_dir_all(repo.join(".bay")).unwrap();
        std::fs::write(repo.join(".bay/config.toml"), text).unwrap();
    }
}

impl TestBox {
    /// Commits `files` (path, contents) on `main` in `repo` and pushes it to `origin`.
    pub fn seed_trunk(&self, repo: &Path, files: &[(&str, &str)]) -> String {
        for (path, text) in files {
            let full = repo.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, text).unwrap();
        }
        self.git_ok(repo, &["add", "--all"]);
        self.git_ok(repo, &["commit", "-q", "-m", "trunk"]);
        self.git_ok(repo, &["push", "-q", "origin", "main"]);
        self.git_ok(repo, &["rev-parse", "HEAD"])
    }

    /// Runs bay with `--json`, asserts the exit code, and returns the result.
    pub fn bay_ok(&self, dir: &Path, args: &[&str]) -> serde_json::Value {
        let (code, result) = result_json(self.bay(dir).args(args));
        assert_eq!(code, 0, "bay {args:?}: {result}");
        result
    }
}

/// The policy and check adapter files for a trunk: one required check `tests` that runs
/// `scripts/bay-check`. `before` runs first (shell); `result` prints the check result,
/// with `$commit` and `$tree` taken from the request.
#[must_use]
pub fn check_files(
    before: &str,
    result: &str,
    expected_minimum: u32,
    timeout_s: u32,
) -> Vec<(String, String)> {
    let script = format!(
        "#!/bin/sh\nreq=$(cat)\ncommit=$(printf '%s' \"$req\" | sed -n 's/.*\"candidate\":{{\"commit\":\"\\([0-9a-f]*\\)\",\"tree\":\"\\([0-9a-f]*\\)\"}}.*/\\1/p')\ntree=$(printf '%s' \"$req\" | sed -n 's/.*\"candidate\":{{\"commit\":\"\\([0-9a-f]*\\)\",\"tree\":\"\\([0-9a-f]*\\)\"}}.*/\\2/p')\n{before}\n{result}\n"
    );
    let policy = format!(
        "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = {expected_minimum}\ntimeout_s = {timeout_s}\n"
    );
    vec![
        ("scripts/bay-check".to_owned(), script),
        (".bay/policy.toml".to_owned(), policy),
    ]
}

/// A policy that declares the repository unchecked (`unchecked = true`, no `[[check]]`): a
/// change lands with no check, and its landing says so. Without a policy at its base a change
/// is refused with `POLICY_MISSING`, so a test that lands without checks seeds this.
pub const UNCHECKED_POLICY: &str = "revision = 1\nunchecked = true\n";

/// [`UNCHECKED_POLICY`] as the files [`TestBox::seed_with_check`] takes.
#[must_use]
pub fn unchecked_files() -> Vec<(String, String)> {
    vec![(".bay/policy.toml".to_owned(), UNCHECKED_POLICY.to_owned())]
}

/// A check result with `units` passing tests.
#[must_use]
pub fn passing_result(units: u32) -> String {
    let checks: Vec<String> = (1..=units)
        .map(|n| format!("{{\\\"id\\\":\\\"t{n}\\\",\\\"verdict\\\":\\\"pass\\\"}}"))
        .collect();
    format!(
        "printf '%s' \"{{\\\"schema\\\":\\\"bay.check.result/1\\\",\\\"exercised\\\":{{\\\"commit\\\":\\\"$commit\\\",\\\"tree\\\":\\\"$tree\\\"}},\\\"plan\\\":{{\\\"discovered\\\":{units},\\\"selected\\\":{units},\\\"completed\\\":{units}}},\\\"checks\\\":[{}]}}\"",
        checks.join(",")
    )
}

impl TestBox {
    /// Seeds trunk with `files` plus executable `scripts/bay-check`, and pushes it.
    pub fn seed_with_check(
        &self,
        repo: &Path,
        files: &[(&str, &str)],
        check: &[(String, String)],
    ) -> String {
        // Tests that kill an executor wait one lease before the takeover: keep it short, unless
        // the test sets its own lease.
        let config = check
            .iter()
            .find(|(path, _)| path == ".bay/config.toml")
            .map_or_else(String::new, |(_, text)| text.clone());
        let config = if config.contains("lease_s") {
            config
        } else if config.contains("[executor]") {
            config.replacen(
                "[executor]\n",
                "[executor]\nlease_s = 2\nheartbeat_s = 1\n",
                1,
            )
        } else {
            format!("{config}\n[executor]\nlease_s = 2\nheartbeat_s = 1\n")
        };
        let mut all: Vec<(&str, &str)> = files.to_vec();
        all.extend(
            check
                .iter()
                .filter(|(path, _)| path != ".bay/config.toml")
                .map(|(path, text)| (path.as_str(), text.as_str())),
        );
        all.push((".bay/config.toml", config.as_str()));
        for (path, text) in &all {
            let full = repo.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, text).unwrap();
        }
        let script = repo.join("scripts/bay-check");
        if script.exists() {
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        self.git_ok(repo, &["add", "--all"]);
        self.git_ok(repo, &["commit", "-q", "-m", "trunk"]);
        self.git_ok(repo, &["push", "-q", "origin", "main"]);
        self.git_ok(repo, &["rev-parse", "HEAD"])
    }

    /// Starts a change on `src/` and writes one new file in its view. Returns (id, view).
    pub fn start_and_edit(&self, repo: &Path) -> (String, PathBuf) {
        // Each change gets its own intent: one intent claims one change.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
        let intent = format!(
            "CARD-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let started = self.bay_ok(repo, &["start", &intent, "--paths", "src/"]);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
        (id, view)
    }

    /// Runs git on a bare repository, naming it explicitly (Git 3.0 `safe.bareRepository`).
    pub fn git_bare(&self, bare: &Path, args: &[&str]) -> String {
        let mut all = vec![format!("--git-dir={}", bare.display())];
        all.extend(args.iter().map(|arg| (*arg).to_owned()));
        let all: Vec<&str> = all.iter().map(String::as_str).collect();
        self.git_ok(self.path(), &all)
    }

    /// The `Change-Id` trailers on the remote's trunk, newest first.
    pub fn trunk_change_ids(&self, bare: &Path) -> Vec<String> {
        self.git_bare(
            bare,
            &[
                "log",
                "main",
                "--format=%(trailers:key=Change-Id,valueonly)",
            ],
        )
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
    }

    /// A clone of `bare` at `<root>/<name>`.
    pub fn clone_of(&self, bare: &Path, name: &str) -> PathBuf {
        let dir = self.path().join(name);
        self.git_ok(
            self.path(),
            &["clone", "-q", bare.to_str().unwrap(), dir.to_str().unwrap()],
        );
        dir
    }
}

/// SSH keys for every class, plus a rogue key that is enrolled nowhere.
pub struct TestKeys {
    pub agent: PathBuf,
    pub executor: PathBuf,
    pub producer: PathBuf,
    pub registry: PathBuf,
    pub rogue: PathBuf,
}

/// The principals the test registry enrolls. `agent` is bay's default actor name in tests.
pub const AGENT: &str = "agent";
pub const EXECUTOR: &str = "exec-1";
pub const PRODUCER: &str = "ci-1";
pub const REGISTRAR: &str = "admin";

impl TestBox {
    #[must_use]
    pub fn keys(&self) -> TestKeys {
        let dir = self.path().join("keys");
        std::fs::create_dir_all(&dir).unwrap();
        let make = |name: &str| {
            let path = dir.join(name);
            if !path.exists() {
                let status = StdCommand::new("ssh-keygen")
                    .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
                    .arg(&path)
                    .status()
                    .unwrap();
                assert!(status.success());
            }
            path
        };
        TestKeys {
            agent: make("agent"),
            executor: make("executor"),
            producer: make("producer"),
            registry: make("registry"),
            rogue: make("rogue"),
        }
    }

    /// Commits a registry with one allowed-signers file per class, signed by the registry
    /// key, and pushes it to `origin`. Returns the registry commit.
    pub fn enroll(&self, repo: &Path, keys: &TestKeys) -> String {
        self.enroll_executor(repo, keys, &keys.executor)
    }

    /// Like [`TestBox::enroll`], with `executor` as the executor key; a later registry commit
    /// is parented on the current one (fast-forward), so this rotates keys.
    pub fn enroll_executor(&self, repo: &Path, keys: &TestKeys, executor: &Path) -> String {
        self.enroll_with(repo, keys, executor, &[])
    }

    /// Like [`TestBox::enroll_executor`], with more agents enrolled beside `agent`.
    pub fn enroll_with(
        &self,
        repo: &Path,
        keys: &TestKeys,
        executor: &Path,
        more_agents: &[(&str, &Path)],
    ) -> String {
        self.enroll_classes(
            repo,
            keys,
            executor,
            more_agents,
            &[(PRODUCER, &keys.producer)],
        )
    }

    /// Like [`TestBox::enroll_with`], with `producers` as the whole producer file.
    pub fn enroll_classes(
        &self,
        repo: &Path,
        keys: &TestKeys,
        executor: &Path,
        more_agents: &[(&str, &Path)],
        producers: &[(&str, &Path)],
    ) -> String {
        let mut agents = key_line(AGENT, &keys.agent);
        for (name, key) in more_agents {
            agents.push_str(&key_line(name, key));
        }
        let producers: String = producers
            .iter()
            .map(|(name, key)| key_line(name, key))
            .collect();
        let files = [
            ("agent", agents),
            ("executor", key_line(EXECUTOR, executor)),
            ("producer", producers),
            ("registry", key_line(REGISTRAR, &keys.registry)),
        ];
        self.enroll_files(repo, keys, &files)
    }

    /// Commits a registry whose tree is exactly `files` (class file name, text), signed by the
    /// registry key and parented on the current one, then updates and pushes it.
    pub fn enroll_files(&self, repo: &Path, keys: &TestKeys, files: &[(&str, String)]) -> String {
        let commit = self.registry_commit(repo, keys, files);
        self.git_ok(repo, &["update-ref", "refs/bay/registry", &commit]);
        self.git_ok(repo, &["push", "-q", "origin", "refs/bay/registry"]);
        commit
    }

    /// The registry commit [`TestBox::enroll_files`] makes, with nothing updated or pushed.
    pub fn registry_commit(
        &self,
        repo: &Path,
        keys: &TestKeys,
        files: &[(&str, String)],
    ) -> String {
        let mut tree_input = String::new();
        for (name, text) in files {
            let out = self
                .git(repo)
                .args(["hash-object", "-w", "--stdin"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write;
                    child.stdin.take().unwrap().write_all(text.as_bytes())?;
                    child.wait_with_output()
                })
                .unwrap();
            let blob = String::from_utf8(out.stdout).unwrap().trim().to_owned();
            {
                use std::fmt::Write;
                let _ = writeln!(tree_input, "100644 blob {blob}\t{name}");
            }
        }
        let out = self
            .git(repo)
            .arg("mktree")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(tree_input.as_bytes())?;
                child.wait_with_output()
            })
            .unwrap();
        let tree = String::from_utf8(out.stdout).unwrap().trim().to_owned();
        let signing_key = format!("user.signingkey={}", keys.registry.display());
        let parent = self
            .git(repo)
            .args(["rev-parse", "--verify", "-q", "refs/bay/registry"])
            .output()
            .unwrap();
        let parent = String::from_utf8(parent.stdout).unwrap().trim().to_owned();
        let mut args = vec![
            "-c",
            "gpg.format=ssh",
            "-c",
            &signing_key,
            "commit-tree",
            "-S",
            &tree,
        ];
        if !parent.is_empty() {
            args.extend(["-p", &parent]);
        }
        args.extend(["-m", "enroll"]);
        self.git_ok(repo, &args)
    }

    /// Whether `commit` in `bare` verifies against an allowed-signers file holding only the
    /// executor key, and was committed by the executor.
    pub fn executor_signed(&self, bare: &Path, keys: &TestKeys, commit: &str) -> bool {
        let public = std::fs::read_to_string(keys.executor.with_extension("pub")).unwrap();
        let mut words = public.split_whitespace();
        let allowed = self.path().join("executor.allowed");
        std::fs::write(
            &allowed,
            format!(
                "{EXECUTOR} {} {}\n",
                words.next().unwrap(),
                words.next().unwrap()
            ),
        )
        .unwrap();
        let setting = format!("gpg.ssh.allowedSignersFile={}", allowed.display());
        let verified = self
            .git(self.path())
            .args([
                &format!("--git-dir={}", bare.display()),
                "-c",
                &setting,
                "verify-commit",
                commit,
            ])
            .output()
            .unwrap()
            .status
            .success();
        verified && self.git_bare(bare, &["log", "-1", "--format=%cn", commit]) == EXECUTOR
    }

    /// Points this repository's bay at the agent, executor and producer keys.
    pub fn use_keys(&self, repo: &Path, keys: &TestKeys) {
        for (key, value) in [
            ("bay.agentKey", keys.agent.display().to_string()),
            ("bay.executorKey", keys.executor.display().to_string()),
            ("bay.executorName", EXECUTOR.to_owned()),
            ("bay.producerKey", keys.producer.display().to_string()),
            ("bay.producerName", PRODUCER.to_owned()),
        ] {
            self.git_ok(repo, &["config", key, &value]);
        }
    }
}

/// The allowed-signers line enrolling `key`'s public half under `principal`.
#[must_use]
pub fn key_line(principal: &str, key: &Path) -> String {
    let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
    let mut words = public.split_whitespace();
    format!(
        "{principal} {} {}\n",
        words.next().unwrap(),
        words.next().unwrap()
    )
}

/// Writes `script` through a child `sh`, so this multithreaded test process never holds a write
/// descriptor to a file that is later executed. A sibling test that forks while that descriptor is
/// open would inherit it until its own exec, and on Linux any exec of the file in that window fails
/// with `ETXTBSY`. The script goes in beside the file and is renamed over it: a file another
/// process is executing (a detached `bay` still running the git wrapper) cannot be opened for
/// writing, also `ETXTBSY`.
pub fn write_executable(path: &Path, script: &str) {
    use std::io::Write as _;
    let mut child = StdCommand::new("sh")
        .args([
            "-c",
            "cat > \"$1.new\" && chmod 755 \"$1.new\" && mv -f \"$1.new\" \"$1\"",
            "sh",
        ])
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    assert!(
        child.wait().unwrap().success(),
        "writing {}",
        path.display()
    );
}

/// The object and ref format for a test that holds for every format: `sha1`/`files`, or
/// `BAY_TEST_FORMATS` (for example `sha256/reftable`, which the git-next CI job sets so the
/// G-tests also run on Git 3.0's defaults). A test that pins a format calls `repo` directly.
#[must_use]
pub fn formats() -> (String, String) {
    let text = std::env::var("BAY_TEST_FORMATS").unwrap_or_else(|_| "sha1/files".to_owned());
    let (object_format, ref_format) = text
        .split_once('/')
        .unwrap_or_else(|| panic!("BAY_TEST_FORMATS is <object>/<ref>, not {text}"));
    (object_format.to_owned(), ref_format.to_owned())
}

/// The check sandbox this machine runs (`sandbox-exec` on macOS, `bwrap` on Linux), or `None`
/// when there is none, so a sandbox case can only skip. CI sets `BAY_REQUIRE_SANDBOX` on Linux,
/// where a missing sandbox then fails the case instead of skipping it unnoticed.
#[must_use]
pub fn sandbox() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        return Some("sandbox-exec");
    }
    // The probe is bay's own: the allow-list a check runs in, with no network (G62).
    let usable = StdCommand::new("bwrap")
        .args(bay::check::sandbox::bwrap_system_args(false))
        .arg("true")
        .output()
        .is_ok_and(|out| out.status.success());
    assert!(
        usable || std::env::var_os("BAY_REQUIRE_SANDBOX").is_none(),
        "BAY_REQUIRE_SANDBOX is set but bwrap cannot run the check sandbox \
         (`bwrap --unshare-pid --unshare-net --ro-bind /usr /usr … --proc /proc --dev /dev true` fails)"
    );
    usable.then_some("bwrap")
}

/// Runs a command and parses its stdout as the `bay.result/1` document.
pub fn result_json(cmd: &mut Command) -> (i32, serde_json::Value) {
    let out = cmd.arg("--json").output().unwrap();
    let code = out.status.code().unwrap_or(-1);
    let text = String::from_utf8(out.stdout).unwrap();
    let value = serde_json::from_str(&text).unwrap_or_else(|error| {
        panic!(
            "not JSON ({error}): {text}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (code, value)
}

/// The doctor row named `name`.
pub fn row<'a>(result: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    result["detail"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .unwrap_or_else(|| panic!("no row {name} in {result}"))
}
