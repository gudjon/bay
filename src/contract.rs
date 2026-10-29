//! The machine-readable contract, `bay help --json` (G252).
//!
//! One `bay.result/1` whose detail lists every command and subcommand with its flags, read at
//! run time from the command line bay parses; every decision with its exit code, and every
//! reason code with what it still allows, when a retry can succeed, the agent's next step and
//! whether it is transient, read from the table in `result.rs`; and the JSON Schema of
//! `bay.result/1`, built from the same table; and each agent adapter's events, what each can
//! block and what the hook does not judge (G306), from the tables `bay install` writes from;
//! and the reader contract of a talk (U5, G454), from the tables its judge uses.
//! No list here is a second definition: each one is
//! derived from the one the code uses.
//! `schemas/bay.result.schema.json` is a copy of the schema that G252 holds equal to it.

use clap::{Arg, ArgAction};
use serde_json::{Value, json};

use crate::result::{BayResult, Decision, Reason, SCHEMA};

/// The draft of JSON Schema the schema of `bay.result/1` is written in.
pub const JSON_SCHEMA_DRAFT: &str = "https://json-schema.org/draft/2020-12/schema";

/// `bay help --json`, from `cli`, the command line bay parses.
#[must_use]
pub fn contract(mut cli: clap::Command) -> BayResult {
    // Built as for a parse: each command gets its `--help`, and global flags reach every
    // subcommand (listed once, under `global`).
    cli.build();
    let mut commands = Vec::new();
    for command in cli.get_subcommands() {
        walk(command, &[], &mut commands);
    }
    BayResult::ok(json!({
        "bay": crate::version::BAY,
        "global": cli.get_arguments().map(argument).collect::<Vec<_>>(),
        "commands": commands,
        "decisions": decisions(),
        "reasons": reasons(),
        // G311: the one word list: each word a person need not know, the word said instead,
        // the other names to avoid, and what it means.
        "words": crate::result::PLAIN_WORDS
            .iter()
            .map(|entry| json!({
                "word": entry.word,
                "say": entry.say,
                "avoid": entry.avoid,
                "means": entry.means,
            }))
            .collect::<Vec<_>>(),
        "schema": schema(),
        // U5 (G454): the reader contract of a talk, from the tables bay judges talks by.
        "talk": crate::talk::show::contract(),
        "agents": agents(),
    }))
}

/// Every agent product bay has an adapter for (G306), each declared from the tables its code
/// uses: the events `bay install` writes with each entry's shape (`agent_settings::entry_shape`),
/// the payload version and doctor's floor, the presence test, and what the hook does not judge.
fn agents() -> Vec<Value> {
    use crate::agent_settings::{
        CLAUDE_EVENTS, CLAUDE_SETTINGS_FILE, CODEX_EVENTS, CODEX_HOOKS_FILE, CODEX_WITHOUT_BAY,
        PRESENCE_GUARD, WITHOUT_BAY, codex_entry_shape, entry_shape,
    };
    let events = adapter_events(CLAUDE_EVENTS, entry_shape);
    let codex_events = adapter_events(CODEX_EVENTS, codex_entry_shape);
    vec![
        json!({
            "product": crate::config::CLAUDE_PRODUCT,
            // The hook events were read against this version's payloads
            // (`tests/fixtures/claude-code-<version>`), and doctor requires it as the floor.
            "payload_version": crate::version::CLAUDE_HOOK_MINIMUM,
            "floor": crate::version::CLAUDE_HOOK_MINIMUM,
            "settings_file": CLAUDE_SETTINGS_FILE,
            "events": events,
            "presence_test": true,
            "presence_guard": PRESENCE_GUARD,
            "without_bay": WITHOUT_BAY,
            "cannot_see": crate::agent_hook::CANNOT_SEE,
        }),
        json!({
            "product": crate::config::CODEX_PRODUCT,
            // Read against `tests/fixtures/codex-<version>`; doctor requires it as the floor.
            "payload_version": crate::version::CODEX_HOOK_MINIMUM,
            "floor": crate::version::CODEX_HOOK_MINIMUM,
            "settings_file": CODEX_HOOKS_FILE,
            "events": codex_events,
            "presence_test": false,
            "presence_guard": null,
            "without_bay": CODEX_WITHOUT_BAY,
            // Codex runs the tool when bay's hook fails, so its deny is advisory: main is the guard.
            "fails_open": true,
            "cannot_see": crate::agent_hook::CANNOT_SEE,
        }),
    ]
}

