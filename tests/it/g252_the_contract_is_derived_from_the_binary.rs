//! G252. The contract is derived from the binary.
//!
//! `bay help --json` prints one `bay.result/1` whose detail is bay's machine-readable contract:
//! every command and flag of the command line bay parses, every decision with its exit code,
//! every reason code of the table in `src/result.rs` with what it allows and when a retry can
//! succeed, and the JSON Schema of `bay.result/1`. Each list is read from the definition the
//! code uses, so this test fails when one drifts: a reason in the listing and not in the table
//! or back, a command clap knows that is not listed, a flag the listing names that `--help` does
//! not show, a real result the schema refuses, or a `schemas/bay.result.schema.json` that is not
//! the generated schema.

use crate::support;

use std::path::Path;

use bay::result::{BayResult, Decision, Reason};
use serde_json::{Value, json};
use support::{TestBox, result_json, unchecked_files};

/// The JSON Schema keywords the validator below implements. A schema keyword outside this list
/// fails the test, so the validator never skips a rule it does not know.
const KEYWORDS: &[&str] = &[
    "$schema",
    "title",
    "description",
    "type",
    "required",
    "additionalProperties",
    "dependentRequired",
    "properties",
    "const",
    "enum",
    "items",
    "minimum",
];

fn contract(test_box: &TestBox) -> Value {
    let (code, result) = result_json(test_box.bay(test_box.path()).arg("help"));
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["decision"], "ok", "{result}");
    result
}

/// Every keyword of `schema` and of its subschemas is one the validator implements.
fn assert_known_keywords(schema: &Value, at: &str) {
    let map = schema
        .as_object()
        .unwrap_or_else(|| panic!("{at}: not a schema"));
    for (key, value) in map {
        assert!(KEYWORDS.contains(&key.as_str()), "{at}: keyword {key}");
        match key.as_str() {
            "properties" => {
                for (name, sub) in value.as_object().unwrap() {
                    assert_known_keywords(sub, &format!("{at}.{name}"));
                }
            }
            "items" => assert_known_keywords(value, &format!("{at}[]")),
            _ => {}
        }
    }
}

/// What in `value` the subset of draft 2020-12 in [`KEYWORDS`] refuses.
fn violations(schema: &Value, value: &Value, at: &str, out: &mut Vec<String>) {
    if let Some(expected) = schema.get("const")
        && value != expected
    {
        out.push(format!("{at}: {value} is not {expected}"));
    }
    if let Some(Value::Array(options)) = schema.get("enum")
        && !options.contains(value)
    {
        out.push(format!("{at}: {value} is not one of the enum"));
    }
    if let Some(Value::String(kind)) = schema.get("type") {
        let fits = match kind.as_str() {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "integer" => value.is_u64() || value.is_i64(),
            other => panic!("the validator does not know type {other}"),
        };
        if !fits {
            out.push(format!("{at}: {value} is not of type {kind}"));
            return;
        }
    }
    if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64)
        && value.as_f64().is_some_and(|number| number < minimum)
    {
        out.push(format!("{at}: {value} is below {minimum}"));
    }
    if let Value::Object(map) = value {
        for required in schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let key = required.as_str().unwrap();
            if !map.contains_key(key) {
                out.push(format!("{at}: no {key}"));
            }
        }
        if let Some(Value::Object(dependent)) = schema.get("dependentRequired") {
            for (key, needed) in dependent {
                for need in needed.as_array().unwrap() {
                    let need = need.as_str().unwrap();
                    if map.contains_key(key) && !map.contains_key(need) {
                        out.push(format!("{at}: {key} without {need}"));
                    }
                }
            }
        }
        let properties = schema.get("properties").and_then(Value::as_object);
        let closed = schema.get("additionalProperties") == Some(&Value::Bool(false));
        for (key, field) in map {
            match properties.and_then(|properties| properties.get(key)) {
                Some(sub) => violations(sub, field, &format!("{at}.{key}"), out),
                None if closed => out.push(format!("{at}: unknown field {key}")),
                None => {}
            }
        }
    }
    if let (Value::Array(items), Some(item)) = (value, schema.get("items")) {
        for (index, element) in items.iter().enumerate() {
            violations(item, element, &format!("{at}[{index}]"), out);
        }
    }
}

fn assert_valid(schema: &Value, result: &Value) {
    let mut found = Vec::new();
    violations(schema, result, "$", &mut found);
    assert!(found.is_empty(), "{found:?}: {result}");
}

/// One row of the reason-code table: (code, allowed, `retry_when`, transient).
type Row = (String, Vec<String>, String, bool);

