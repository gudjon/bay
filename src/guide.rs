//! `bay help`: every command, subcommand and flag of bay on one page, grouped by who runs it
//! (G303).
//!
//! The page is read from the contract (`crate::contract`, `bay help --json`), which is read
//! from the command line bay parses: the guide and the contract are two views of one list, so
//! the guide never names a command or a flag bay does not take, nor misses one it does. A
//! hidden command or flag is an alias kept for old callers and is left out of both views'
//! reading here. The flags every command takes (`-C`, `--json`) are named once, at the end.

use std::fmt::Write;

use serde_json::Value;

/// The commands a person asks for, in the order they meet them. The agent runs them for the
/// person.
const PEOPLE: &[&str] = &["save", "waiting", "approve", "reject", "undo"];

/// The commands an agent runs to do its work.
const AGENTS: &[&str] = &["start", "merge", "abandon", "status", "talk"];

/// Whether a top-level command belongs to a group of the page.
type Member = fn(&str) -> bool;

/// The page `bay help` prints, from `cli`, the command line bay parses.
#[must_use]
pub fn guide(cli: clap::Command) -> String {
    let contract = crate::contract::contract(cli);
    let detail = contract.detail.unwrap_or(Value::Null);
    let all = detail["commands"].as_array().cloned().unwrap_or_default();
    // A hidden command hides what is under it too.
    let hidden_tops: Vec<&str> = all
        .iter()
        .filter(|command| hidden(command))
        .filter_map(|command| command["command"].as_str())
        .collect();
    let commands: Vec<&Value> = all
        .iter()
        .filter(|command| {
            let path = command["command"].as_str().unwrap_or_default();
            let top = path.split(' ').next().unwrap_or_default();
            !hidden(command) && !hidden_tops.contains(&top)
        })
        .collect();
    let mut out = format!(
        "bay {}: agents submit once, and bay lands exactly what was checked.\n",
        crate::version::BAY
    );
    let groups: [(&str, Member); 3] = [
        ("For people (an agent runs these for you)", |top| {
            PEOPLE.contains(&top)
        }),
        ("For agents", |top| AGENTS.contains(&top)),
        ("Admin", |top| {
            !PEOPLE.contains(&top) && !AGENTS.contains(&top)
        }),
    ];
    for (title, member) in groups {
        let mut tops: Vec<&str> = commands
            .iter()
            .filter_map(|command| command["command"].as_str())
            .filter(|path| !path.contains(' ') && member(path))
            .collect();
        // People's and agents' commands in the order they are used; the rest as bay lists them.
        let order = |top: &str| {
            PEOPLE
                .iter()
                .chain(AGENTS)
                .position(|known| *known == top)
                .unwrap_or(usize::MAX)
        };
        tops.sort_by_key(|top| order(top));
        let _ = write!(out, "\n{title}\n");
        for top in tops {
            for command in commands.iter().filter(|command| {
                command["command"]
                    .as_str()
                    .is_some_and(|path| path == top || path.starts_with(&format!("{top} ")))
            }) {
                section(&mut out, command);
            }
        }
    }
    let globals: Vec<String> = detail["global"]
        .as_array()
        .map(|all| {
            all.iter()
                .filter(|arg| shown(arg))
                .map(|arg| format!("  {:<28} {}", flag(arg), first_sentence(&arg["help"])))
                .collect()
        })
        .unwrap_or_default();
    let _ = write!(
        out,
        "\nEvery command also takes\n{}\n  {:<28} Say more about one command: bay <command> --help\n  {:<28} Name this bay's version\n\n`bay help --json` is the same list for programs, with every result code.\n",
        globals.join("\n"),
        "--help",
        "--version"
    );
    out
}