/// The declared events of one adapter: each of `table`'s `(product_event, bay_event)` with the
/// entry `shape` gives it, the shape the writer uses.
fn adapter_events(
    table: &[(&str, &str)],
    shape: fn(&str, &str) -> crate::agent_settings::EntryShape,
) -> Vec<Value> {
    table
        .iter()
        .map(|(product_event, bay_event)| {
            let shape = shape(product_event, bay_event);
            json!({
                "product_event": product_event,
                "bay_event": bay_event,
                "blocks": shape.blocks,
                "on_failure": shape.on_failure.unwrap_or(crate::agent_settings::DEFAULT_ON_FAILURE),
                "timeout_s": shape.timeout_s,
                "matcher": shape.matcher,
                "rewakes": shape.rewakes,
            })
        })
        .collect()
}

/// G321 (learned in a fleet rollout, 2026-10-22): what each top-level command writes, and what
/// it never writes, so a person or an agent never has to guess before running it. One table,
/// declared here, exported on each command of `bay help --json` (`writes`, `never`) and shown
/// by `bay help`; the read commands' "nothing of yours" is held by G321's test, which runs them
/// and finds no file, branch or main moved.
pub const WRITES: &[(&str, &[&str], &[&str])] = &[
    (
        "save",
        &[
            "a change of the main checkout's edits, merged into main",
            "the main checkout, moved to the new main",
        ],
        &["any branch of yours"],
    ),
    (
        "waiting",
        &["only fetched copies of the remote's refs"],
        &["your files", "your branches", "main"],
    ),
    (
        "approve",
        &["your signed approval, or with --waive a signed waiver, pushed"],
        &["the change itself", "main"],
    ),
    (
        "reject",
        &["your signed no, pushed: the change is blocked"],
        &["the change itself", "main"],
    ),
    (
        "undo",
        &["a new change that reverses one merge, checked and merged like any other"],
        &["history: nothing is rewritten"],
    ),
    (
        "start",
        &[
            "the change's record and claim on the remote",
            "its view: a separate folder under .git/bay/views",
        ],
        &["your checkout's files", "main"],
    ),
    (
        "merge",
        &[
            "a request on the change's record; with --wait, the checks' evidence and the merge into main",
        ],
        &["your checkout's files"],
    ),
    (
        "abandon",
        &["an abandon request on the change's record"],
        &["the view's files", "main"],
    ),
    (
        "status",
        &["only fetched copies of the remote's refs, and bay's cache under .git/bay"],
        &["your files", "your branches", "main"],
    ),
    ("talk", &["one talk on the remote"], &["your files", "main"]),
    (
        "prune",
        &["deletes remote branches that already merged, each archived first and logged"],
        &["a branch that has not merged", "main"],
    ),
    (
        "enrol",
        &[
            "one key in the key registry, signed with this machine's registry key and pushed",
            "with --remove, the name's keys out of its class and into the registry's removed file",
        ],
        &["main"],
    ),
    (
        "doctor",
        &[
            "bay's cache under .git/bay",
            "this machine's readiness on its presence record on the remote, for bay status --all",
        ],
        &[
            "your files",
            "your branches",
            "main",
            "settings or hook entries",
        ],
    ),
    (
        "install",
        &[
            "the bay program and its skill in each Claude config directory",
            "in a clone, without --machine: bay's hook entries in the project's agent settings file and this clone's bay.* Git config",
        ],
        &[
            "main: you commit and merge the settings file",
            "with --machine: hook entries, a project's settings or its Git config",
        ],
    ),
    (
        "host",
        &["a new bare repository whose main only moves forward"],
        &["an existing repository"],
    ),
    (
        "runner",
        &[
            "run: the merges of queued changes into main, with their evidence and its log",
            "install: a service file that keeps it running",
        ],
        &["your checkout's files"],
    ),
    (
        "share",
        &["a change and its evidence pushed to a remote, or written to a bundle file"],
        &["main"],
    ),
    (
        "evidence",
        &["record: one signed record, pushed; missing writes nothing"],
        &["main"],
    ),
    (
        "update",
        &["a newer bay program, verified before it replaces this one"],
        &["anything in a project"],
    ),
    (
        "assets",
        &["nothing: held reads main's policy and history"],
        &["your files", "main", "the store"],
    ),
    (
        "plugin",
        &["nothing: it runs a plugin with test inputs"],
        &["your files", "main"],
    ),
    (
        "hook",
        &["the session's record of what it did; a deny of the agent's action when bay refuses it"],
        &["your files", "main"],
    ),
    ("help", &["nothing"], &["anything"]),
];

