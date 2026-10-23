//! A local signed release for `bay update` tests (G50, auto-update): a `file://` base passed
//! through `BAY_TEST_RELEASE_URL`, signed by a throwaway key passed through
//! `BAY_TEST_RELEASE_KEY`. The bay under test is a copy at the fixture's `~/.local/bin/bay`,
//! so a swap never touches the build. Box-level settings live in the fixture's global Git
//! config (`GIT_CONFIG_GLOBAL`).

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};
use tempfile::TempDir;

/// How a fixture release's archive is built.
#[derive(Default)]
pub struct Build {
    /// What the binary's `--version` prints (default `bay <tag>`).
    pub prints: Option<String>,
    /// `bay` in the archive is a symbolic link.
    pub symlink: bool,
    /// The binary's `install` fails.
    pub install_fails: bool,
    /// The archive's name (default this platform's).
    pub archive: Option<String>,
}

pub struct Fixture {
    root: TempDir,
    key: PathBuf,
    public: String,
}

/// This platform's release target, as `scripts/release.sh` names its archives.
pub fn target() -> String {
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        _ => "unknown-linux-musl",
    };
    format!("{}-{os}", std::env::consts::ARCH)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    bay::agent_settings::hex(&Sha256::digest(bytes))
}

/// `output` and `spawn` that retry while the program is "text file busy" (ETXTBSY): a copied
/// bay can be briefly held open for writing by a child another test thread forked during the
/// copy.
pub trait RetryBusy {
    fn output_busy(&mut self) -> std::process::Output;
    fn spawn_busy(&mut self) -> std::process::Child;
}

fn retry_busy<T>(mut attempt: impl FnMut() -> std::io::Result<T>) -> T {
    let mut tries = 0;
    loop {
        match attempt() {
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 100 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            other => return other.unwrap(),
        }
    }
}

impl RetryBusy for Command {
    fn output_busy(&mut self) -> std::process::Output {
        retry_busy(|| self.output())
    }

    fn spawn_busy(&mut self) -> std::process::Child {
        retry_busy(|| self.spawn())
    }
}

