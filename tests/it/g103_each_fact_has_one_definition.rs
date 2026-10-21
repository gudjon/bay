//! G103. Each fact has one definition.
//!
//! A fact defined twice drifts (verbs-and-size ruling § 2), so this test fails when a second
//! definition appears, for a fixed list of facts:
//!
//! - every reason code is declared once, in `src/result.rs`, and no other source file holds a
//!   string literal that is a reason code: a refusal names its code through `Reason`;
//! - every bay ref namespace (`refs/bay/…`, `refs/notes/bay/…`) is spelled in one module,
//!   `src/names.rs`, and every other module builds its names from that module's constants;
//! - every flag `INTERFACES` § 1 documents is in the `--help` of its command.
//!
//! Test modules are not definitions: their fixtures may spell a code or a ref name.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use bay::result::Reason;

/// The flags of `INTERFACES` § 1, by command.
const FLAGS: &[(&[&str], &[&str])] = &[
    (&[], &["-C", "--json", "--version", "--help"]),
    (
        &["start"],
        &[
            "--paths",
            "--intent-revision",
            "--new",
            "--resume",
            "--fixes",
        ],
    ),
    (&["land"], &["--wait", "--abandon"]),
    (&["undo"], &["--wait"]),
    (&["status"], &["--fleet"]),
    (&["update"], &["--version", "--check"]),
    (&["host", "init"], &["--trunk", "--object-format"]),
    (&["executor", "run"], &["--once", "--interval-s"]),
    (&["share"], &["--remote", "--bundle"]),
];

/// Flags § 1 lists that `--help` does not show, each with a value it parses. `bay update
/// --channel` is accepted and ignored since update channels were removed. `bay land --change`
/// and `--observe` are kept for older scripts (G293): the change is positional, and
/// `--wait=SECONDS` observes.
const HIDDEN_FLAGS: &[(&str, &str, &str)] = &[
    ("update", "--channel", "x"),
    ("land", "--change", "abcd2345"),
    ("land", "--observe", "2"),
];

fn src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `.rs` file under `dir`, with its path relative to `src/`.
fn sources(dir: &Path, out: &mut Vec<(String, String)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let name = path
                .strip_prefix(src())
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.push((name, std::fs::read_to_string(&path).unwrap()));
        }
    }
}

/// The source of each file without its test module.
fn production_sources() -> Vec<(String, String)> {
    let mut all = Vec::new();
    sources(&src(), &mut all);
    for (_, text) in &mut all {
        if let Some(at) = text.find("#[cfg(test)]\nmod ") {
            text.truncate(at);
        }
    }
    all.sort();
    all
}

/// The string literals of Rust source `text`, with their line numbers, outside comments.
/// Raw strings, byte strings, char literals and lifetimes are told apart; escapes are kept as
/// written.
fn literals(text: &str) -> Vec<(usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let (mut i, mut line) = (0, 1);
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '\n' => {
                line += 1;
                i += 1;
            }
            '/' if next == Some('/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if next == Some('*') => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    if chars[i] == '\n' {
                        line += 1;
                    }
                    i += 1;
                }
                i += 2;
            }
            '\'' => {
                // A char literal ('x', '\n', '"'); otherwise a lifetime or label.
                if next == Some('\\') {
                    // Past the backslash and the escaped char, to the closing quote.
                    i += 3;
                    while i < chars.len() && chars[i] != '\'' {
                        i += 1;
                    }
                    i += 1;
                } else if chars.get(i + 2) == Some(&'\'') {
                    i += 3;
                } else {
                    i += 1;
                }
            }
            'r' if (next == Some('"') || next == Some('#'))
                && (i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_')) =>
            {
                let mut j = i + 1;
                let mut hashes = 0;
                while chars.get(j) == Some(&'#') {
                    hashes += 1;
                    j += 1;
                }
                if chars.get(j) != Some(&'"') {
                    i += 1;
                    continue;
                }
                let start_line = line;
                let mut body = String::new();
                j += 1;
                loop {
                    if j >= chars.len() {
                        break;
                    }
                    if chars[j] == '"' && (1..=hashes).all(|k| chars.get(j + k) == Some(&'#')) {
                        j += 1 + hashes;
                        break;
                    }
                    if chars[j] == '\n' {
                        line += 1;
                    }
                    body.push(chars[j]);
                    j += 1;
                }
                out.push((start_line, body));
                i = j;
            }
            '"' => {
                let start_line = line;
                let mut body = String::new();
                let mut j = i + 1;
                while j < chars.len() && chars[j] != '"' {
                    if chars[j] == '\\' {
                        body.push(chars[j]);
                        j += 1;
                    }
                    if chars[j] == '\n' {
                        line += 1;
                    }
                    body.push(chars[j]);
                    j += 1;
                }
                out.push((start_line, body));
                i = j + 1;
            }
            _ => i += 1,
        }
    }
    out
}

