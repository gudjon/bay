//! G297 (N-21): two machines land under approvals-only signing with no names set.
//!
//! `[policy] signing = "approvals"` promises a small team lands from their own machines with no
//! key and no name to set. Each machine writes its check evidence under its producer name, and
//! one name has one writer. So a clone with no `bay.producerName` takes a name of its own,
//! `local-<id>`, the first time it writes unsigned evidence, and keeps it in its own git
//! config: the second machine's landing never meets the first machine's evidence ref, and each
//! reads the other's records as another writer's. A command that only reads writes no config.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/signing.rs` `own_producer_name`: return `"local".to_owned()` first (the second
//!   machine's landing fails: producer `local` collides).
//! - `src/publish.rs` `Ctx::producer`: `self.keys.producer_name.clone()` for the unsigned name
//!   (the second machine's landing fails the same way).
//! - `src/signing.rs` `own_producer_name`: write nothing, `Ok(_) => name` for every outcome
//!   without the `git config` (each command takes a new name, and the clone keeps none).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::UNSANDBOXED_CHOICE;
use support::approval::{Trunk, trunk};

/// [`trunk`] on a box whose global config names no producer, as a person's machine has none.
fn unnamed_trunk() -> Trunk {
    let trunk = trunk();
    std::fs::write(&trunk.test_box.global_config, UNSANDBOXED_CHOICE).unwrap();
    trunk
}

/// Starts a change on `src/` in `dir`, writes `path`, and lands it with `--wait`.
fn land_from(trunk: &Trunk, dir: &Path, intent: &str, path: &str) -> (i32, Value) {
    let started = trunk
        .test_box
        .bay_ok(dir, &["start", intent, "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(path), "fn f() {}\n").unwrap();
    support::result_json(trunk.test_box.bay(&view).args(["merge", "--wait"]))
}

/// The clone's own `bay.producerName`.
fn producer_name(trunk: &Trunk, dir: &Path) -> String {
    trunk
        .test_box
        .git_ok(dir, &["config", "--local", "bay.producerName"])
}

#[test]
fn a_second_machine_lands_and_each_keeps_a_name_of_its_own() {
    let trunk = unnamed_trunk();
    // A read writes no name.
    trunk.test_box.bay_ok(&trunk.repo, &["status"]);
    let unset = trunk
        .test_box
        .git(&trunk.repo)
        .args(["config", "--local", "bay.producerName"])
        .output()
        .unwrap();
    assert!(!unset.status.success(), "a read wrote a producer name");

    let (code, first) = land_from(&trunk, &trunk.repo, "CARD-one", "src/one.rs");
    assert_eq!(code, 0, "{first}");
    assert_eq!(first["decision"], "merged", "{first}");

    let host = trunk.test_box.path().join("host.git");
    let second = trunk.test_box.clone_of(&host, "second-box");
    let (code, landed) = land_from(&trunk, &second, "CARD-two", "src/two.rs");
    assert_eq!(code, 0, "the second machine could not land: {landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");

    // Each clone keeps one name, its own, from one command to the next.
    let one = producer_name(&trunk, &trunk.repo);
    let two = producer_name(&trunk, &second);
    assert!(one.starts_with("local-"), "{one}");
    assert!(two.starts_with("local-"), "{two}");
    assert_ne!(one, two);
    let (code, again) = land_from(&trunk, &second, "CARD-three", "src/three.rs");
    assert_eq!(code, 0, "{again}");
    assert_eq!(producer_name(&trunk, &second), two);

    // The remote holds each machine's evidence under its own name.
    let refs = trunk.test_box.git_ok(
        &host,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/notes/bay/evidence/",
        ],
    );
    assert!(
        refs.contains(&format!("refs/notes/bay/evidence/{one}")),
        "{refs}"
    );
    assert!(
        refs.contains(&format!("refs/notes/bay/evidence/{two}")),
        "{refs}"
    );
}

#[test]
fn a_name_the_machine_set_is_kept() {
    let trunk = unnamed_trunk();
    trunk
        .test_box
        .git_ok(&trunk.repo, &["config", "bay.producerName", "studio"]);
    let (code, landed) = land_from(&trunk, &trunk.repo, "CARD-one", "src/one.rs");
    assert_eq!(code, 0, "{landed}");
    assert_eq!(producer_name(&trunk, &trunk.repo), "studio");
    let host = trunk.test_box.path().join("host.git");
    let refs = trunk.test_box.git_ok(
        &host,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/notes/bay/evidence/",
        ],
    );
    assert!(refs.contains("refs/notes/bay/evidence/studio"), "{refs}");
}
