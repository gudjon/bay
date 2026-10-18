//! RFC 8785 JSON Canonicalization Scheme for the values bay hashes: objects, arrays,
//! strings, integers, booleans and null. Keys sort by UTF-16 code units; no whitespace.

use std::fmt::Write;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// The canonical text of `value`.
///
/// # Errors
///
/// A number that is not an integer (bay's hashed documents carry none).
pub fn canonical(value: &Value) -> Result<String, String> {
    let mut out = String::new();
    write_value(value, &mut out)?;
    Ok(out)
}

/// `sha256:` and the lower-case hex sha256 of the canonical text.
///
/// # Errors
///
/// See [`canonical`].
pub fn digest(value: &Value) -> Result<String, String> {
    let text = canonical(value)?;
    Ok(format!(
        "sha256:{}",
        crate::agent_settings::hex(&Sha256::digest(text.as_bytes()))
    ))
}

fn write_value(value: &Value, out: &mut String) -> Result<(), String> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                let _ = write!(out, "{int}");
            } else if let Some(int) = number.as_u64() {
                let _ = write!(out, "{int}");
            } else {
                return Err(format!("non-integer number {number} in a hashed document"));
            }
        }
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_value(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_value(&map[key.as_str()], out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::canonical;
    use serde_json::json;

    #[test]
    fn sorts_keys_and_drops_whitespace() {
        let value = json!({"b": [1, true, null], "a": {"y": "x\n", "x": -2}});
        assert_eq!(
            canonical(&value).unwrap(),
            r#"{"a":{"x":-2,"y":"x\n"},"b":[1,true,null]}"#
        );
    }

    #[test]
    fn keys_sort_by_utf16_code_units() {
        // RFC 8785 § 3.2.3: U+20AC sorts after U+1F600's surrogate pair would by code point,
        // but before it by UTF-16 code unit.
        let value = json!({"\u{1F600}": 1, "\u{20AC}": 2, "a": 3});
        assert_eq!(
            canonical(&value).unwrap(),
            "{\"a\":3,\"\u{20AC}\":2,\"\u{1F600}\":1}"
        );
    }

    #[test]
    fn refuses_fractions() {
        assert!(canonical(&json!({"x": 1.5})).is_err());
    }
}