/// One command's lines: its usage, what it does, and each flag with what it does.
fn section(out: &mut String, command: &Value) {
    let path = command["command"].as_str().unwrap_or_default();
    let args: Vec<&Value> = command["args"]
        .as_array()
        .map(|all| all.iter().filter(|arg| shown(arg)).collect())
        .unwrap_or_default();
    let positionals: Vec<String> = args
        .iter()
        .filter(|arg| arg["positional"] == true)
        .map(|arg| positional(arg))
        .collect();
    let has_subcommands = command["subcommands"]
        .as_array()
        .is_some_and(|subs| !subs.is_empty());
    let mut usage = format!("  bay {path}");
    if has_subcommands {
        usage.push_str(" <COMMAND>");
    }
    for positional in &positionals {
        usage.push(' ');
        usage.push_str(positional);
    }
    let _ = writeln!(out, "{usage}");
    // The group's title already says Admin.
    let about = first_sentence(&command["about"]);
    let about = about
        .strip_prefix("Admin: ")
        .map_or_else(|| about.clone(), upper_first);
    if !about.is_empty() {
        let _ = writeln!(out, "      {about}");
    }
    // G321: what the command writes, and what it never writes.
    let listed = |key: &str| -> Vec<String> {
        command[key]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let (writes, never) = (listed("writes"), listed("never"));
    if !writes.is_empty() {
        let _ = writeln!(out, "      Writes: {}", writes.join("; "));
    }
    if !never.is_empty() {
        let _ = writeln!(out, "      Never: {}", never.join("; "));
    }
    for arg in args.iter().filter(|arg| arg["positional"] != true) {
        let _ = writeln!(
            out,
            "      {:<28} {}",
            flag(arg),
            first_sentence(&arg["help"])
        );
    }
}

/// `--name <NAME>`, `--name`, or `-C <NAME>` for a flag, as a person types it.
fn flag(arg: &Value) -> String {
    let name = arg["long"]
        .as_str()
        .or_else(|| arg["short"].as_str())
        .unwrap_or_default();
    if arg["takes_value"] == true {
        let value = arg["value_name"].as_str().map_or_else(
            || arg["name"].as_str().unwrap_or_default().to_uppercase(),
            str::to_owned,
        );
        let more = if arg["repeatable"] == true { "..." } else { "" };
        format!("{name} <{value}>{more}")
    } else {
        name.to_owned()
    }
}

/// `<NAME>` for a required positional argument, `[NAME]` for an optional one.
fn positional(arg: &Value) -> String {
    let name = arg["name"].as_str().unwrap_or_default().to_uppercase();
    let more = if arg["repeatable"] == true { "..." } else { "" };
    if arg["required"] == true {
        format!("<{name}>{more}")
    } else {
        format!("[{name}]{more}")
    }
}

/// Whether the guide names `arg`: not hidden, and not clap's own `--help` or `--version`, which
/// every command takes and the last line of the page names.
fn shown(arg: &Value) -> bool {
    !hidden(arg) && !builtin(arg)
}

/// Whether `arg` is clap's own `--help` or `--version`, which take no value. A command's own
/// `--version <VERSION>`, such as `bay update`'s, is shown.
fn builtin(arg: &Value) -> bool {
    matches!(arg["name"].as_str(), Some("help" | "version")) && arg["takes_value"] != true
}

/// Whether a command or a flag of the contract is hidden.
fn hidden(value: &Value) -> bool {
    value["hidden"] == true
}

/// `text` with its first letter in upper case, to begin a line.
fn upper_first(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// The first sentence of a help text: up to the first full stop that ends a sentence, so a
/// line stays one line. The rest is in `bay <command> --help`.
fn first_sentence(text: &Value) -> String {
    let text = text.as_str().unwrap_or_default().trim();
    let end = text
        .match_indices(". ")
        .map(|(at, _)| at)
        .next()
        .unwrap_or(text.len());
    text[..end].trim_end_matches('.').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sentence_keeps_one_sentence_whole() {
        assert_eq!(
            first_sentence(&Value::from("Open a change. Then land it.")),
            "Open a change"
        );
        assert_eq!(
            first_sentence(&Value::from("A path like `a.b` stays")),
            "A path like `a.b` stays"
        );
    }

    #[test]
    fn a_flag_shows_its_value() {
        let arg = serde_json::json!({"long": "--paths", "name": "paths", "takes_value": true, "repeatable": true});
        assert_eq!(flag(&arg), "--paths <PATHS>...");
        let bare = serde_json::json!({"long": "--wait", "name": "wait", "takes_value": false});
        assert_eq!(flag(&bare), "--wait");
    }
}
