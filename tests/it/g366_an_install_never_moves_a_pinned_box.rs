//! G366 (owner ruling 2026-10-25, "fix+mitigate"; a fleet rollout's finding: a test loop's
//! `bay install` replaced a shared runner's binary 38 times in 30 minutes). An install never
//! moves a pinned box.
//!
//! A box pinned to one version (`bay.updateVersion`, or the process's `BAY_UPDATE_VERSION`) is
//! moved only by its pin. `bay install` run by a bay of another version is refused
//! `UPDATE_PINNED` before it writes anything, naming the pin and the binary it leaves alone. A
//! pin that names this version, and no pin at all, install as before.
//!
//! Guard to mutate: `src/install.rs` `pinned_elsewhere`: return `Ok(None)` at once (the pinned
//! box's binary is replaced).

use crate::support;

use support::{TestBox, result_json};

fn pin(test_box: &TestBox, version: &str) {
    std::fs::write(
        &test_box.global_config,
        format!("[bay]\n\tupdateVersion = {version}\n"),
    )
    .unwrap();
}

#[test]
fn a_box_pinned_to_another_version_keeps_its_binary() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    pin(&test_box, "0.0.1");
    let binary = test_box.local_bin().join("bay");
    std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
    std::fs::write(&binary, "the pinned bay\n").unwrap();

    let (code, refused) = result_json(test_box.bay(&repo).arg("install"));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "UPDATE_PINNED", "{refused}");
    assert_eq!(refused["detail"]["pinned"], "0.0.1", "{refused}");
    assert_eq!(
        std::fs::read_to_string(&binary).unwrap(),
        "the pinned bay\n",
        "the pinned box's binary was replaced"
    );
}

#[test]
fn a_pin_naming_this_version_or_no_pin_installs() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    pin(&test_box, env!("CARGO_PKG_VERSION"));
    let (code, installed) = result_json(test_box.bay(&repo).arg("install"));
    assert_eq!(code, 0, "{installed}");
    assert!(test_box.local_bin().join("bay").is_file());

    let other = TestBox::new();
    let repo = other.repo_any("repo");
    let (code, installed) = result_json(other.bay(&repo).arg("install"));
    assert_eq!(code, 0, "{installed}");
}