#[test]
fn the_lexer_finds_literals_and_skips_comments_chars_and_lifetimes() {
    let text = "// \"IN_COMMENT\"\nfn a<'b>(x: &'b str) -> char { let _ = r#\"RAW\"#; let _ = '\"'; let _ = \"PLAIN\\\"Q\"; '\\'' }\n/* \"BLOCK\" */ const C: &str = \"LAST\";\n";
    let found: Vec<String> = literals(text).into_iter().map(|(_, text)| text).collect();
    assert_eq!(found, ["RAW", "PLAIN\\\"Q", "LAST"]);
}

#[test]
fn every_reason_code_is_declared_once_in_result_rs() {
    let sources = production_sources();
    let codes: Vec<&str> = Reason::ALL.iter().map(|reason| reason.code()).collect();
    let (_, result_rs) = sources
        .iter()
        .find(|(name, _)| name == "result.rs")
        .unwrap();
    let declared = literals(result_rs);
    for code in &codes {
        let count = declared.iter().filter(|(_, text)| text == code).count();
        assert_eq!(
            count, 1,
            "{code} is declared {count} times in src/result.rs"
        );
    }
    let mut second = Vec::new();
    for (name, text) in &sources {
        if name == "result.rs" {
            continue;
        }
        for (line, literal) in literals(text) {
            if codes.contains(&literal.as_str()) {
                second.push(format!(
                    "src/{name}:{line}: \"{literal}\" (name it as Reason::…code())"
                ));
            }
        }
    }
    assert!(
        second.is_empty(),
        "a reason code defined outside src/result.rs:\n{}",
        second.join("\n")
    );
}

#[test]
fn every_bay_ref_namespace_is_spelled_in_names_rs() {
    let mut second = Vec::new();
    let mut in_names = 0;
    for (name, text) in production_sources() {
        for (line, literal) in literals(&text) {
            if literal.contains("refs/bay/") || literal.contains("refs/notes/bay/") {
                if name == "names.rs" {
                    in_names += 1;
                } else {
                    second.push(format!("src/{name}:{line}: \"{literal}\""));
                }
            }
        }
    }
    assert!(in_names > 0, "src/names.rs spells no bay ref namespace");
    assert!(
        second.is_empty(),
        "a bay ref namespace spelled outside src/names.rs (use its constants):\n{}",
        second.join("\n")
    );
}

#[test]
fn every_flag_interfaces_1_documents_is_in_help() {
    let mut missing = Vec::new();
    for (command, flags) in FLAGS {
        let output = Command::cargo_bin("bay")
            .unwrap()
            .args(*command)
            .arg("--help")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "bay {command:?} --help: {output:?}"
        );
        let help = String::from_utf8(output.stdout).unwrap();
        for flag in *flags {
            let shown = help
                .split(|c: char| c.is_whitespace() || c == ',' || c == '=' || c == '[')
                .any(|word| word == *flag);
            if !shown {
                missing.push(format!("bay {} {flag}", command.join(" ")));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "flags INTERFACES § 1 documents that --help does not show:\n{}",
        missing.join("\n")
    );
    // The hidden flags are still accepted: clap parses them, so a usage error never names them.
    for (command, flag, value) in HIDDEN_FLAGS {
        let output = Command::cargo_bin("bay")
            .unwrap()
            .args([*command, *flag, *value, "--help"])
            .output()
            .unwrap();
        assert!(output.status.success(), "bay {command} {flag}: {output:?}");
    }
}
