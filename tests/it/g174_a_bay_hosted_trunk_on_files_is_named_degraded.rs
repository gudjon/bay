//! G174: a bay-hosted trunk on the `files` ref backend is named degraded (`DESIGN` § 15.2).
//!
//! `bay host init` makes a reftable repository, where a multi-ref update is one atomic write.
//! On `files` a reader can see half of one. Doctor names that on the host, with the remedy,
//! as advisory: publication still works there, through trunk as the commit point.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json, row};

#[test]
fn a_host_on_files_is_degraded_with_the_remedy_and_still_publishes() {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let (_, doctor) = result_json(test_box.bay(&host).arg("doctor"));
    let on_reftable = row(&doctor, "ref-format");
    assert_eq!(on_reftable["ref_format"], "reftable", "{doctor}");
    assert_eq!(on_reftable["state"], "green", "{doctor}");

    test_box.git_bare(&host, &["refs", "migrate", "--ref-format=files"]);
    let (code, doctor) = result_json(test_box.bay(&host).arg("doctor"));
    let on_files = row(&doctor, "ref-format");
    assert_eq!(on_files["ref_format"], "files", "{doctor}");
    assert_eq!(on_files["state"], "degraded", "{doctor}");
    assert!(
        on_files["note"]
            .as_str()
            .is_some_and(|note| note.contains("half of a multi-ref update")),
        "{doctor}"
    );
    assert_eq!(
        on_files["recovery"], "git refs migrate --ref-format=reftable",
        "{doctor}"
    );
    assert_eq!(code, 0, "advisory, never red: {doctor}");
    assert_ne!(doctor["decision"], "blocked", "{doctor}");

    // Publication still works on the files host.
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(
        test_box.git_bare(&host, &["rev-parse", "main"]),
        landed["detail"]["commit"].as_str().unwrap()
    );

    // A work repository's forge remote is never judged on its ref format: the row is the host's.
    let (_, doctor) = result_json(test_box.bay(&repo).arg("doctor"));
    assert!(
        doctor["detail"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["name"] != "ref-format"),
        "{doctor}"
    );
}
