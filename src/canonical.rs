//! RFC 8785 JSON Canonicalization Scheme for the values bay hashes and signs: objects, arrays,
//! strings, numbers, booleans and null. Keys sort by UTF-16 code units; no whitespace. A
//! fraction (an evidence record's `seconds`) is written as ECMAScript writes a number
//! (RFC 8785 § 3.2.2.3).

use std::fmt::Write;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// The canonical text of `value`.
///
/// # Errors
///
/// A number that is not finite.
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
                write_fraction(number.as_f64().unwrap_or(f64::NAN), out)?;
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

/// `value` as ECMAScript `Number.prototype.toString` writes it: the shortest digits that
/// read back as `value`, in positional form for a decimal exponent from -6 to 20, else in
/// exponent form.
fn write_fraction(value: f64, out: &mut String) -> Result<(), String> {
    if !value.is_finite() {
        return Err(format!("non-finite number {value} in a canonical document"));
    }
    if value == 0.0 {
        out.push('0');
        return Ok(());
    }
    if value < 0.0 {
        out.push('-');
    }
    // Rust's `{:e}` writes the shortest round-trip digits: `d[.ddd]e<exp>`.
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific
        .split_once('e')
        .ok_or_else(|| format!("unexpected float text {scientific}"))?;
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let exponent: i32 = exponent
        .parse()
        .map_err(|_| format!("unexpected float text {scientific}"))?;
    let k = i32::try_from(digits.len()).map_err(|error| error.to_string())?;
    // ECMAScript's n: the value is 0.<digits> times ten to the n.
    let n = exponent + 1;
    let zeros = |count: i32| "0".repeat(usize::try_from(count).unwrap_or(0));
    if k <= n && n <= 21 {
        out.push_str(&digits);
        out.push_str(&zeros(n - k));
    } else if 0 < n && n <= 21 {
        let point = usize::try_from(n).unwrap_or(0);
        out.push_str(&digits[..point]);
        out.push('.');
        out.push_str(&digits[point..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        out.push_str(&zeros(-n));
        out.push_str(&digits);
    } else {
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let _ = write!(out, "e{}{}", if n > 0 { "+" } else { "-" }, (n - 1).abs());
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
    fn writes_fractions_as_ecmascript_does() {
        // RFC 8785 Appendix B's number cases, and the check-cost values bay records.
        for (value, text) in [
            (4.1, "4.1"),
            (4.0, "4"),
            (-0.0, "0"),
            (0.000_001, "0.000001"),
            (1e-7, "1e-7"),
            (1e21, "1e+21"),
            (123_456_789_012_345_680_000.0, "123456789012345680000"),
            (-1.5, "-1.5"),
            (5e-324, "5e-324"),
            (1.797_693_134_862_315_7e308, "1.7976931348623157e+308"),
            (333_333_333.333_333_3, "333333333.3333333"),
        ] {
            assert_eq!(canonical(&json!(value)).unwrap(), text, "{value:e}");
        }
    }
}
