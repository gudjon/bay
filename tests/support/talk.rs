//! A bay host with a key registry and three agents on three clones, for the talk G-tests
//! (`DESIGN` § 11c): `agent` on `a`, `agent-b` on `b`, `agent-c` on `c`. The machines share
//! nothing but the host; each clone pushes and fetches it.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;

use super::{TestBox, TestKeys, result_json};

pub struct TalkCase {
    pub test_box: TestBox,
    pub host: PathBuf,
    pub a: PathBuf,
    pub b: PathBuf,
    pub c: PathBuf,
    pub keys: TestKeys,
    pub b_key: PathBuf,
    pub c_key: PathBuf,
}

/// A new ed25519 key at `<root>/keys/<name>`, enrolled nowhere yet.
pub fn fresh_key(test_box: &TestBox, name: &str) -> PathBuf {
    let path = test_box.path().join("keys").join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let status = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    path
}

/// The time `secs` from now, as bay writes times.
#[must_use]
pub fn from_now(secs: u64) -> String {
    bay::clock::rfc3339_of(bay::live::unix_now() + secs)
}

#[must_use]
pub fn hosted_talk() -> TalkCase {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let a = test_box.repo_without_remote_any("a");
    test_box.git_ok(&a, &["remote", "add", "origin", host.to_str().unwrap()]);
    test_box.seed_trunk(&a, &[("src/a.rs", "fn a() {}\n")]);
    let keys = test_box.keys();
    let b_key = fresh_key(&test_box, "agent-b");
    let c_key = fresh_key(&test_box, "agent-c");
    test_box.enroll_with(
        &a,
        &keys,
        &keys.executor,
        &[("agent-b", &b_key), ("agent-c", &c_key)],
    );
    test_box.use_keys(&a, &keys);
    let b = test_box.clone_of(&host, "b");
    test_box.git_ok(&b, &["config", "bay.agentKey", b_key.to_str().unwrap()]);
    let c = test_box.clone_of(&host, "c");
    test_box.git_ok(&c, &["config", "bay.agentKey", c_key.to_str().unwrap()]);
    for clone in [&b, &c] {
        test_box.git_ok(
            clone,
            &[
                "fetch",
                "-q",
                "origin",
                "+refs/bay/registry:refs/bay/registry",
            ],
        );
    }
    TalkCase {
        test_box,
        host,
        a,
        b,
        c,
        keys,
        b_key,
        c_key,
    }
}

impl TalkCase {
    /// The clone each agent runs on.
    #[must_use]
    pub fn clone_of(&self, actor: &str) -> &Path {
        match actor {
            "agent" => &self.a,
            "agent-b" => &self.b,
            "agent-c" => &self.c,
            other => panic!("no clone for {other}"),
        }
    }

    /// `bay` on `actor`'s clone, as `actor` in session `s-<actor>`.
    #[must_use]
    pub fn bay_as(&self, actor: &str) -> Command {
        let mut cmd = self.test_box.bay(self.clone_of(actor));
        cmd.env("BAY_ACTOR", actor)
            .env("BAY_SESSION", format!("s-{actor}"));
        cmd
    }

    /// `bay talk <args>` as `actor`: the exit code and the result.
    pub fn talk(&self, actor: &str, args: &[&str]) -> (i32, Value) {
        result_json(self.bay_as(actor).arg("talk").args(args))
    }

    /// `bay status` as `actor`.
    pub fn status(&self, actor: &str) -> (i32, Value) {
        result_json(self.bay_as(actor).arg("status"))
    }

