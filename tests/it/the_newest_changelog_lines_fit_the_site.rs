//! `AGENTS.md`: getbay.io copies `CHANGELOG.md` at each release tag, so each line of a release
//! is 20 words or fewer. The newest release's section is the one a tag copies; this holds it to
//! that (two over-long lines in a row reached the site on 2026-10-27).

const MOST_WORDS: usize = 20;

#[test]
fn every_line_of_the_newest_release_is_twenty_words_or_fewer() {
    let changelog =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/CHANGELOG.md")).unwrap();
    let newest: Vec<&str> = changelog
        .split("\n## ")
        .nth(1)
        .expect("a release section")
        .lines()
        .skip(1)
        .filter(|line| line.starts_with("- "))
        .collect();
    assert!(!newest.is_empty(), "the newest release names no change");
    for line in newest {
        let words = line
            .trim_start_matches("- ")
            .replace('`', "")
            .split_whitespace()
            .count();
        assert!(words <= MOST_WORDS, "{words} words: {line}");
    }
}