/// The rows of the `reasons!` table in `src/result.rs`, as written.
fn table_rows() -> Vec<Row> {
    let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/result.rs"))
        .unwrap();
    let start = text.find("\nreasons! {\n").expect("no reasons! table");
    let table = &text[start..];
    let table = &table[..table.find("\n}\n").unwrap()];
    let mut rows = Vec::new();
    for line in table.lines().map(str::trim) {
        let Some((_, row)) = line.split_once(" => ") else {
            continue;
        };
        if line.starts_with("//") {
            continue;
        }
        // "CODE", ["a", "b"], "retry_when", false;
        let (code, rest) = row.split_once(", [").unwrap();
        let (allowed, rest) = rest.split_once("], ").unwrap();
        let (retry_when, transient) = rest.split_once(", ").unwrap();
        let unquote = |word: &str| word.trim().trim_matches('"').to_owned();
        rows.push((
            unquote(code),
            allowed
                .split(',')
                .map(unquote)
                .filter(|word| !word.is_empty())
                .collect(),
            unquote(retry_when),
            transient.trim_end_matches(';') == "true",
        ));
    }
    rows
}

#[test]
fn every_reason_in_the_table_is_listed_and_no_other() {
    let test_box = TestBox::new();
    let result = contract(&test_box);
    let listed: Vec<Row> = result["detail"]["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["code"].as_str().unwrap().to_owned(),
                row["allowed"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|word| word.as_str().unwrap().to_owned())
                    .collect(),
                row["retry_when"].as_str().unwrap().to_owned(),
                row["transient"].as_bool().unwrap(),
            )
        })
        .collect();
    let table = table_rows();
    assert_eq!(table.len(), Reason::ALL.len(), "the scan of src/result.rs");
    assert_eq!(listed, table);

    // Every decision with the exit code the binary exits with.
    let decisions: Vec<(String, i64)> = result["detail"]["decisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["decision"].as_str().unwrap().to_owned(),
                row["exit"].as_i64().unwrap(),
            )
        })
        .collect();
    let expected: Vec<(String, i64)> = Decision::ALL
        .iter()
        .map(|decision| (decision.word().to_owned(), i64::from(decision.exit_code())))
        .collect();
    assert_eq!(decisions, expected);
    assert!(
        decisions.contains(&("blocked".to_owned(), 10)),
        "{decisions:?}"
    );
    assert!(
        decisions.contains(&("failed".to_owned(), 20)),
        "{decisions:?}"
    );
}

/// The top-level command names `bay -h` shows: since 0.2.15 (G313, R6) the grouped page, one
/// `  bay <command>` heading per command.
fn help_commands(test_box: &TestBox) -> Vec<String> {
    let out = test_box.bay(test_box.path()).arg("-h").output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let help = String::from_utf8(out.stdout).unwrap();
    let mut names: Vec<String> = help
        .lines()
        .filter_map(|line| line.strip_prefix("  bay "))
        .filter_map(|rest| rest.split_whitespace().next())
        .filter(|name| !name.starts_with('<') && !name.starts_with('['))
        .map(str::to_owned)
        .collect();
    names.dedup();
    assert!(!names.is_empty(), "no commands on the page: {help}");
    names
}

#[test]
fn every_command_clap_knows_is_listed_with_the_flags_its_help_shows() {
    let test_box = TestBox::new();
    let result = contract(&test_box);
    let commands = result["detail"]["commands"].as_array().unwrap();
    let top: Vec<&str> = commands
        .iter()
        .map(|command| command["command"].as_str().unwrap())
        .filter(|path| !path.contains(' '))
        .collect();
    let shown = help_commands(&test_box);
    assert!(shown.contains(&"help".to_owned()), "{shown:?}");
    for name in &shown {
        assert!(top.contains(&name.as_str()), "bay {name} is not listed");
    }
    // A listed command `bay -h` does not show is one clap hides, and the listing says so.
    for command in commands {
        let path = command["command"].as_str().unwrap();
        if !path.contains(' ') && !shown.iter().any(|name| name == path) {
            assert_eq!(command["hidden"], true, "{command}");
        }
    }
    let global: Vec<&str> = result["detail"]["global"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|arg| arg["long"].as_str())
        .collect();
    for flag in ["--json", "--help", "--version"] {
        assert!(global.contains(&flag), "{global:?}");
    }

    // Each listed command parses, and its help shows every flag the listing names for it.
    for command in commands {
        let path: Vec<&str> = command["command"].as_str().unwrap().split(' ').collect();
        let out = test_box
            .bay(test_box.path())
            .args(&path)
            .arg("--help")
            .output()
            .unwrap();
        assert!(out.status.success(), "bay {path:?} --help: {out:?}");
        let help = String::from_utf8(out.stdout).unwrap();
        for arg in command["args"].as_array().unwrap() {
            assert_eq!(arg["global"], false, "{arg}");
            if arg["hidden"] == true {
                continue;
            }
            if let Some(long) = arg["long"].as_str() {
                let found = help
                    .split(|c: char| c.is_whitespace() || c == ',' || c == '=' || c == '[')
                    .any(|word| word == long);
                assert!(found, "bay {path:?} --help does not show {long}");
            }
            assert!(arg["takes_value"].is_boolean(), "{arg}");
            assert!(arg["required"].is_boolean(), "{arg}");
        }
    }
    // A flag with a value and one without are told apart, from clap.
    let flag = |command_name: &str, name: &str| {
        let command = commands
            .iter()
            .find(|command| command["command"] == command_name)
            .unwrap();
        command["args"]
            .as_array()
            .unwrap()
            .iter()
            .find(|arg| arg["long"] == name)
            .unwrap_or_else(|| panic!("{command_name} has no {name}: {command}"))
            .clone()
    };
    assert_eq!(flag("start", "--paths")["takes_value"], true);
    assert_eq!(flag("start", "--new")["takes_value"], false);
    // G293: `--wait[=SECONDS]` takes an optional value.
    assert_eq!(flag("merge", "--wait")["takes_value"], true);
    let undo = commands
        .iter()
        .find(|command| command["command"] == "undo")
        .unwrap();
    let change = undo["args"]
        .as_array()
        .unwrap()
        .iter()
        .find(|arg| arg["name"] == "change")
        .unwrap();
    assert_eq!(change["positional"], true, "{change}");
    assert_eq!(change["required"], true, "{change}");
    let host = commands
        .iter()
        .find(|command| command["command"] == "host")
        .expect("host is not listed");
    assert!(host["about"].is_string(), "{host}");
}

