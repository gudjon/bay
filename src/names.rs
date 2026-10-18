//! Bay ref names (`DESIGN` § 10a).
//!
//! Every id bay puts in a ref name is generated from a fixed alphabet; the names an operator
//! chooses (producer, executor, machine) use lower-case letters, digits and `-`, at most 32
//! characters, never `/`. So no bay ref is a directory of another (Git refuses a ref that is
//! also a directory of another, on both backends), and `<producer>/<id>` parses at its one `/`.

/// Validates a producer, executor or machine name.
///
/// # Errors
///
/// The name is empty, longer than 32 characters, or has a character outside `a-z 0-9 -`.
pub fn operator_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 32 {
        return Err(format!("the name {name:?} must have 1 to 32 characters"));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(format!(
            "the name {name:?} may use only lower-case letters, digits and -"
        ));
    }
    Ok(())
}

fn hex32(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn decimal(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) && !value.starts_with('0')
}

/// Validates a full bay ref name: its namespace and every id or name in it. Refs outside
/// `refs/bay/` and `refs/notes/bay/` are not bay's and pass.
///
/// # Errors
///
/// The name is in a bay namespace but is not a bay ref name.
pub fn bay_ref(name: &str) -> Result<(), String> {
    let refuse = |why: &str| Err(format!("{name} is not a bay ref name: {why}"));
    if let Some(producer) = name.strip_prefix("refs/notes/bay/evidence/") {
        return operator_name(producer).map_err(|why| format!("{name}: {why}"));
    }
    if name.starts_with("refs/notes/bay/") {
        return refuse("bay's notes are refs/notes/bay/evidence/<producer>");
    }
    let Some(rest) = name.strip_prefix("refs/bay/") else {
        return Ok(());
    };
    if rest == "registry" {
        return Ok(());
    }
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        ["changes" | "requests" | "landed", id] => {
            if crate::ids::is_change_id(id) {
                Ok(())
            } else {
                refuse("a change id is 8 characters of lower-case base32")
            }
        }
        ["intent" | "landed-intent", hash, generation] => {
            if hex32(hash) && decimal(generation) {
                Ok(())
            } else {
                refuse("an intent claim is <32 hex>/<generation>")
            }
        }
        ["queue" | "queue-done" | "landed-index", number] => {
            if decimal(number) {
                Ok(())
            } else {
                refuse("a queue place or shard is a decimal number")
            }
        }
        ["live" | "log", operator] => {
            operator_name(operator).map_err(|why| format!("{name}: {why}"))
        }
        _ => refuse("unknown namespace"),
    }
}

/// Whether some name in `names` is a directory prefix of another (`refs/x/a` and
/// `refs/x/a/b`): such a set cannot exist at once.
#[must_use]
pub fn prefix_clash<'a>(names: &[&'a str]) -> Option<(&'a str, &'a str)> {
    for a in names {
        for b in names {
            if b.len() > a.len() && b.starts_with(a) && b.as_bytes()[a.len()] == b'/' {
                return Some((a, b));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{bay_ref, operator_name, prefix_clash};

    #[test]
    fn operator_names_are_short_lower_case_and_have_no_slash() {
        assert!(operator_name("ci-linux-2").is_ok());
        assert!(operator_name("ci/linux").is_err());
        assert!(operator_name("CI").is_err());
        assert!(operator_name(&"a".repeat(33)).is_err());
        assert!(operator_name("").is_err());
    }

    #[test]
    fn ids_come_from_the_fixed_alphabet() {
        assert!(bay_ref("refs/bay/changes/abcd2345").is_ok());
        assert!(bay_ref("refs/bay/changes/ABCD2345").is_err());
        assert!(bay_ref("refs/bay/changes/abcd2345/request").is_err());
        assert!(bay_ref("refs/bay/queue/07").is_err());
        assert!(bay_ref("refs/bay/intent/0123456789abcdef0123456789abcdef/1").is_ok());
        assert!(bay_ref("refs/bay/whatever").is_err());
        assert!(bay_ref("refs/notes/bay/evidence/ci/linux").is_err());
        assert!(bay_ref("refs/heads/main").is_ok());
    }

    #[test]
    fn a_name_that_is_a_prefix_of_another_is_found() {
        assert_eq!(
            prefix_clash(&["refs/bay/changes/abc", "refs/bay/changes/abc/request"]),
            Some(("refs/bay/changes/abc", "refs/bay/changes/abc/request"))
        );
        assert_eq!(
            prefix_clash(&["refs/bay/changes/abc", "refs/bay/changes/abcd"]),
            None
        );
    }
}
