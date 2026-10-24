//! G151. `scripts/release.sh` takes a signature made by an external signer (a hardware key
//! that signs only with a touch): `attach-sig` copies it in only when it verifies against a
//! key in `scripts/release/allowed_signers`, and `publish` refuses a release whose
//! `SHA256SUMS.sig` is absent or does not verify. `publish` tags a commit on main, or a patch of
//! an older minor on its own line, `origin/release-<MAJOR>.<MINOR>`. The script runs in a
//! throwaway repository whose allowed-signers file lists a throwaway key A; no build and no
//! network is involved.
//!
//! Guard to mutate for the release line: `scripts/release.sh` `publish`: drop the
//! `origin/release-$minor` alternative (a patch of an older minor is refused).

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
        std::fs::write(out.join("VERSION"), format!("{VERSION}\n")).unwrap();
        let sums = Command::new("shasum")
            .args(["-a", "256", "VERSION"])
            .current_dir(&out)
            .output()
            .unwrap();
        std::fs::write(out.join("SHA256SUMS"), sums.stdout).unwrap();
        std::fs::write(out.join("COMMIT"), format!("{}\n", release.head())).unwrap();
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

    fn head(&self) -> String {
        let out = ok(Command::new("git")
            .arg("-C")
            .arg(self.repo())
            .args(["rev-parse", "HEAD"]));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
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
        // A `gh` that records its arguments and the notes file, in place of the real one.
        let bin = self.root.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let gh = bin.join("gh");
        std::fs::write(
            &gh,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {log}/gh-args\nwhile [ $# -gt 0 ]; do [ \"$1\" = --notes-file ] && cp \"$2\" {log}/gh-notes; shift; done\n",
                log = self.root.path().display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let out = Command::new("sh")
            .arg("scripts/release.sh")
            .args(args)
            .current_dir(self.repo())
            .env("PATH", path)
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

/// publish tags the commit build recorded and reads its notes there, so trunk can move while an
/// external signature is made; a release directory without that record is refused.
#[test]
fn publish_tags_the_built_commit_after_trunk_moved() {
    let release = Release::new();
    let built = release.head();
    let origin = release.root.path().join("origin.git");
    ok(Command::new("git")
        .arg("init")
        .arg("-q")
        .arg("--bare")
        .arg(&origin));
    release.git(&["remote", "add", "origin", origin.to_str().unwrap()]);
    std::fs::write(release.repo().join("later.txt"), "later\n").unwrap();
    release.git(&["add", "later.txt"]);
    std::fs::write(
        release.repo().join("CHANGELOG.md"),
        "# Changelog\n\n## 9.9.10\nLater.\n\n## 9.9.9\nMoved on.\n",
    )
    .unwrap();
    release.git(&["commit", "-qam", "trunk moves on"]);
    release.git(&["push", "-q", "origin", "HEAD:refs/heads/main"]);
    assert_ne!(release.head(), built);

    let a = release.root.path().join("keys/a");
    let (code, stderr) = release.script(&[
        "attach-sig",
        VERSION,
        release.signed_by(&a).to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{stderr}");
    let (code, stderr) = release.script(&["publish", VERSION]);
    assert_eq!(code, 0, "{stderr}");
    let args = std::fs::read_to_string(release.root.path().join("gh-args")).unwrap();
    let args: Vec<&str> = args.lines().collect();
    let target = args.iter().position(|arg| *arg == "--target").unwrap();
    assert_eq!(args[target + 1], built, "{args:?}");
    let notes = std::fs::read_to_string(release.root.path().join("gh-notes")).unwrap();
    assert!(
        notes.contains("Pre-release."),
        "the built commit's notes: {notes}"
    );
    assert!(!notes.contains("Moved on."), "{notes}");
    assert!(notes.contains(&format!("Built from {built}.")), "{notes}");

    std::fs::remove_file(release.out().join("COMMIT")).unwrap();
    let (code, stderr) = release.script(&["publish", VERSION]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("COMMIT is absent"), "{stderr}");
}

/// A patch of an older minor is built on its own line (0.3.8 on release-0.3 while main holds
/// 0.4): publish takes a built commit on `origin/release-<MAJOR>.<MINOR>` of the version's own
/// minor, and refuses one on neither that line nor main (the Mac's report, 2026-10-24).
#[test]
fn publish_takes_a_commit_on_its_minors_release_line() {
    let release = Release::new();
    let built = release.head();
    let origin = release.root.path().join("origin.git");
    ok(Command::new("git")
        .arg("init")
        .arg("-q")
        .arg("--bare")
        .arg(&origin));
    release.git(&["remote", "add", "origin", origin.to_str().unwrap()]);
    // main is another line that never held the built commit.
    release.git(&["checkout", "-q", "--orphan", "other"]);
    release.git(&["commit", "-q", "-m", "main of the next minor"]);
    release.git(&["push", "-q", "origin", "HEAD:refs/heads/main"]);
    release.git(&["checkout", "-q", "-f", &built]);
    let a = release.root.path().join("keys/a");
    let (code, stderr) = release.script(&[
        "attach-sig",
        VERSION,
        release.signed_by(&a).to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{stderr}");

    let (code, stderr) = release.script(&["publish", VERSION]);
    assert_eq!(
        code, 1,
        "a commit on no release line was published: {stderr}"
    );
    assert!(
        stderr.contains("on neither origin/main nor origin/release-9.9"),
        "{stderr}"
    );

    release.git(&[
        "push",
        "-q",
        "origin",
        &format!("{built}:refs/heads/release-9.9"),
    ]);
    let (code, stderr) = release.script(&["publish", VERSION]);
    assert_eq!(code, 0, "{stderr}");
}