pub fn run(cmd: &mut Command) -> String {
    let out = cmd.output_busy();
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

impl Fixture {
    pub fn new() -> Self {
        let root = TempDir::new().unwrap();
        for dir in ["home/.local/bin", "state", "keys", "release", "fakebin"] {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        std::fs::write(root.path().join("global.gitconfig"), "").unwrap();
        std::fs::copy(
            env!("CARGO_BIN_EXE_bay"),
            root.path().join("home/.local/bin/bay"),
        )
        .unwrap();
        let key = Self::keygen(root.path(), "release");
        let public = Self::public_line(&key);
        Self { root, key, public }
    }

    pub fn keygen(root: &Path, name: &str) -> PathBuf {
        let key = root.join("keys").join(name);
        run(Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "test-release", "-f"])
            .arg(&key));
        key
    }

    pub fn public_line(key: &Path) -> String {
        let text = std::fs::read_to_string(key.with_extension("pub")).unwrap();
        text.split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn path(&self) -> &Path {
        self.root.path()
    }

    /// Puts a `curl` on the fixture's PATH that logs each call to `curl.log`, then runs the
    /// real one.
    pub fn log_curl(&self) {
        let real = run(Command::new("sh").args(["-c", "command -v curl"]));
        let log = self.path().join("curl.log");
        let script = format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real.trim()
        );
        let path = self.path().join("fakebin/curl");
        std::fs::write(&path, script).unwrap();
        run(Command::new("chmod").arg("755").arg(&path));
    }

    /// The logged `curl` calls whose arguments contain `needle`.
    pub fn curl_calls(&self, needle: &str) -> usize {
        std::fs::read_to_string(self.path().join("curl.log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(needle))
            .count()
    }

    /// The installed bay: `$HOME/.local/bin/bay`.
    pub fn bin(&self) -> PathBuf {
        self.home().join(".local/bin/bay")
    }

    pub fn global_config(&self) -> PathBuf {
        self.path().join("global.gitconfig")
    }

    /// Sets a box-level setting in the fixture's global Git config.
    pub fn box_config(&self, key: &str, value: &str) {
        run(Command::new("git")
            .args(["config", "--file"])
            .arg(self.global_config())
            .args([key, value]));
    }

    /// Where the once-a-day check is recorded (`XDG_STATE_HOME` points into the fixture).
    pub fn state_file(&self) -> PathBuf {
        self.path().join("state/bay/update.json")
    }

    pub fn state(&self) -> Option<serde_json::Value> {
        let text = std::fs::read_to_string(self.state_file()).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Records a check `ago_s` seconds ago.
    pub fn checked_ago(&self, ago_s: u64) -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        std::fs::create_dir_all(self.state_file().parent().unwrap()).unwrap();
        std::fs::write(
            self.state_file(),
            serde_json::json!({"checked_at": now - ago_s}).to_string(),
        )
        .unwrap();
        now - ago_s
    }

    pub fn home(&self) -> PathBuf {
        self.path().join("home")
    }

    /// A release `v<tag>` whose `VERSION` says `version` and whose binary prints `bay <tag>`,
    /// signed by `signer` (the fixture's release key when `None`).
    pub fn release(&self, tag: &str, version: &str, signer: Option<&Path>) -> PathBuf {
        self.release_with(tag, version, signer, &Build::default())
    }

    /// A release built as `build` says.
    pub fn release_with(
        &self,
        tag: &str,
        version: &str,
        signer: Option<&Path>,
        build: &Build,
    ) -> PathBuf {
        let dir = self.path().join("release/download").join(format!("v{tag}"));
        let stage = self.path().join("stage").join(tag);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&stage).unwrap();
        // The new binary: prints its version, records the arguments its `install` ran with, and
        // logs any other run to `$HOME/ran-<tag>` (an executor that moved to it, G251). It takes
        // any command's `--help` (G337).
        let prints = build.prints.clone().unwrap_or_else(|| format!("bay {tag}"));
        let install = if build.install_fails {
            "echo '{\"schema\":\"bay.result/1\",\"decision\":\"failed\",\"detail\":\"no skill\"}'; exit 20".to_owned()
        } else {
            format!(
                "echo \"$*\" > \"$HOME/installed-{tag}\"; echo '{{\"schema\":\"bay.result/1\",\"decision\":\"ok\"}}'; exit 0"
            )
        };
        let script = format!(
            "#!/bin/sh\nfor a in \"$@\"; do\n  case \"$a\" in\n    --version) echo \"{prints}\"; exit 0 ;;\n    --help) exit 0 ;;\n    install) {install} ;;\n  esac\ndone\necho \"$*\" >> \"$HOME/ran-{tag}\"\nexit 1\n"
        );
        let binary = stage.join("bay");
        let _ = std::fs::remove_file(&binary);
        if build.symlink {
            // `bay` in the archive is a link to a script outside it that prints the target.
            let real = self.path().join("stage").join(format!("{tag}-real"));
            std::fs::write(&real, &script).unwrap();
            run(Command::new("chmod").arg("755").arg(&real));
            std::os::unix::fs::symlink(&real, &binary).unwrap();
        } else {
            std::fs::write(&binary, &script).unwrap();
            run(Command::new("chmod").arg("755").arg(&binary));
        }
        let archive = build
            .archive
            .clone()
            .unwrap_or_else(|| format!("bay-{}.tar.gz", target()));
        run(Command::new("tar")
            .arg("-C")
            .arg(&stage)
            .arg("-czf")
            .arg(dir.join(&archive))
            .arg("bay"));
        std::fs::write(dir.join("VERSION"), format!("{version}\n")).unwrap();
        let digest = |name: &str| sha256_hex(&std::fs::read(dir.join(name)).unwrap());
        let sums = format!(
            "{}  VERSION\n{}  {archive}\n",
            digest("VERSION"),
            digest(&archive)
        );
        std::fs::write(dir.join("SHA256SUMS"), sums).unwrap();
        run(Command::new("ssh-keygen")
            .args(["-Y", "sign", "-q", "-n", "bay-release", "-f"])
            .arg(signer.unwrap_or(&self.key))
            .arg(dir.join("SHA256SUMS")));
        dir
    }

    /// The release list the target is resolved from: `(tag, prerelease)`, newest first, one
    /// release per page (`releases-<n>.json`), then an empty page, as the GitHub API pages it.
    /// Each release lists the assets the fixture built for it, or, for one it did not build,
    /// every asset a release has, including this platform's archive.
    pub fn list(&self, releases: &[(&str, bool)]) {
        let listed: Vec<(&str, bool, bool)> = releases
            .iter()
            .map(|&(tag, pre)| (tag, pre, true))
            .collect();
        self.list_with(&listed);
    }

    /// As [`Fixture::list`], with `(tag, prerelease, built)`: a release that is not `built`
    /// lists only another platform's archive, so it has no build for this one.
    pub fn list_with(&self, releases: &[(&str, bool, bool)]) {
        for (index, &(tag, pre, built)) in releases.iter().enumerate() {
            let page = serde_json::json!([{"tag_name": format!("v{tag}"), "prerelease": pre,
                "draft": false, "assets": self.assets(tag, built)}]);
            std::fs::write(
                self.path()
                    .join(format!("release/releases-{}.json", index + 1)),
                page.to_string(),
            )
            .unwrap();
        }
        std::fs::write(
            self.path()
                .join(format!("release/releases-{}.json", releases.len() + 1)),
            "[]",
        )
        .unwrap();
    }

    /// The `assets` the release list names for `tag`, as `[{"name": ...}]`.
    fn assets(&self, tag: &str, built: bool) -> serde_json::Value {
        let dir = self.path().join("release/download").join(format!("v{tag}"));
        let mut names: Vec<String> = match std::fs::read_dir(&dir) {
            Ok(entries) => entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => ["VERSION", "SHA256SUMS", "SHA256SUMS.sig"]
                .iter()
                .map(|name| (*name).to_owned())
                .chain([format!("bay-{}.tar.gz", target())])
                .collect(),
        };
        if !built {
            let own = format!("bay-{}.tar.gz", target());
            names.retain(|name| *name != own);
            names.push("bay-other-platform.tar.gz".to_owned());
        }
        names.sort();
        names.dedup();
        serde_json::Value::Array(
            names
                .into_iter()
                .map(|name| serde_json::json!({"name": name}))
                .collect(),
        )
    }

    /// The copied bay, with the fixture release and key.
    pub fn bay(&self, dir: &Path) -> Command {
        self.bay_at(&self.bin(), dir)
    }

    /// `program` (a bay) with the fixture's environment.
    pub fn bay_at(&self, program: &Path, dir: &Path) -> Command {
        let mut cmd = Command::new(program);
        cmd.arg("-C").arg(dir);
        let path = format!(
            "{}:{}:/bin:/usr/bin",
            self.path().join("fakebin").display(),
            self.home().join(".local/bin").display()
        );
        cmd.env_clear()
            .env("PATH", path)
            .env("HOME", self.home())
            .env("XDG_STATE_HOME", self.path().join("state"))
            .env("GIT_CONFIG_GLOBAL", self.global_config())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", self.path())
            .env(
                "BAY_TEST_RELEASE_URL",
                format!("file://{}", self.path().join("release").display()),
            )
            .env("BAY_TEST_RELEASE_KEY", &self.public);
        cmd
    }

    pub fn update(&self, dir: &Path, args: &[&str]) -> (i32, serde_json::Value) {
        let out = self.bay(dir).arg("update").args(args).output_busy();
        let text = String::from_utf8(out.stdout).unwrap();
        let value = serde_json::from_str(&text).unwrap_or_else(|_| {
            panic!(
                "stdout is one bay.result/1: {text}\nstderr: {}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.code().unwrap(), value)
    }

    /// [`Fixture::update`] with `keys` (one public key per line) as the pinned release keys.
    pub fn update_with_keys(
        &self,
        dir: &Path,
        args: &[&str],
        keys: &str,
    ) -> (i32, serde_json::Value) {
        let out = self
            .bay(dir)
            .env("BAY_TEST_RELEASE_KEY", keys)
            .arg("update")
            .args(args)
            .output_busy();
        let text = String::from_utf8(out.stdout).unwrap();
        let value = serde_json::from_str(&text).unwrap_or_else(|_| panic!("one result: {text}"));
        (out.status.code().unwrap(), value)
    }

    /// A repository whose `.bay/config.toml` is `config`.
    pub fn repo(&self, config: &str) -> PathBuf {
        self.repo_named("repo", config)
    }

    pub fn repo_named(&self, name: &str, config: &str) -> PathBuf {
        let repo = self.path().join(name);
        run(Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1"));
        std::fs::create_dir_all(repo.join(".bay")).unwrap();
        std::fs::write(repo.join(".bay/config.toml"), config).unwrap();
        // bay reads the config committed at the trunk tracking ref (ruling: config location):
        // commit it and name that ref the remote's default branch, as a clone of it would.
        let git = |args: &[&str]| {
            run(Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@t.invalid"])
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1"))
        };
        git(&["add", ".bay/config.toml"]);
        git(&["commit", "-q", "-m", "config"]);
        git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ]);
        repo
    }

    /// The copied binary is byte-identical to the build, and no temporary download is left.
    pub fn assert_untouched(&self) {
        assert!(
            std::fs::read(self.bin()).unwrap() == std::fs::read(env!("CARGO_BIN_EXE_bay")).unwrap(),
            "the installed binary changed"
        );
        assert_no_leftovers(self);
    }
}

pub fn assert_no_leftovers(fixture: &Fixture) {
    let left: Vec<_> = std::fs::read_dir(fixture.home().join(".local/bin"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name != "bay")
        .collect();
    assert!(left.is_empty(), "left beside the binary: {left:?}");
}
