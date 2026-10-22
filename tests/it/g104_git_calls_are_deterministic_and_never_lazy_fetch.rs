//! G104: bay reads Git the same way on every box (`GIT-FEATURES` § 3).
//!
//! bay judges some of Git's answers by their words (a path that is no repository, a rejected
//! push, a held lock), so every Git call runs in the C locale: a box whose locale translates
//! Git's messages (here `LANGUAGE=de`) reads them as any other box does. A box's own
//! `log.showSignature` never adds a signature's verification to the lines bay reads. The
//! card's other half, that metadata reads never fetch a blob lazily, is tested with the
//! blob-less store (G104b).

use crate::support;

use support::{TestBox, result_json, unchecked_files};

#[test]
fn outside_a_repository_reads_alike_whatever_the_boxs_language() {
    let test_box = TestBox::new();
    let outside = test_box.path().join("not-a-repository");
    std::fs::create_dir_all(&outside).unwrap();
    for language in [None, Some("de")] {
        let mut cmd = test_box.bay(&outside);
        if let Some(language) = language {
            cmd.env("LANG", "en_US.UTF-8").env("LANGUAGE", language);
        }
        let (_, doctor) = result_json(cmd.arg("doctor"));
        assert_eq!(
            doctor["detail"]["repository"],
            serde_json::Value::Null,
            "{doctor}"
        );
        assert!(
            doctor.to_string().contains("not in a git repository"),
            "{language:?}: {doctor}"
        );
    }
}

/// A box that sets `log.showSignature` reads a signed trunk commit as any other box does: Git
/// would print the signature's verification among the `log` lines bay parses.
#[test]
fn a_boxs_log_show_signature_never_reaches_what_bay_reads() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    test_box.git_ok(
        test_box.path(),
        &["config", "--global", "log.showSignature", "true"],
    );
    let cursor = test_box.bay_ok(&repo, &["status", "--since"])["detail"]["cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let key = test_box.keys().agent;
    std::fs::write(repo.join("outside.txt"), "outside\n").unwrap();
    test_box.git_ok(&repo, &["add", "outside.txt"]);
    test_box.git_ok(
        &repo,
        &[
            "-c",
            "gpg.format=ssh",
            "-c",
            &format!("user.signingKey={}", key.display()),
            "commit",
            "-q",
            "-S",
            "-m",
            "outside",
        ],
    );
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);
    let signed = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);

    let watched = test_box.bay_ok(&repo, &["status", "--since", &cursor, "--merged"]);
    let landings = watched["detail"]["landings"].as_array().unwrap();
    assert_eq!(landings.len(), 1, "{watched}");
    assert_eq!(landings[0]["commit"], signed.as_str(), "{watched}");
    assert_eq!(landings[0]["outside_bay"], true, "{watched}");
}
