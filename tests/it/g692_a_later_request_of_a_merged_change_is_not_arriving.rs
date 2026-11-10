//! G692 (a fleet finding, 2026-11-10: `bay queue` said the line's longest wait was 1,414
//! minutes, for a later revision of a change that had merged). An older bay let a merged change
//! take a new revision and a new merge request, and left its state `ready`. No runner runs that
//! request, as a change with a published operation has ended; `bay queue` listed it as arriving
//! all the same, so the line looked stuck for a day while nothing waited.
//!
//! Guard to mutate: in `src/line.rs` `waits`, the `published` arm (the merged change is listed as
//! arriving, with a wait that only grows).

#![cfg(unix)]

use crate::support;

use support::{TestBox, unchecked_files};

#[test]
fn a_later_request_of_a_merged_change_is_not_arriving() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let (id, view) = test_box.start_and_edit(&repo);
    test_box.bay_ok(&view, &["merge", "--wait"]);

    // GIVEN the record an older bay left: the merged change `ready` again, with a later
    // revision's request still submitted
    let chain = format!("refs/bay/changes/{id}");
    let writer = test_box.clone_of(&remote, "writer");
    test_box.git_ok(
        &writer,
        &["fetch", "-q", "origin", &format!("{chain}:{chain}")],
    );
    test_box.git_ok(&writer, &["checkout", "-q", "--detach", &chain]);
    let file = writer.join("change.json");
    let mut record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let request = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let mut later = record["operations"][0].clone();
    later["op_id"] = format!("{id}.2").into();
    later["request_id"] = request.clone().into();
    later["state"] = "submitted".into();
    later["epoch"] = 0.into();
    later.as_object_mut().unwrap().remove("attempts");
    later.as_object_mut().unwrap().remove("owner");
    record["state"] = "ready".into();
    record["operations"].as_array_mut().unwrap().push(later);
    std::fs::write(&file, serde_json::to_string_pretty(&record).unwrap()).unwrap();
    test_box.git_ok(
        &writer,
        &["commit", "-q", "-am", &format!("request {id}.2")],
    );
    test_box.git_ok(&writer, &["push", "-q", "origin", &format!("HEAD:{chain}")]);
    test_box.git_bare(
        &remote,
        &["update-ref", &format!("refs/bay/requests/{id}"), &request],
    );

    // WHEN another clone reads the line THEN nothing arrives and nothing waits
    let observer = test_box.clone_of(&remote, "observer");
    let line = test_box.bay_ok(&observer, &["queue"]);
    assert_eq!(line["detail"]["arriving"], serde_json::json!([]), "{line}");
    assert_eq!(line["detail"]["unread"], serde_json::json!([]), "{line}");
    assert_eq!(
        line["detail"]["oldest_wait_s"],
        serde_json::Value::Null,
        "{line}"
    );
}
