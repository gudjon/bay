//! Shard indexes in tests (ruling R11, G921, G923): where a compacted name is recorded, the
//! shard a runner moves next, two runners over a ref budget, and shard commits written
//! on a host directly, past its hook, as old compactions left them.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::Digest as _;

use super::{
    AGENT, EXECUTOR, PRODUCER, REGISTRAR, TestBox, check_files, key_line, passing_result,
    result_json,
};

/// The shard of `name`: the first byte of sha256(name).
#[must_use]
pub fn shard(name: &str) -> u8 {
    sha2::Sha256::digest(name.as_bytes())[0]
}

/// The shard index that records `name`.
#[must_use]
pub fn index(name: &str) -> String {
    format!("refs/bay/landed-index/{}", shard(name))
}

/// The path that records `name` in its shard (G923): the second byte of sha256(name) in hex,
/// then the name with `/` as `%2F`.
#[must_use]
pub fn path(name: &str) -> String {
    let digest = sha2::Sha256::digest(name.as_bytes());
    format!("{:02x}/{}", digest[1], name.replace('/', "%2F"))
}

/// The refs of one change that move once retention cut it, its chain first: its chain and its
/// request, and a landed change's marker, claim and landed claim.
pub type Change = Vec<String>;

/// The names of `changes` the host still holds (`left`) that a runner may move now: a chain only
/// once the host holds no other ref of its change (G921).
#[must_use]
pub fn movable(changes: &[Change], left: &BTreeSet<String>) -> Vec<String> {
    let mut names = Vec::new();
    for change in changes {
        let Some((chain, rest)) = change.split_first() else {
            continue;
        };
        let others: Vec<&String> = rest.iter().filter(|name| left.contains(*name)).collect();
        if others.is_empty() && left.contains(chain) {
            names.push(chain.clone());
        }
        names.extend(others.into_iter().cloned());
    }
    names
}

/// The shard runner `runner` (its name) moves from the shards of `names` (BACKLOG 353): the one
/// its order puts first, the highest sha256 of its presence ref, a NUL and the shard number.
#[must_use]
pub fn name_shard(names: &[String], runner: &str) -> Option<u8> {
    let me = format!("refs/bay/executor/{runner}");
    names.iter().map(|name| shard(name)).max_by_key(|&number| {
        let mut input = me.as_bytes().to_vec();
        input.extend([0, number]);
        let mut weight = [0; 32];
        weight.copy_from_slice(&sha2::Sha256::digest(&input));
        weight
    })
}

/// The names of `changes` the host still holds as refs.
#[must_use]
pub fn remaining(test_box: &TestBox, host: &Path, changes: &[Change]) -> BTreeSet<String> {
    let listed = test_box.git_bare(host, &["for-each-ref", "--format=%(refname)", "refs/bay/"]);
    let held: BTreeSet<&str> = listed.lines().collect();
    changes
        .iter()
        .flatten()
        .filter(|name| held.contains(name.as_str()))
        .cloned()
        .collect()
}

/// One pass of the runner in `clone`, live for the next 300 s.
pub fn pass(test_box: &TestBox, clone: &Path) -> Value {
    let (code, ran) =
        result_json(
            test_box
                .bay(clone)
                .args(["runner", "run", "--once", "--interval-s", "300"]),
        );
    assert_eq!(code, 0, "{ran}");
    ran
}

/// The names a pass moved, and the errors it reported.
#[must_use]
pub fn moved(ran: &Value) -> (BTreeSet<String>, Vec<Value>) {
    let mut names = BTreeSet::new();
    let mut errors = Vec::new();
    for entry in ran["detail"]["ran"].as_array().into_iter().flatten() {
        if entry.get("error").is_some() {
            errors.push(entry.clone());
        }
        for name in entry["compacted"].as_array().into_iter().flatten() {
            names.insert(name.as_str().unwrap().to_owned());
        }
    }
    (names, errors)
}

/// A host whose changes retention cuts at once, over a ref budget of 1, with two enrolled
/// runners `exec-1` and `exec-2` in their own clones.
pub struct TwoRunners {
    pub host: PathBuf,
    /// The agent's clone, which made the changes.
    pub repo: PathBuf,
    pub runners: [PathBuf; 2],
    /// The changes merged, then those abandoned.
    pub changes: Vec<Change>,
    /// The markers of each merged change, with their targets.
    pub markers: Vec<(String, String)>,
}