    /// The tip of `name` on the host, if it exists.
    #[must_use]
    pub fn host_tip(&self, name: &str) -> Option<String> {
        let out = self
            .test_box
            .git(self.test_box.path())
            .args([
                &format!("--git-dir={}", self.host.display()),
                "rev-parse",
                "--verify",
                "-q",
                name,
            ])
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8(out.stdout).unwrap().trim().to_owned())
    }

    /// The tip of `name` in `dir`, if it exists.
    #[must_use]
    pub fn local_tip(&self, dir: &Path, name: &str) -> Option<String> {
        let out = self
            .test_box
            .git(dir)
            .args(["rev-parse", "--verify", "-q", name])
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8(out.stdout).unwrap().trim().to_owned())
    }

    /// The files of `rev`'s tree in `dir` (or the host's, for `None`).
    #[must_use]
    pub fn files(&self, dir: Option<&Path>, rev: &str) -> Vec<String> {
        let text = match dir {
            Some(dir) => self
                .test_box
                .git_ok(dir, &["ls-tree", "-r", "--name-only", rev]),
            None => self
                .test_box
                .git_bare(&self.host, &["ls-tree", "-r", "--name-only", rev]),
        };
        text.lines().map(str::to_owned).collect()
    }

    /// A push of `refspec` from `dir` that the host must refuse; its stderr.
    pub fn refused(&self, dir: &Path, refspec: &str) -> String {
        let out = self
            .test_box
            .git(dir)
            .args(["push", "origin", refspec])
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "push {refspec} was accepted: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
}

/// The ref a talk to `who` with this id lives on.
#[must_use]
pub fn talk_ref(who: &str, id: &str) -> String {
    format!("refs/bay/talk/{who}/{id}")
}

/// A commit in `dir` whose tree holds `files` (path, text), on `parent`, committed at `at`, and
/// signed by `key` as `principal`: a chain shape bay's own writers never make, for the host to
/// judge.
pub fn forge(
    dir: &Path,
    key: &Path,
    principal: &str,
    parent: Option<&str>,
    files: &[(&str, &str)],
    at: u64,
) -> String {
    let git = bay::git::Git::new(dir);
    let git_dir = git.run(&["rev-parse", "--absolute-git-dir"]).unwrap();
    let index = format!("{git_dir}/bay-forge-index");
    let _ = std::fs::remove_file(&index);
    let with_index = [("GIT_INDEX_FILE", index.as_str())];
    git.run_with(&["read-tree", "--empty"], &with_index, None)
        .unwrap();
    for (path, text) in files {
        let blob = git
            .run_with(
                &["hash-object", "-w", "--stdin"],
                &[],
                Some(text.as_bytes()),
            )
            .unwrap();
        let info = format!("100644,{blob},{path}");
        git.run_with(
            &["update-index", "--add", "--cacheinfo", &info],
            &with_index,
            None,
        )
        .unwrap();
    }
    let tree = git.run_with(&["write-tree"], &with_index, None).unwrap();
    let _ = std::fs::remove_file(&index);
    let signer = bay::signing::Signer {
        key: key.to_path_buf(),
        name: principal.to_owned(),
    };
    let mut args = signer.config_args();
    args.extend(["commit-tree".to_owned(), tree, "-S".to_owned()]);
    if let Some(parent) = parent {
        args.extend(["-p".to_owned(), parent.to_owned()]);
    }
    args.extend(["-m".to_owned(), "forged".to_owned()]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let date = format!("{at} +0000");
    let mut envs = signer.identity();
    envs.push(("GIT_AUTHOR_DATE".to_owned(), date.clone()));
    envs.push(("GIT_COMMITTER_DATE".to_owned(), date));
    let envs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    git.run_with(&args, &envs, None).unwrap()
}

/// Every file of `rev` in `dir`, with its text.
#[must_use]
pub fn contents(dir: &Path, rev: &str) -> Vec<(String, String)> {
    let git = bay::git::Git::new(dir);
    git.run(&["ls-tree", "-r", "--name-only", rev])
        .unwrap()
        .lines()
        .map(|path| {
            let text = git.run(&["show", &format!("{rev}:{path}")]).unwrap();
            (path.to_owned(), text)
        })
        .collect()
}

/// The canonical text of a JSON value, as a talk file holds it.
#[must_use]
pub fn canonical(value: &Value) -> String {
    bay::canonical::canonical(value).unwrap()
}

/// The commit time of `rev` in `dir`.
#[must_use]
pub fn commit_time(dir: &Path, rev: &str) -> u64 {
    bay::git::Git::new(dir)
        .run(&["show", "-s", "--format=%ct", rev])
        .unwrap()
        .parse()
        .unwrap()
}
