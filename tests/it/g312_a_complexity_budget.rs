//! G312 (owner ruling 2026-10-22): bay grows only on purpose: a complexity budget.
//!
//! bay's passion is to feel simple while it is powerful underneath, and simplicity erodes one
//! reasonable addition at a time. So the surface a person or an agent must learn is counted
//! here from the contract, `bay help --json`: the commands and the flags that are not hidden
//! (an alias kept for old callers is hidden, and not counted), the reason codes, and the words
//! of the one word list. A change that adds to any of them fails this test until the same commit
//! raises the budget below, with one line on the step it takes away from someone (`AGENTS.md`:
//! "a new command or flag must take a step away from someone, not add one"). A change that
//! removes surface lowers the budget the same way, so the budget never hides slack.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/main.rs`: un-hide a hidden alias, such as `start --intent-revision` (one flag more).
//! - `src/result.rs`: a new row in the reason table (one reason more).

use crate::support;

use std::collections::BTreeSet;

use serde_json::Value;
use support::TestBox;

/// The budget. Raise a number only in the commit that adds the surface, and add its line.
///
/// - 0.2.15: the counts bay had when the budget began.
/// - G317, one reason (`BAY_BELOW_FLOOR`), folded into `TOOL_BELOW_FLOOR` in 0.3.0; G317 itself
///   is removed in 0.3.1 (owner ruling: no project floor), with no count of its own left.
/// - 0.3.0, one name per idea (G325): `watch` is `status --since` (one read command), and
///   `approve --reject` is `reject` (the person's two words); `start --resume`, `land
///   --abandon`, `status --stranded` and `status --heads --prune` are gone, each covered by the
///   command that already did it. Fifteen reasons fold into four (a queue's turn, a tool below
///   its floor, an absent tool, a refused talk), and a lane change's checks end `ok`, so its
///   refusal is gone.
/// - 0.3.2, two flags (`enrol --remove`, `--reason`): removing a key was a hand-built, registry
///   signed Git commit; now it is one command that records why (owner ruling 2026-10-22).
/// - 0.4.0, two commands fewer: `host init` is `host` (one word for the one step), and the
///   agent hook `bay hook`, which `bay install` writes and no one types, is hidden from `--help`.
const COMMANDS: usize = 26;
const FLAGS: usize = 76;
/// - 0.3.3, one reason (`REGISTRY_UNREADABLE`): a registry no version rule allows made every
///   command fail with a raw error; doctor now names it and the repair.
/// - 0.4.0, one reason (`LOCK_NOT_HELD`, BACKLOG 57): an edit of a file that never merges,
///   made around its lock, reached the merge and lost someone's work there; it is refused first.
/// - 0.4.1, one reason (`ASSET_MISSING`, BACKLOG 49): a merge could name bytes no store held,
///   and every later clone and check found the file gone; it is refused first.
const REASONS: usize = 60;
const WORDS: usize = 7;

/// The commands of the contract that are not hidden, nor under a hidden command.
fn commands(contract: &Value) -> Vec<&Value> {
    let all = contract["detail"]["commands"].as_array().unwrap();
    let hidden: Vec<&str> = all
        .iter()
        .filter(|command| command["hidden"] == true)
        .filter_map(|command| command["command"].as_str())
        .collect();
    all.iter()
        .filter(|command| {
            let path = command["command"].as_str().unwrap_or_default();
            let top = path.split(' ').next().unwrap_or_default();
            command["hidden"] != true && !hidden.contains(&top)
        })
        .collect()
}

#[test]
fn the_surface_a_person_or_an_agent_learns_stays_within_its_budget() {
    let test_box = TestBox::new();
    // `bay_ok` adds `--json`: this is `bay help --json`.
    let contract = test_box.bay_ok(test_box.path(), &["help"]);
    let shown = commands(&contract);
    // A flag is counted once per command that takes it; clap's own --help and --version are not.
    let flags: BTreeSet<String> = shown
        .iter()
        .flat_map(|command| {
            let path = command["command"].as_str().unwrap_or_default().to_owned();
            command["args"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|arg| arg["hidden"] != true && arg["positional"] != true)
                .filter(|arg| {
                    !(matches!(arg["name"].as_str(), Some("help" | "version"))
                        && arg["takes_value"] != true)
                })
                .map(move |arg| format!("{path} {}", arg["name"].as_str().unwrap_or_default()))
        })
        .collect();
    let reasons = contract["detail"]["reasons"].as_array().unwrap().len();
    let words = contract["detail"]["words"].as_array().unwrap().len();
    let counted = (shown.len(), flags.len(), reasons, words);
    assert_eq!(
        counted,
        (COMMANDS, FLAGS, REASONS, WORDS),
        "bay's surface changed (commands, flags, reasons, words). If that is on purpose, set the \
         budget in tests/it/g312_a_complexity_budget.rs in this same commit, with one line on the \
         step it takes away from someone."
    );
}