/// `command` and, after it, each of its subcommands, depth first.
fn walk<'a>(command: &'a clap::Command, parent: &[&'a str], out: &mut Vec<Value>) {
    let mut path = parent.to_vec();
    path.push(command.get_name());
    let writes = WRITES
        .iter()
        .find(|(name, _, _)| path.len() == 1 && *name == path[0]);
    out.push(json!({
        "command": path.join(" "),
        "writes": writes.map(|(_, writes, _)| writes),
        "never": writes.map(|(_, _, never)| never),
        "about": command.get_about().map(ToString::to_string),
        "hidden": command.is_hide_set(),
        "args": command
            .get_arguments()
            .filter(|arg| !arg.is_global_set())
            .map(argument)
            .collect::<Vec<_>>(),
        "subcommands": command
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect::<Vec<_>>(),
    }));
    for sub in command.get_subcommands() {
        walk(sub, &path, out);
    }
}

/// One flag or positional argument.
fn argument(arg: &Arg) -> Value {
    let action = arg.get_action();
    json!({
        "name": arg.get_id().as_str(),
        "long": arg.get_long().map(|long| format!("--{long}")),
        "short": arg.get_short().map(|short| format!("-{short}")),
        "positional": arg.is_positional(),
        "takes_value": action.takes_values(),
        // G327: the value may be left out (`--wait`, `--since`); `require_equals` says it is
        // then written `--wait=SECONDS`.
        "value_optional": action.takes_values()
            && arg.get_num_args().is_some_and(|range| range.min_values() == 0),
        "require_equals": arg.is_require_equals_set(),
        "repeatable": matches!(action, ArgAction::Append | ArgAction::Count),
        "required": arg.is_required_set(),
        "global": arg.is_global_set(),
        "hidden": arg.is_hide_set(),
        "default": arg
            .get_default_values()
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        "values": arg
            .get_possible_values()
            .iter()
            .map(|value| value.get_name().to_owned())
            .collect::<Vec<_>>(),
        "help": arg.get_help().map(ToString::to_string),
        // The placeholder `--help` shows for the value, such as `CHANGE` or `SECONDS`; null when
        // the flag takes none.
        "value_name": action.takes_values().then(|| {
            arg.get_value_names()
                .and_then(|names| names.first())
                .map_or_else(|| arg.get_id().as_str().to_uppercase(), ToString::to_string)
        }),
    })
}

/// Every decision with its exit code (`Decision::exit_code`).
fn decisions() -> Vec<Value> {
    Decision::ALL
        .iter()
        .map(|decision| json!({"decision": decision.word(), "exit": decision.exit_code()}))
        .collect()
}

/// Every row of the reason-code table, with the next step its refusals carry (`Reason::next`),
/// so a table of reasons and what to do is generated from here. Which decision a refusal
/// carries is the refusing command's (`blocked`, `failed`, or `queued` while bay acts on its
/// own); the row holds the rest.
fn reasons() -> Vec<Value> {
    Reason::ALL
        .iter()
        .map(|reason| {
            json!({
                "code": reason.code(),
                "allowed": reason.allowed(),
                "retry_when": reason.retry_when(),
                "next_step": reason.next(),
                "transient": reason.transient(),
            })
        })
        .collect()
}

/// Each word once, in the order first seen.
fn first_seen<'a>(words: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut seen = Vec::new();
    for word in words {
        if !seen.contains(&word) {
            seen.push(word);
        }
    }
    seen
}

