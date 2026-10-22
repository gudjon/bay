//! G327 (0.3.1, the site's synopsis ask): `bay help` shows which values may be left out, and
//! names each argument by its own placeholder.
//!
//! `bay merge --wait` waits for the outcome and `--wait=SECONDS` waits that long, and `bay
//! status --since` reads from the start; the page showed both as if the value were required.
//! So `bay help --json` marks each such flag `value_optional` (with `require_equals` when the
//! value is written `--flag=VALUE`), and the page writes `--wait[=<SECONDS>]` and `--since
//! [<CURSOR>]`. A positional argument shows its placeholder, as `--help` does:
//! `[PUBLIC_KEY_FILE]`, not the field name.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/contract.rs` `argument`: `range.min_values() == 0` to `false` (no flag is optional).
//! - `src/guide.rs` `flag`: drop the `(true, true)` arm (`--wait <SECONDS>` again).
//! - `src/guide.rs` `positional`: name the argument by `arg["name"]` again.

use crate::support;

use serde_json::Value;
use support::TestBox;

fn arg<'a>(contract: &'a Value, command: &str, long: &str) -> &'a Value {
    contract["detail"]["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["command"] == command)
        .and_then(|entry| {
            entry["args"]
                .as_array()
                .unwrap()
                .iter()
                .find(|arg| arg["long"] == long)
        })
        .unwrap_or_else(|| panic!("bay {command} has no {long}"))
}

#[test]
fn optional_values_are_marked_and_shown_as_optional() {
    let test_box = TestBox::new();
    // `bay_ok` adds `--json`: this is `bay help --json`.
    let contract = test_box.bay_ok(test_box.path(), &["help"]);
    let wait = arg(&contract, "merge", "--wait");
    assert_eq!(wait["value_optional"], true, "{wait}");
    assert_eq!(wait["require_equals"], true, "{wait}");
    let since = arg(&contract, "status", "--since");
    assert_eq!(since["value_optional"], true, "{since}");
    assert_eq!(since["require_equals"], false, "{since}");
    let paths = arg(&contract, "start", "--paths");
    assert_eq!(paths["value_optional"], false, "{paths}");

    let out = test_box.bay(test_box.path()).arg("help").output().unwrap();
    let page = String::from_utf8(out.stdout).unwrap();
    assert!(page.contains("--wait[=<SECONDS>]"), "{page}");
    assert!(page.contains("--since [<CURSOR>]"), "{page}");
    assert!(page.contains("--paths <PATHS>..."), "{page}");
    assert!(
        page.contains("bay enrol <CLASS> <NAME> [PUBLIC_KEY_FILE]"),
        "{page}"
    );
}
