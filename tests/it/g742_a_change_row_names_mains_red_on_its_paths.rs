//! G742 (a request of a status panel, 2026-11-15: it showed "main is red" to every agent, whose
//! change mostly touched none of the red check's paths, and could not tell which without matching
//! globs of its own). A listing names, on each live change row, `red_on_paths`: main's red checks
//! that change touches, by the rule its merge is judged by (G677, `crate::red::blocking`). A change
//! elsewhere names none.
//!
//! Guard to mutate: in `src/status.rs` `red_on_paths`, name every red check whatever the change
//! touches, and the change elsewhere names one.

use crate::g244_a_red_trunk_is_owned_until_fixed_undone_or_quarantined::trunk;

use std::path::PathBuf;

/// Starts a change writing `path`, never merged: its id.
fn started(
    trunk: &crate::g244_a_red_trunk_is_owned_until_fixed_undone_or_quarantined::Trunk,
    path: &str,
) -> String {
    let dir = std::path::Path::new(path)
        .parent()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let started = trunk
        .test_box
        .bay_ok(&trunk.repo, &["start", path, "--paths", dir.as_str()]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(path), format!("{path}\n")).unwrap();
    trunk
        .test_box
        .bay_ok(&trunk.repo, &["start", started["change"].as_str().unwrap()]);
    started["change"].as_str().unwrap().to_owned()
}

#[test]
fn a_live_change_names_the_red_checks_on_its_paths_and_one_elsewhere_none() {
    let trunk = trunk();
    // GIVEN main red on the post check `suite`, whose paths are src/**
    let (code, landed, _) = trunk.land("src/broke.rs", None);
    assert_eq!(code, 0, "{landed}");
    trunk.suite(
        landed["detail"]["commit"].as_str().unwrap(),
        "product_failure",
    );
    // AND a live change on src/ and one on docs/
    let on = started(&trunk, "src/on.rs");
    let off = started(&trunk, "docs/off.md");

    // WHEN a listing reads them
    let status = trunk.test_box.bay_ok(&trunk.repo, &["status"]);
    assert_eq!(status["detail"]["red"][0]["check"], "suite", "{status}");
    let row = |id: &str| {
        status["detail"]["changes"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["change"] == id))
            .cloned()
            .unwrap_or_else(|| panic!("no row {id}: {status}"))
    };

    // THEN the change on src/ names the red check, and the one on docs/ none
    assert_eq!(
        row(&on)["red_on_paths"],
        serde_json::json!(["suite"]),
        "{status}"
    );
    assert_eq!(row(&off)["red_on_paths"], serde_json::json!([]), "{status}");
}
