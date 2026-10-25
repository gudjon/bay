//! G369 (BACKLOG 66 E, owner rulings 2026-10-24 and 2026-10-25: large files never go in Git; the
//! store is derived from the remote). A large file is kept in the remote's own store.
//!
//! With `[plugin.assets]` naming no `command`, bay is its own assets plugin (`bay assets store`,
//! the plugin protocol), and the store is the remote's: a clone that can push needs no store setup
//! of its own.
//! - A path remote keeps the bytes beside the repository, in `bay/store/objects/sha256/<hex>`; a
//!   saved file over the limit goes there, never into Git, and another clone gets it at install.
//! - An ssh remote keeps them on the host, through Git's own ssh command: `bay assets store
//!   <path>` runs there, with the bytes over the connection.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/assets.rs` `own_store`: return `None` at once (with no command, nothing is stored).
//! - `src/assets/store.rs` `run`: answer an ssh remote with the path case here (the store is
//!   never asked over ssh).

// The fake ssh is a shell script, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use crate::g340_a_large_file_saved_never_enters_git::{MIB, sha256_hex, video};
use support::{TestBox, UNCHECKED_POLICY, result_json};

/// A main checkout whose policy limits a file to 1 MiB and whose config offloads to the remote's
/// own store (no command). Returns the box, the checkout and the remote.
fn trunk() -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[
            (
                ".bay/policy.toml".to_owned(),
                format!("{UNCHECKED_POLICY}\n[limits]\nmax_file_mib = 1\n"),
            ),
            (
                ".bay/config.toml".to_owned(),
                "[plugin.assets]\nover_limit = \"offload\"\n".to_owned(),
            ),
        ],
    );
    (test_box, repo, remote)
}

fn object(remote: &Path, bytes: &[u8]) -> PathBuf {
    remote
        .join("bay/store/objects/sha256")
        .join(sha256_hex(bytes))
}

#[test]
fn a_path_remote_keeps_the_bytes_and_another_clone_gets_them() {
    let (test_box, repo, remote) = trunk();
    let bytes = video();
    std::fs::create_dir_all(repo.join("video")).unwrap();
    std::fs::write(repo.join("video/clip.mov"), &bytes).unwrap();
    let (code, saved) = result_json(test_box.bay(&repo).args(["save", "add the video"]));
    assert_eq!(code, 0, "{saved}");
    assert_eq!(saved["decision"], "merged", "{saved}");
    assert_eq!(
        std::fs::read(object(&remote, &bytes)).ok().as_deref(),
        Some(bytes.as_slice()),
        "the remote's store does not hold the bytes"
    );
    let manifest = test_box.git_bare(&remote, &["show", "main:.bay-assets"]);
    assert!(manifest.contains("video/clip.mov"), "{manifest}");
    assert!(bytes.len() > MIB);

    let clone = test_box.clone_of(&remote, "second");
    test_box.bay_ok(&clone, &["install"]);
    assert_eq!(std::fs::read(clone.join("video/clip.mov")).unwrap(), bytes);
}

/// An `ssh` that runs on this machine: it drops its options and the host, and runs the remote
/// command here, with `bay` meaning the bay under test.
const FAKE_SSH: &str = "#!/bin/sh\nwhile [ $# -gt 0 ]; do case \"$1\" in -o|-p|-i|-l) shift 2;; -*) shift;; *) break;; esac; done\nshift\ncmd=\"$*\"\necho \"$cmd\" >> \"$FAKE_SSH_LOG\"\ncase \"$cmd\" in bay\\ *) cmd=\"$FAKE_BAY${cmd#bay}\";; esac\nexec sh -c \"$cmd\"\n";

#[test]
fn an_ssh_remote_keeps_the_bytes_on_its_host() {
    let (test_box, _repo, remote) = trunk();
    let ssh = test_box.path().join("fake-ssh");
    std::fs::write(&ssh, FAKE_SSH).unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let log = test_box.path().join("ssh.log");
    let clone = test_box.clone_of(&remote, "over-ssh");
    test_box.git_ok(
        &clone,
        &[
            "remote",
            "set-url",
            "origin",
            &format!("somehost:{}", remote.display()),
        ],
    );
    let bytes: Vec<u8> = video().iter().map(|byte| byte ^ 0x5a).collect();
    std::fs::create_dir_all(clone.join("video")).unwrap();
    std::fs::write(clone.join("video/take2.mov"), &bytes).unwrap();
    let (code, saved) = result_json(
        test_box
            .bay(&clone)
            .env("GIT_SSH_COMMAND", &ssh)
            .env("FAKE_BAY", env!("CARGO_BIN_EXE_bay"))
            .env("FAKE_SSH_LOG", &log)
            .args(["save", "add the second take"]),
    );
    assert_eq!(code, 0, "{saved}");
    assert_eq!(saved["decision"], "merged", "{saved}");
    assert_eq!(
        std::fs::read(object(&remote, &bytes)).ok().as_deref(),
        Some(bytes.as_slice()),
        "the host's store does not hold the bytes"
    );
    // The bytes went over the connection, to `bay assets store` on the host.
    let ran = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        ran.lines()
            .any(|line| line.starts_with("bay assets store ")),
        "the store was never asked over ssh: {ran}"
    );
}
