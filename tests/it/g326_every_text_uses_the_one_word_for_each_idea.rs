//! G326 (0.3.1, after the 0.3.0 vocabulary pass): every text bay shows uses the one word for each
//! idea.
//!
//! 0.3.0 renamed the ideas (`merge`, `main`, `runner`, `plugin`, `machine`), and a sentence that
//! still says land, trunk, executor, adapter, box or publication teaches the old name again. So
//! every command's and flag's help, each command's `writes` and `never`, every reason's
//! `next_step` in `bay help --json`, and the skill are held to the new words. A name in backticks
//! (`no_live_executor`, `` `executor` `` as a key class) or a dotted config key
//! (`bay.executorKey`) is a literal name, and is not read as a word.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/main.rs` `Command::Undo`: its help back to "checked and landed like any other".
//! - `src/result.rs` `Reason::next`, `MainHeld`: "lands once the hold clears".
//! - `skills/bay/SKILL.md`: "An executor checks the exact candidate".

use crate::support;

use support::TestBox;

/// The old words, as whole words in any case.
const OLD: &[&str] = &[
    "land",
    "lands",
    "landed",
    "landing",
    "landings",
    "trunk",
    "executor",
    "executors",
    "adapter",
    "adapters",
    "box",
    "boxes",
    "publication",
    "publications",
    "publish",
    "published",
];

/// `text` without its backticked spans and dotted names, which are literal names.
fn words_of(text: &str) -> Vec<String> {
    let mut plain = String::new();
    let mut quoted = false;
    for c in text.chars() {
        if c == '`' {
            quoted = !quoted;
            plain.push(' ');
        } else if !quoted {
            plain.push(c);
        }
    }
    plain
        .split(|c: char| c.is_whitespace() || ",;:()[]\"!?".contains(c))
        .filter(|word| !word.trim_end_matches('.').contains('.'))
        .map(|word| {
            word.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|word| !word.is_empty())
        .collect()
}

fn old_words(text: &str) -> Vec<String> {
    words_of(text)
        .into_iter()
        .filter(|word| OLD.contains(&word.as_str()))
        .collect()
}

#[test]
fn the_contract_and_the_skill_say_merge_main_runner_plugin_and_machine() {
    let test_box = TestBox::new();
    // `bay_ok` adds `--json`: this is `bay help --json`.
    let contract = test_box.bay_ok(test_box.path(), &["help"]);
    let mut found = Vec::new();
    let mut check = |place: String, text: &str| {
        let old = old_words(text);
        if !old.is_empty() {
            found.push(format!("{place}: {old:?} in {text:?}"));
        }
    };
    for command in contract["detail"]["commands"].as_array().unwrap() {
        if command["hidden"] == true {
            continue;
        }
        let name = command["command"].as_str().unwrap_or_default();
        check(
            format!("bay {name}"),
            command["about"].as_str().unwrap_or_default(),
        );
        for key in ["writes", "never"] {
            for line in command[key].as_array().into_iter().flatten() {
                check(
                    format!("bay {name} {key}"),
                    line.as_str().unwrap_or_default(),
                );
            }
        }
        for arg in command["args"].as_array().into_iter().flatten() {
            if arg["hidden"] == true {
                continue;
            }
            let flag = arg["long"]
                .as_str()
                .or(arg["name"].as_str())
                .unwrap_or_default();
            check(
                format!("bay {name} {flag}"),
                arg["help"].as_str().unwrap_or_default(),
            );
        }
    }
    for reason in contract["detail"]["reasons"].as_array().unwrap() {
        check(
            format!("{} next_step", reason["code"].as_str().unwrap_or_default()),
            reason["next_step"].as_str().unwrap_or_default(),
        );
    }
    let skill = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/bay/SKILL.md"),
    )
    .unwrap();
    for (n, line) in skill.lines().enumerate() {
        check(format!("SKILL.md:{}", n + 1), line);
    }
    assert!(
        found.is_empty(),
        "texts that still use an old word:\n{}",
        found.join("\n")
    );
}

#[test]
fn a_literal_name_is_not_a_word() {
    assert_eq!(
        old_words("set `bay.executorKey`, then `no_live_executor` clears"),
        Vec::<String>::new()
    );
    assert_eq!(
        old_words("git config bay.executorSandbox none"),
        Vec::<String>::new()
    );
    assert_eq!(old_words("the executor runs"), ["executor"]);
    assert_eq!(old_words("Land it."), ["land"]);
}