#[test]
fn the_schema_file_is_the_generated_schema() {
    let test_box = TestBox::new();
    let result = contract(&test_box);
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("schemas/bay.result.schema.json");
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(
        written,
        result["detail"]["schema"],
        "{} is not `bay help --json`'s detail.schema",
        file.display()
    );
    let schema = &result["detail"]["schema"];
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    let codes: Vec<&str> = Reason::ALL.iter().map(|reason| reason.code()).collect();
    assert_eq!(schema["properties"]["reason"]["enum"], json!(codes));
    let decisions: Vec<&str> = Decision::ALL
        .iter()
        .map(|decision| decision.word())
        .collect();
    assert_eq!(schema["properties"]["decision"]["enum"], json!(decisions));
}

#[test]
fn the_schema_holds_real_results_and_refuses_broken_ones() {
    let test_box = TestBox::new();
    let contract = contract(&test_box);
    let schema = contract["detail"]["schema"].clone();
    assert_known_keywords(&schema, "$");
    assert_valid(&schema, &contract);

    // Every refusal of the table, as the binary's own library writes it.
    for &reason in Reason::ALL {
        for decision in [Decision::Blocked, Decision::Failed, Decision::Queued] {
            let text = BayResult::refused(decision, reason)
                .with_change("c7k2m4")
                .to_json()
                .unwrap();
            assert_valid(&schema, &serde_json::from_str(&text).unwrap());
        }
    }

    // Results of real commands: outside a repository, and a change started, landed and read.
    let (_, outside) = result_json(test_box.bay(test_box.path()).arg("status"));
    assert_valid(&schema, &outside);
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let (id, view) = test_box.start_and_edit(&repo);
    let (_, started) = result_json(test_box.bay(&repo).args(["status", &id]));
    assert_valid(&schema, &started);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_valid(&schema, &landed);
    let reads: [&[&str]; 3] = [
        &["status", id.as_str()],
        &["status", "--branches"],
        &["status", "--since"],
    ];
    for args in reads {
        let (_, result) = result_json(test_box.bay(&repo).args(args));
        assert_valid(&schema, &result);
    }
    // The same change landed again, as the binary answers it.
    let (_, again) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_valid(&schema, &again);

    // The validator is not vacuous: each broken result is refused.
    let blocked: Value =
        serde_json::from_str(&BayResult::blocked(Reason::CheckUnknown).to_json().unwrap()).unwrap();
    let mut broken = Vec::new();
    for (field, value) in [
        ("decision", json!("maybe")),
        ("reason", json!("NOT_A_CODE")),
        ("allowed", json!(["fly"])),
        ("elapsed_s", json!(-1)),
        ("unknown_field", json!(1)),
    ] {
        let mut result = blocked.clone();
        result[field] = value;
        broken.push(result);
    }
    let mut without = blocked.clone();
    without.as_object_mut().unwrap().remove("retry_when");
    broken.push(without);
    let mut no_schema = blocked;
    no_schema.as_object_mut().unwrap().remove("schema");
    broken.push(no_schema);
    for result in &broken {
        let mut found = Vec::new();
        violations(&schema, result, "$", &mut found);
        assert!(!found.is_empty(), "the schema accepts {result}");
    }
}
