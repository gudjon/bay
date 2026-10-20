//! G151. `scripts/release.sh` takes a signature made by an external signer (a hardware key
//! that signs only with a touch): `attach-sig` copies it in only when it verifies against a
//! key in `scripts/release/allowed_signers`, and `publish` refuses a release whose
//! `SHA256SUMS.sig` is absent or does not verify. The script runs in a throwaway repository
//! whose allowed-signers file lists a throwaway key A; no build and no network is involved.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const VERSION: &str = "9.9.9";

struct Release {
    root: TempDir,
}

fn ok(cmd: &mut Command) -> Output {
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{cmd:?}: {out:?}");
    out
}

impl Release {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(repo.join("scripts/release")).unwrap();
        std::fs::create_dir_all(root.path().join("keys")).unwrap();
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/release.sh"),
            repo.join("scripts/release.sh"),
        )
        .unwrap();
        std::fs::write(
            repo.join("Cargo.toml"),
            format!("[package]\nname = \"x\"\nversion = \"{VERSION}\"\n"),
        )
        .unwrap();
        std::fs::write(
            repo.join("CHANGELOG.md"),
            format!("# Changelog\n\n## {VERSION}\nPre-release.\n"),
        )
        .unwrap();
        let release = Self { root };
        let a = release.keygen("a");
        std::fs::write(
            repo.join("scripts/release/allowed_signers"),
            format!(
                "bay-release namespaces=\"bay-release\" {}\n",
                public_line(&a)
            ),
        )
        .unwrap();
        release.git(&["init", "-q"]);
        release.git(&["add", "."]);
        release.git(&["commit", "-q", "-m", "release fixture"]);
        let out = release.out();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(
            out.join("SHA256SUMS"),
            format!("{}  VERSION\n", "0".repeat(64)),
        )
        .unwrap();
        release
    }

    fn repo(&self) -> PathBuf {
        self.root.path().join("repo")
    }

    fn out(&self) -> PathBuf {
        self.repo().join("target/release").join(VERSION)
    }

    fn git(&self, args: &[&str]) {
        ok(Command::new("git")
            .arg("-C")
            .arg(self.repo())
            .args(["-c", "user.name=t", "-c", "user.email=t@t.invalid"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1"));
    }

    fn keygen(&self, name: &str) -> PathBuf {
        let key = self.root.path().join("keys").join(name);
        ok(Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key));
        key
    }

    /// A signature over the release's SHA256SUMS by `key`, outside the release directory.
    fn signed_by(&self, key: &Path) -> PathBuf {
        let name = key.file_name().unwrap().to_string_lossy().into_owned();
        let sums = self.root.path().join(format!("sums-{name}"));
        std::fs::copy(self.out().join("SHA256SUMS"), &sums).unwrap();
        ok(Command::new("ssh-keygen")
            .args(["-Y", "sign", "-q", "-n", "bay-release", "-f"])
            .arg(key)
            .arg(&sums));
        sums.with_file_name(format!("sums-{name}.sig"))
    }

    fn script(&self, args: &[&str]) -> (i32, String) {
        let out = Command::new("sh")
            .arg("scripts/release.sh")
            .args(args)
            .current_dir(self.repo())
            .env("HOME", self.root.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        (
            out.status.code().unwrap(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

fn public_line(key: &Path) -> String {
    let text = std::fs::read_to_string(key.with_extension("pub")).unwrap();
    text.split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn attach_sig_takes_only_a_signature_by_a_listed_key() {
    let release = Release::new();
    let b = release.keygen("b");
    let (code, stderr) = release.script(&[
        "attach-sig",
        VERSION,
        release.signed_by(&b).to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("does not verify"), "{stderr}");
    assert!(!release.out().join("SHA256SUMS.sig").exists());

    let a = release.root.path().join("keys/a");
    let signature = release.signed_by(&a);
    let (code, stderr) = release.script(&["attach-sig", VERSION, signature.to_str().unwrap()]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        std::fs::read(release.out().join("SHA256SUMS.sig")).unwrap(),
        std::fs::read(&signature).unwrap()
    );
}

#[test]
fn publish_refuses_an_absent_or_unverified_signature() {
    let release = Release::new();
    let (code, stderr) = release.script(&["publish", VERSION]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("SHA256SUMS.sig is absent"), "{stderr}");

    let b = release.keygen("b");
    std::fs::copy(release.signed_by(&b), release.out().join("SHA256SUMS.sig")).unwrap();
    let (code, stderr) = release.script(&["publish", VERSION]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("does not verify"), "{stderr}");
}
