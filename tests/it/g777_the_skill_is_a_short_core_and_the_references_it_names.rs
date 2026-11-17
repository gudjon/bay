//! G777 (a measurement, 2026-11-17: the skill was one 49 KB file, about 13,000 tokens, read
//! whole on every use, 183 times a day on one box). The skill is a folder: a short core,
//! `SKILL.md`, and reference files beside it that the core names. `bay install` puts every file
//! of the folder in place and removes a file it no longer carries; `bay doctor` is red when any
//! one file of the installed copy differs, and names that file.
//!
//! Guards to mutate: in `src/skill.rs`, drop a reference file from `FILES` (install no longer
//! places it, and the source-directory test fails); make `compare` report no files for a
//! difference (doctor's note no longer names the file).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, result_json, row};

fn source_skill() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/bay")
}

fn installed_skill(test_box: &TestBox) -> PathBuf {
    test_box.home.join(".claude/skills/bay")
}

/// Every file of the source skill folder, by its name.
fn source_files() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(source_skill())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn install_places_every_file_and_doctor_names_the_one_that_differs() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let files = source_files();
    assert!(
        files.len() > 1 && files.iter().any(|name| name == "SKILL.md"),
        "the skill is a core and its references: {files:?}"
    );
    // GIVEN an older install that left a file this version no longer carries
    let copy = installed_skill(&test_box);
    std::fs::create_dir_all(&copy).unwrap();
    std::fs::write(copy.join("retired.md"), "an older reference\n").unwrap();

    // WHEN bay installs
    let (code, installed) = result_json(test_box.bay(&repo).arg("install"));
    assert_eq!(code, 0, "{installed}");

    // THEN every file of the folder is in place, byte for byte, and the retired one is gone
    for name in &files {
        assert_eq!(
            std::fs::read(copy.join(name)).unwrap(),
            std::fs::read(source_skill().join(name)).unwrap(),
            "{name}"
        );
    }
    assert!(!copy.join("retired.md").exists());
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    assert_eq!(row(&result, "skill")["state"], "green");

    // WHEN one reference file of the installed copy differs
    let reference = files
        .iter()
        .find(|name| *name != "SKILL.md")
        .unwrap()
        .clone();
    std::fs::write(copy.join(&reference), "an older reference\n").unwrap();

    // THEN doctor is red and names that file
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "SKILL_MISMATCH");
    let skill = row(&result, "skill");
    assert_eq!(
        skill["note"],
        format!("skill does not match this bay: {reference}"),
        "{skill}"
    );
    assert_eq!(skill["dirs"][0]["files"][0], reference.as_str(), "{skill}");
}
