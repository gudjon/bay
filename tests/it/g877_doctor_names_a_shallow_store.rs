//! G877 (a measurement, 2026-11-23): `bay doctor` names a shallow store.
//!
//! A machine's shared store kept a shallow boundary of 17 commits whose history it held in full;
//! each push from it packed against the boundary for about 4 s of a 5 to 13 s push. The `store`
//! row names a shallow store, the commits in its boundary, and the command that ends it.
//! Advisory: a shallow clone can be the site's choice.
//!
//! Guard to mutate: `src/doctor.rs` `shallow_entries`: answer `None` (no store reads as shallow).

use crate::support;

use serde_json::Value;
use support::{TestBox, result_json, row};

fn store(test_box: &TestBox, dir: &std::path::Path) -> Value {
    let (_, doctor) = result_json(test_box.bay(dir).arg("doctor"));
    assert_ne!(doctor["decision"], "blocked", "{doctor}");
    let store = row(&doctor, "store").clone();
    assert_ne!(store["state"], "red", "{doctor}");
    store
}

#[test]
fn doctor_names_a_shallow_store_until_it_is_unshallowed() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    test_box.seed_trunk(&repo, &[("src/b.rs", "fn b() {}\n")]);
    let full = store(&test_box, &repo);
    assert_eq!(full["shallow"], Value::Null, "{full}");
    assert!(!full["notes"].to_string().contains("shallow"), "{full}");

    // A fetch with a depth leaves a boundary, though every commit is here.
    test_box.git_ok(&repo, &["fetch", "-q", "--depth", "1", "origin", "main"]);
    let shallow = store(&test_box, &repo);
    assert_eq!(shallow["shallow"], 1, "{shallow}");
    assert_eq!(shallow["state"], "advisory", "{shallow}");
    assert!(
        shallow["notes"]
            .to_string()
            .contains("a shallow store (1 in its boundary)"),
        "{shallow}"
    );

    // The command the note names ends it.
    test_box.git_ok(&repo, &["fetch", "-q", "--unshallow", "origin"]);
    let unshallowed = store(&test_box, &repo);
    assert_eq!(unshallowed["shallow"], Value::Null, "{unshallowed}");
}
