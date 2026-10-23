//! G311 (owner ruling 2026-10-22, research ch. 35 R1): bay keeps one word list.
//!
//! The words a person who does not know Git need not know, each with the plain word said
//! instead, have one home: `PLAIN_WORDS` in bay's code. `bay help --json` exports it as
//! `detail.words`, the skill's "never say" line names exactly those words, and every `say`
//! sentence is held to it (the unit tests of `result.rs`). A site's copy checker reads the same
//! list from the contract, so no second list can disagree with it.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/contract.rs` `contract`: drop `"words"` (the contract exports no list).
//! - `src/result.rs` `PLAIN_WORDS`: drop the `view` entry (the skill's line names a word the
//!   list does not hold).
//! - `src/contract.rs` `contract`: drop `"means"` from an entry (R1: each entry says what it
//!   means, and lists the other names to avoid).

use crate::support;

use std::collections::BTreeSet;

use support::TestBox;

/// The words the skill's "Never say ... to them" line names.
fn skill_words() -> BTreeSet<String> {
    let skill = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/bay/SKILL.md"),
    )
    .unwrap();
    let line = skill
        .lines()
        .find(|line| line.contains("Never say "))
        .unwrap_or_else(|| panic!("the skill has no \"Never say\" line"));
    let listed = line
        .split("Never say ")
        .nth(1)
        .and_then(|rest| rest.split(" to them").next())
        .unwrap_or_else(|| panic!("no word list in {line:?}"));
    listed
        .replace(" or ", ", ")
        .split(", ")
        .map(|word| word.trim().trim_matches('`').to_owned())
        .filter(|word| !word.is_empty())
        .collect()
}

#[test]
fn the_contract_exports_the_one_word_list_and_the_skill_names_exactly_it() {
    let test_box = TestBox::new();
    // `bay_ok` adds `--json`: this is `bay help --json`.
    let contract = test_box.bay_ok(test_box.path(), &["help"]);
    let words = contract["detail"]["words"]
        .as_array()
        .unwrap_or_else(|| panic!("no detail.words: {contract}"));
    assert!(words.len() >= 7, "{words:?}");
    let listed: BTreeSet<String> = words
        .iter()
        .map(|entry| {
            let say = entry["say"].as_str().unwrap_or_default();
            assert!(!say.is_empty(), "a word with nothing said instead: {entry}");
            // R1 (ch. 35 § 8): each entry says what it means, in plain words, and lists the
            // other names to avoid.
            let means = entry["means"].as_str().unwrap_or_default();
            assert!(!means.is_empty(), "a word with no meaning: {entry}");
            assert!(
                !means
                    .split(|c: char| !c.is_ascii_alphabetic())
                    .any(bay::result::unplain),
                "a meaning that is not plain: {entry}"
            );
            assert!(
                entry["avoid"].is_array(),
                "a word with no avoid list: {entry}"
            );
            entry["word"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(skill_words(), listed, "the skill and the contract disagree");
    for word in [
        "trunk", "commit", "merge", "branch", "rebase", "ref", "view",
    ] {
        assert!(
            listed.contains(word),
            "{word} is not on the list: {listed:?}"
        );
    }
    let trunk = words.iter().find(|entry| entry["word"] == "trunk").unwrap();
    assert_eq!(trunk["say"], "main", "{trunk}");
}
