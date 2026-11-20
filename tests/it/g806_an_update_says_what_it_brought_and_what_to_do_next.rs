//! G806 (owner, 2026-11-20: "after each bay update … a proper call to action, to know what has
//! been updated since the last update and to update main setups … a proper closed loop"). An
//! update lists, in `detail.changes`, each release it brings past the installed bay with that
//! release's notes, newest first, read from the release list it already reads; and its sentence
//! tells the agent to read them, to run `bay install --machine`, and to read `bay doctor` in each
//! repository, which names a hook its settings now lack. A check (`--check`)
//! lists the same changes before anything is installed.
//!
//! Guard to mutate: in `src/update.rs` `Plan::detail`, drop `changes` (the case's list is empty).

#![cfg(unix)]

use crate::release_fixture;

use release_fixture::Fixture;
use release_fixture::RetryBusy as _;

const NEWER: &str = "9.2.0";
const MIDDLE: &str = "9.1.5";

fn update(fixture: &Fixture, args: &[&str]) -> serde_json::Value {
    let out = fixture
        .bay(fixture.path())
        .arg("update")
        .args(args)
        .output_busy();
    let text = String::from_utf8(out.stdout).unwrap();
    serde_json::from_str(&text).unwrap_or_else(|_| panic!("one result: {text}"))
}

#[test]
fn an_update_lists_each_release_it_brings_and_says_to_install_in_each_repository() {
    let fixture = Fixture::new();
    fixture.release(NEWER, NEWER, None);
    fixture.release(MIDDLE, MIDDLE, None);
    let installed = bay::version::BAY;
    fixture.list_noted(&[
        (NEWER, false, true, "- Runners take the next head at once.\n- A view goes when its change ends.\n\nBuilt from abc."),
        (MIDDLE, false, true, "- Doctor names bay's disk use."),
        (installed, false, true, "- An older note no update brings."),
    ]);

    let check = update(&fixture, &["--check"]);
    let changes = check["detail"]["changes"]
        .as_array()
        .unwrap_or_else(|| panic!("{check}"));
    assert_eq!(
        changes.len(),
        2,
        "only what is newer than the installed bay: {check}"
    );

    let result = update(&fixture, &[]);
    assert_eq!(result["detail"]["updated"], true, "{result}");
    let changes = result["detail"]["changes"]
        .as_array()
        .unwrap_or_else(|| panic!("{result}"));
    assert_eq!(changes[0]["version"], NEWER, "newest first: {result}");
    assert_eq!(
        changes[0]["notes"],
        serde_json::json!([
            "Runners take the next head at once.",
            "A view goes when its change ends."
        ]),
        "{result}"
    );
    assert_eq!(changes[1]["version"], MIDDLE, "{result}");
    let say = result["say"].as_str().unwrap();
    assert!(say.contains("detail.changes"), "{say}");
    assert!(say.contains("bay install --machine"), "{say}");
    assert!(say.contains("bay doctor"), "{say}");
}
