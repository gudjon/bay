//! `AGENTS.md`: getbay.io copies `CHANGELOG.md` at each release tag, so each line of a release
//! is 20 words or fewer (two over-long lines in a row reached the site on 2026-10-27). The rule
//! lives in `scripts/changelog-fits.sh`, which `scripts/gate.sh` runs before any compile; this
//! runs the same script, on the real changelog and on two that break it.

use std::path::Path;
use std::process::Command;

fn fits(changelog: &Path) -> (bool, String) {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/changelog-fits.sh");
    let out = Command::new("sh")
        .arg(script)
        .arg(changelog)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn written(name: &str, text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    (dir, path)
}

#[test]
fn every_line_of_the_newest_release_is_twenty_words_or_fewer() {
    let (ok, why) = fits(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/CHANGELOG.md"
    )));
    assert!(ok, "{why}");
}

#[test]
fn a_line_of_twenty_one_words_fails_and_an_older_release_is_not_read() {
    let twenty: Vec<String> = (1..=20).map(|n| format!("w{n}")).collect();
    let twenty = twenty.join(" ");
    let (_dir, at_most) = written(
        "CHANGELOG.md",
        &format!("# Changelog\n\n## 9.9.9\n\n- `{twenty}`\n\n## 9.9.8\n\n- {twenty} x y\n"),
    );
    let (ok, why) = fits(&at_most);
    assert!(ok, "twenty words, backticks not counted: {why}");
    let (_dir, over) = written(
        "CHANGELOG.md",
        &format!("# Changelog\n\n## 9.9.9\n\n- {twenty} `x`\n"),
    );
    let (ok, why) = fits(&over);
    assert!(!ok && why.contains("21 words"), "{why}");
}

#[test]
fn a_newest_release_that_names_no_change_fails() {
    let (_dir, empty) = written("CHANGELOG.md", "# Changelog\n\n## 9.9.9\n\nnothing\n");
    let (ok, why) = fits(&empty);
    assert!(!ok && why.contains("names no change"), "{why}");
}
