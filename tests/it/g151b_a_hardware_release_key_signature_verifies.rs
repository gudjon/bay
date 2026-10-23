//! G151b. A release signed by any key in bay's pinned list verifies, and nothing else does. The
//! fixture is a real signature by the hardware-held release key (a secure enclave key, shown to
//! SSH as `sk-ecdsa-sha2-nistp256@openssh.com`), made with a touch over `sk-test.txt` in the
//! namespace `bay-test`, so it can never pass as a release signature (namespace `bay-release`).
//! The allowed-signers file is the one bay writes from `RELEASE_KEYS`.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use bay::update::{RELEASE_KEYS, allowed_signers};
use tempfile::TempDir;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/release-keys")
        .join(name)
}

/// `ssh-keygen -Y verify` of `data` and `sig` against an allowed-signers file of `keys`, in
/// `namespace`, as bay verifies a release.
fn verifies(keys: &[String], namespace: &str, data: &Path, sig: &Path) -> bool {
    let dir = TempDir::new().unwrap();
    let allowed = dir.path().join("allowed_signers");
    std::fs::write(&allowed, allowed_signers(keys)).unwrap();
    Command::new("ssh-keygen")
        .args(["-Y", "verify", "-q", "-f"])
        .arg(&allowed)
        .args(["-I", "bay-release", "-n", namespace, "-s"])
        .arg(sig)
        .stdin(std::fs::File::open(data).unwrap())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn pinned() -> Vec<String> {
    RELEASE_KEYS.iter().map(|key| (*key).to_owned()).collect()
}

#[test]
fn the_hardware_keys_signature_verifies_only_under_the_pinned_list() {
    let (data, sig) = (fixture("sk-test.txt"), fixture("sk-test.txt.sig"));
    assert!(
        verifies(&pinned(), "bay-test", &data, &sig),
        "the hardware key's real signature does not verify under RELEASE_KEYS"
    );
    // The old key alone does not trust it.
    assert!(!verifies(&pinned()[..1], "bay-test", &data, &sig));
    // A test-namespace signature never passes as a release signature.
    assert!(!verifies(&pinned(), "bay-release", &data, &sig));
}

#[test]
fn a_signature_by_a_key_not_pinned_is_refused() {
    let dir = TempDir::new().unwrap();
    let key = dir.path().join("other");
    let made = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .status()
        .unwrap();
    assert!(made.success());
    let data = dir.path().join("SHA256SUMS");
    std::fs::write(&data, "not a release\n").unwrap();
    let signed = Command::new("ssh-keygen")
        .args(["-Y", "sign", "-q", "-n", "bay-release", "-f"])
        .arg(&key)
        .arg(&data)
        .status()
        .unwrap();
    assert!(signed.success());
    let sig = dir.path().join("SHA256SUMS.sig");
    assert!(!verifies(&pinned(), "bay-release", &data, &sig));
}