/// The JSON Schema (draft 2020-12) of `bay.result/1`. Its enums are the decisions and the
/// reason-code table; its properties are the fields of `BayResult`, in their order.
#[must_use]
pub fn schema() -> Value {
    let decisions: Vec<&str> = Decision::ALL
        .iter()
        .map(|decision| decision.word())
        .collect();
    let codes: Vec<&str> = Reason::ALL.iter().map(|reason| reason.code()).collect();
    let allowed = first_seen(
        Reason::ALL
            .iter()
            .flat_map(|reason| reason.allowed().iter().copied()),
    );
    let retry_when = first_seen(Reason::ALL.iter().map(|reason| reason.retry_when()));
    let string = json!({"type": "string"});
    json!({
        "$schema": JSON_SCHEMA_DRAFT,
        "title": SCHEMA,
        "description": "The one result every bay command prints on stdout.",
        "type": "object",
        // Every result says one plain sentence (owner ruling 2026-10-21, `BayResult::say`).
        "required": ["schema", "decision", "say"],
        "additionalProperties": false,
        "dependentRequired": {"reason": ["allowed", "retry_when", "next_step"]},
        "properties": {
            "schema": {"const": SCHEMA},
            "op_id": string,
            "candidate": string,
            "decision": {"enum": decisions},
            "state": string,
            "reason": {"enum": codes},
            "say": string,
            "change": string,
            "detail": {},
            "allowed": {"type": "array", "items": {"enum": allowed}},
            "retry_when": {"enum": retry_when},
            "next_step": string,
            "waiting_for": string,
            "elapsed_s": {"type": "integer", "minimum": 0},
            "evidence": {"type": "array", "items": string},
            "receipt": string,
            "freshness": {
                "type": "object",
                "required": ["main", "fetched_at"],
                "additionalProperties": false,
                "properties": {"main": string, "fetched_at": string},
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::schema;
    use crate::result::{BayResult, Decision, Freshness, Reason, SCHEMA};
    use serde_json::{Value, json};

    /// BACKLOG 52 H5: every agent product `bay hook` reads is declared in `detail.agents`. The
    /// match names each variant, so a new product does not compile until it is listed here, and
    /// then fails here until the contract declares it.
    #[test]
    fn every_agent_product_is_declared() {
        use crate::agent_hook::Product;
        for product in [Product::Claude, Product::Codex] {
            let name = match product {
                Product::Claude => crate::config::CLAUDE_PRODUCT,
                Product::Codex => crate::config::CODEX_PRODUCT,
            };
            assert!(
                super::agents()
                    .iter()
                    .any(|adapter| adapter["product"] == name),
                "{name} is not declared"
            );
        }
    }

    #[test]
    fn the_schema_names_every_field_of_a_result_in_its_order() {
        // Every field is named here, so a new field fails to compile until it is listed.
        let result = BayResult {
            schema: SCHEMA,
            op_id: Some("c7k2m4.1".to_owned()),
            candidate: Some("c7k2m4.1.1".to_owned()),
            decision: Decision::Queued,
            state: Some("admitted".to_owned()),
            reason: Some(Reason::TrunkMoved),
            change: Some("c7k2m4".to_owned()),
            detail: Some(json!({})),
            allowed: Some(Vec::new()),
            retry_when: Some(Reason::TrunkMoved.retry_when()),
            waiting_for: Some("executor_capacity".to_owned()),
            elapsed_s: Some(1),
            evidence: Some(Vec::new()),
            receipt: Some("local/ev1".to_owned()),
            freshness: Some(Freshness {
                trunk: "a1b2c3d".to_owned(),
                fetched_at: "2026-10-20T00:00:00Z".to_owned(),
            }),
            human: None,
        };
        let value: Value = serde_json::from_str(&result.to_json().unwrap()).unwrap();
        let fields: Vec<&String> = value.as_object().unwrap().keys().collect();
        let schema = schema();
        let properties: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
        assert_eq!(fields, properties);
    }

    #[test]
    fn the_schema_enums_are_the_tables() {
        let schema = schema();
        assert_eq!(
            schema["properties"]["reason"]["enum"]
                .as_array()
                .unwrap()
                .len(),
            Reason::ALL.len()
        );
        assert_eq!(
            schema["properties"]["decision"]["enum"]
                .as_array()
                .unwrap()
                .len(),
            Decision::ALL.len()
        );
    }
}
