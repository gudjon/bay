//! G40. The holder can abandon and another actor can adopt.
//!
//! The current actor submits request kind `abandon` on an unlanded change (`bay land
//! --abandon`; there is no `bay abandon` verb) and the state becomes `abandoned`. A second
//! actor adopts that intent with `bay start`. Another actor's abandon request is refused with
//! `ABANDON_REFUSED`. Trunk does not move. The view remains.

mod support;

use std::path::PathBuf;

use support::{TestBox, check_files, passing_result, result_json};

const TAKEOVER_S: u64 = 60;

#[test]
fn the_holder_abandons_and_another_actor_adopts() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    // A long takeover window: only the abandon can free the intent within the test.
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        format!("[executor]\ntakeover_s = {TAKEOVER_S}\n"),
    ));
    let trunk = test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let started = test_box.bay_ok(&repo, &["start", "CARD-40", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/half.rs"), "fn half() {}\n").unwrap();

    // Another actor cannot abandon it.
    let (code, refused) = result_json(
        test_box
            .bay(&view)
            .env("BAY_ACTOR", "someone-else")
            .args(["land", "--abandon"]),
    );
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "ABANDON_REFUSED");

    // The holder abandons: the state is abandoned; trunk and the view stay.
    let (code, abandoned) = result_json(test_box.bay(&view).args(["land", "--abandon"]));
    assert_eq!(code, 0, "{abandoned}");
    assert_eq!(abandoned["detail"]["state"], "abandoned");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), trunk);
    assert!(view.join("src/half.rs").is_file(), "the view remains");
    let record = test_box.git_bare(
        &remote,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    assert!(record.contains("\"state\": \"abandoned\""), "{record}");

    // A second actor adopts the intent at once (no takeover window) and lands it.
    let asked = std::time::Instant::now();
    let (code, adopted) = result_json(
        test_box
            .bay(&repo)
            .env("BAY_ACTOR", "agent-b")
            .args(["start", "CARD-40", "--paths", "src/"]),
    );
    assert_eq!(code, 0, "{adopted}");
    assert_eq!(adopted["change"], id.as_str());
    assert_eq!(adopted["detail"]["adopted"], true);
    assert!(
        asked.elapsed() < std::time::Duration::from_secs(TAKEOVER_S / 2),
        "the adoption waited for the takeover window: {:?}",
        asked.elapsed()
    );
    let (code, landed) = result_json(
        test_box
            .bay(&view)
            .env("BAY_ACTOR", "agent-b")
            .args(["land", "--wait"]),
    );
    assert_eq!(code, 0, "{landed}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id.clone()]);

    // An abandon after the landing is refused.
    let (code, late) = result_json(
        test_box
            .bay(&view)
            .env("BAY_ACTOR", "agent-b")
            .args(["land", "--abandon"]),
    );
    assert_eq!(code, 10, "{late}");
    assert_eq!(late["reason"], "CHANGE_ALREADY_LANDED");
}