/// [`TwoRunners`] with `landed` changes merged and `abandoned` changes abandoned.
pub fn two_runners(test_box: &TestBox, landed: usize, abandoned: usize) -> TwoRunners {
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[retention]\nwindow_s = 0\nref_budget = 1\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let keys = test_box.keys();
    // Each runner signs its results under its own producer name, with its own key.
    let producer_2 = test_box.path().join("keys").join("producer-2");
    let made = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "producer-2", "-f"])
        .arg(&producer_2)
        .status()
        .unwrap();
    assert!(made.success());
    test_box.enroll_files(
        &repo,
        &keys,
        &[
            ("agent", key_line(AGENT, &keys.agent)),
            (
                "runner",
                key_line(EXECUTOR, &keys.executor) + &key_line("exec-2", &keys.rogue),
            ),
            (
                "producer",
                key_line(PRODUCER, &keys.producer) + &key_line("ci-2", &producer_2),
            ),
            ("registry", key_line(REGISTRAR, &keys.registry)),
        ],
    );
    test_box.use_keys(&repo, &keys);

    let mut changes = Vec::new();
    let mut markers = Vec::new();
    for n in 1..=landed + abandoned {
        let started = test_box.bay_ok(&repo, &["start", &format!("SHARD-{n}"), "--paths", "src/"]);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(
            view.join(format!("src/n{n}.rs")),
            format!("fn n{n}() {{}}\n"),
        )
        .unwrap();
        if n > landed {
            // Never submitted, an abandoned change has a chain and no request.
            let (code, ended) = result_json(test_box.bay(&view).arg("abandon"));
            assert_eq!(code, 0, "{ended}");
            changes.push(vec![format!("refs/bay/changes/{id}")]);
            continue;
        }
        let mut change = vec![
            format!("refs/bay/changes/{id}"),
            format!("refs/bay/requests/{id}"),
        ];
        let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
        assert_eq!(code, 0, "{merged}");
        let intent = started["detail"]["intent"].as_str().unwrap().to_owned();
        let landed_intent = intent.replace("refs/bay/intent/", "refs/bay/landed-intent/");
        for name in [format!("refs/bay/landed/{id}"), intent, landed_intent] {
            let target = test_box.git_bare(&host, &["rev-parse", &name]);
            markers.push((name.clone(), target));
            change.push(name);
        }
        changes.push(change);
    }

    let first = test_box.clone_of(&host, "runner-1");
    test_box.use_keys(&first, &keys);
    let second = test_box.clone_of(&host, "runner-2");
    test_box.use_keys(&second, &keys);
    for (key, value) in [
        ("bay.runnerKey", keys.rogue.to_str().unwrap()),
        ("bay.runnerName", "exec-2"),
        ("bay.producerName", "ci-2"),
        ("bay.producerKey", producer_2.to_str().unwrap()),
    ] {
        test_box.git_ok(&second, &["config", key, value]);
    }
    TwoRunners {
        host,
        repo,
        runners: [first, second],
        changes,
        markers,
    }
}

/// A git command on the bare `host`, through the index file `index`, given `input`: its output.
fn on_host(test_box: &TestBox, host: &Path, index: &Path, args: &[&str], input: &str) -> String {
    let mut child = test_box
        .git(test_box.path())
        .arg(format!("--git-dir={}", host.display()))
        .args(args)
        .env("GIT_INDEX_FILE", index)
        .env("GIT_COMMITTER_NAME", EXECUTOR)
        .env("GIT_COMMITTER_EMAIL", "exec@bay.invalid")
        .env("GIT_AUTHOR_NAME", EXECUTOR)
        .env("GIT_AUTHOR_EMAIL", "exec@bay.invalid")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), input.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Appends to each shard index of `host` the entries `entries` holds for it (a path and the
/// blob there), past the host's hook, as old compactions left them: one unsigned commit a
/// shard, written through a temporary index.
pub fn write_shards(
    test_box: &TestBox,
    host: &Path,
    entries: &BTreeMap<u8, Vec<(String, String)>>,
) {
    let index = test_box.path().join("written-shard-index");
    for (shard, entries) in entries {
        let _ = std::fs::remove_file(&index);
        let name = format!("refs/bay/landed-index/{shard}");
        let tip = test_box.git_bare(host, &["for-each-ref", "--format=%(objectname)", &name]);
        if tip.is_empty() {
            on_host(test_box, host, &index, &["read-tree", "--empty"], "");
        } else {
            on_host(test_box, host, &index, &["read-tree", &tip], "");
        }
        let info = entries
            .iter()
            .fold(String::new(), |mut info, (path, blob)| {
                let _ = writeln!(info, "100644 {blob}\t{path}");
                info
            });
        on_host(
            test_box,
            host,
            &index,
            &["update-index", "--index-info"],
            &info,
        );
        let tree = on_host(test_box, host, &index, &["write-tree"], "");
        let mut commit = vec!["commit-tree", tree.as_str(), "-m", "shard"];
        if !tip.is_empty() {
            commit.extend(["-p", tip.as_str()]);
        }
        let commit = on_host(test_box, host, &index, &commit, "");
        test_box.git_bare(host, &["update-ref", &name, &commit]);
    }
    let _ = std::fs::remove_file(&index);
}
