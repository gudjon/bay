//! G104: bay reads Git the same way on every box (`GIT-FEATURES` § 3).
//!
//! bay judges some of Git's answers by their words (a path that is no repository, a rejected
//! push, a held lock), so every Git call runs in the C locale: a box whose locale translates
//! Git's messages (here `LANGUAGE=de`) reads them as any other box does. The card's other half,
//! that metadata reads never fetch a blob lazily, is tested with the blob-less store (G212).

use crate::support;

use support::{TestBox, result_json};

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
