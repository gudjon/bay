//! The machine-readable contract, `bay help --json` (G252).
//!
//! One `bay.result/1` whose detail lists every command and subcommand with its flags, read at
//! run time from the command line bay parses; every decision with its exit code, and every
//! reason code with what it still allows, when a retry can succeed, the agent's next step and
//! whether it is transient, read from the table in `result.rs`; and the JSON Schema of
//! `bay.result/1`, built from the same table; and each agent adapter's events, what each can
//! block and what the hook does not judge (G306), from the tables `bay install` writes from.
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
        "schema": schema(),
        "adapters": adapters(),
    }))
}

/// Every agent product bay has an adapter for (G306), each declared from the tables its code
/// uses: the events `bay install` writes with each entry's shape (`agent_settings::entry_shape`),
/// the payload version and doctor's floor, the presence test, and what the hook does not judge.
fn adapters() -> Vec<Value> {
    use crate::agent_settings::{
        CLAUDE_EVENTS, CLAUDE_SETTINGS_FILE, DEFAULT_ON_FAILURE, PRESENCE_GUARD, WITHOUT_BAY,
        entry_shape,
    };
    let events: Vec<Value> = CLAUDE_EVENTS
        .iter()
        .map(|(product_event, bay_event)| {
            let shape = entry_shape(product_event);
            json!({
                "product_event": product_event,
                "bay_event": bay_event,
                "blocks": shape.blocks,
                "on_failure": shape.on_failure.unwrap_or(DEFAULT_ON_FAILURE),
                "timeout_s": shape.timeout_s,
                "matcher": shape.matcher,
            })
        })
        .collect();
    vec![json!({
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
    })]
}

/// `command` and, after it, each of its subcommands, depth first.
fn walk<'a>(command: &'a clap::Command, parent: &[&'a str], out: &mut Vec<Value>) {
    let mut path = parent.to_vec();
    path.push(command.get_name());
    out.push(json!({
        "command": path.join(" "),
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
            "published": {"type": "boolean"},
            "reason": {"enum": codes},
            "say": string,
            "change": string,
            "detail": {},
            "allowed": {"type": "array", "items": {"enum": allowed}},
            "retry_when": {"enum": retry_when},
            "next_step": string,
            "waiting_for": string,
            "elapsed_s": {"type": "integer", "minimum": 0},
            "receipts": {"type": "array", "items": string},
            "next": string,
            "receipt": string,
            "freshness": {
                "type": "object",
                "required": ["trunk", "fetched_at"],
                "additionalProperties": false,
                "properties": {"trunk": string, "fetched_at": string},
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::schema;
    use crate::result::{BayResult, Decision, Freshness, Reason, SCHEMA};
    use serde_json::{Value, json};

    #[test]
    fn the_schema_names_every_field_of_a_result_in_its_order() {
        // Every field is named here, so a new field fails to compile until it is listed.
        let result = BayResult {
            schema: SCHEMA,
            op_id: Some("c7k2m4.1".to_owned()),
            candidate: Some("c7k2m4.1.1".to_owned()),
            decision: Decision::Queued,
            state: Some("admitted".to_owned()),
            published: Some(false),
            reason: Some(Reason::TrunkMoved),
            change: Some("c7k2m4".to_owned()),
            detail: Some(json!({})),
            allowed: Some(Vec::new()),
            retry_when: Some(Reason::TrunkMoved.retry_when()),
            waiting_for: Some("executor_capacity".to_owned()),
            elapsed_s: Some(1),
            receipts: Some(Vec::new()),
            next: Some("published"),
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
