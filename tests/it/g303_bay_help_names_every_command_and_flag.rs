//! G303: `bay help` names every command, subcommand and flag bay takes, grouped by who runs
//! it.
//!
//! The page is read from the same list as the contract, `bay help --json`, so a person or an
//! agent who reads it sees all of bay on one page: each command that is not hidden, under its
//! own heading line `bay <command>`, with each of its flags that is not hidden; the flags every
//! command takes once at the end. People's commands come first, then the agents', then admin.
//! A hidden command is an alias kept for old callers, and the page leaves it out.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/guide.rs` `section`: drop the loop that writes each flag (no flag is named).
//! - `src/guide.rs` `guide`: drop the "Admin" group (no admin command is named).
//! - `src/guide.rs` `guide`: `!hidden(command) && !hidden_tops.contains(&top)` to `true` (a
//!   hidden command is named).
//! - `src/main.rs` `help`: drop the `command.is_empty()` branch (`bay help` prints clap's
//!   list of commands, with no flags).

use crate::support;

use serde_json::Value;
use support::TestBox;

/// The lines of `bay help`, and the contract `bay help --json`.
fn help_and_contract(test_box: &TestBox) -> (String, Value) {
    let out = test_box.bay(test_box.path()).arg("help").output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let page = String::from_utf8(out.stdout).unwrap();
    // `bay_ok` adds `--json`: this is `bay help --json`.
    let contract = test_box.bay_ok(test_box.path(), &["help"]);
    (page, contract)
}

/// The lines of `page` under the heading `bay <path>`, up to the next heading or group title.
fn section<'a>(page: &'a str, path: &str) -> Option<Vec<&'a str>> {
    let lines: Vec<&str> = page.lines().collect();
    let heading = lines.iter().position(|line| {
        line.strip_prefix("  bay ")
            .is_some_and(|rest| rest == path || rest.starts_with(&format!("{path} ")))
            && line.starts_with("  bay ")
            && !line.starts_with("      ")
    })?;
    let rest = &lines[heading + 1..];
    let end = rest
        .iter()
        .position(|line| !line.starts_with("      "))
        .unwrap_or(rest.len());
    Some(rest[..end].to_vec())
}

#[test]
fn every_command_and_flag_the_contract_lists_is_on_the_page() {
    let test_box = TestBox::new();
    let (page, contract) = help_and_contract(&test_box);
    let commands = contract["detail"]["commands"].as_array().unwrap();
    let hidden_tops: Vec<&str> = commands
        .iter()
        .filter(|command| command["hidden"] == true)
        .filter_map(|command| command["command"].as_str())
        .collect();
    let mut seen = 0;
    for command in commands {
        let path = command["command"].as_str().unwrap();
        let top = path.split(' ').next().unwrap();
        if command["hidden"] == true || hidden_tops.contains(&top) {
            assert!(
                section(&page, path).is_none(),
                "hidden command {path} is on the page:\n{page}"
            );
            continue;
        }
        let lines =
            section(&page, path).unwrap_or_else(|| panic!("no `bay {path}` on the page:\n{page}"));
        for arg in command["args"].as_array().unwrap() {
            let Some(long) = arg["long"].as_str() else {
                continue;
            };
            if arg["hidden"] == true
                || (matches!(arg["name"].as_str(), Some("help" | "version"))
                    && arg["takes_value"] != true)
            {
                continue;
            }
            assert!(
                lines
                    .iter()
                    .any(|line| line.trim_start().starts_with(&format!("{long} "))
                        || line.trim_start() == long
                        || line.trim_start().starts_with(&format!("{long}  "))),
                "`bay {path}` does not name {long}:\n{}",
                lines.join("\n")
            );
        }
        seen += 1;
    }
    assert!(seen >= 20, "only {seen} commands on the page");
    for global in contract["detail"]["global"].as_array().unwrap() {
        if let Some(long) = global["long"].as_str()
            && global["hidden"] != true
            && !(matches!(global["name"].as_str(), Some("help" | "version"))
                && global["takes_value"] != true)
        {
            assert!(page.contains(long), "the page never names {long}:\n{page}");
        }
    }
}

#[test]
fn people_come_first_then_agents_then_admin() {
    let test_box = TestBox::new();
    let (page, _) = help_and_contract(&test_box);
    let at = |needle: &str| {
        page.find(needle)
            .unwrap_or_else(|| panic!("{needle:?} is not on the page:\n{page}"))
    };
    let people = at("For people");
    let agents = at("For agents");
    let admin = at("\nAdmin\n");
    assert!(people < agents && agents < admin, "{page}");
    assert!(
        at("\n  bay save") > people && at("\n  bay save") < agents,
        "{page}"
    );
    assert!(
        at("\n  bay start") > agents && at("\n  bay start") < admin,
        "{page}"
    );
    assert!(at("\n  bay doctor") > admin, "{page}");
    // A subcommand sits under its command: `bay host init` after `bay host`.
    assert!(at("\n  bay host init") > at("\n  bay host"), "{page}");
}
