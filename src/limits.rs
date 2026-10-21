//! Size limits: `[limits]` in `.bay/policy.toml` (owner ruling 2026-10-21).
//!
//! A repository says how big one change may be: `max_file_mib`, the size of any one file the
//! change adds or changes; `max_files`, how many files it touches; `max_added_lines`, the lines
//! it adds, a binary file counted by its size alone. Each key is optional and a positive
//! integer; an absent key is no limit. The limits are judged by the policy at the accepted base,
//! like every policy key, so a change never lifts its own. `bay land` judges them at submit,
//! before anything is submitted, and the executor at candidate build, before any check runs. A
//! change over one is refused `CHANGE_TOO_LARGE`, naming each limit exceeded, the value and the
//! largest offenders.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::git::Git;
use crate::result::{BayResult, Reason};

/// One MiB, in bytes.
const MIB: u64 = 1024 * 1024;

/// How many offending paths a refusal names for each limit.
const OFFENDERS: usize = 5;

/// The `[limits]` table. `None` is no limit.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default)]
    pub max_file_mib: Option<u64>,
    #[serde(default)]
    pub max_files: Option<u64>,
    #[serde(default)]
    pub max_added_lines: Option<u64>,
}

impl Limits {
    /// Whether any limit is set.
    #[must_use]
    pub const fn is_set(&self) -> bool {
        self.max_file_mib.is_some() || self.max_files.is_some() || self.max_added_lines.is_some()
    }

    /// The table as the policy states it, each key only when set.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut table = serde_json::Map::new();
        for (key, limit) in self.keys() {
            if let Some(limit) = limit {
                table.insert(key.to_owned(), json!(limit));
            }
        }
        Value::Object(table)
    }

    fn keys(&self) -> [(&'static str, Option<u64>); 3] {
        [
            ("max_file_mib", self.max_file_mib),
            ("max_files", self.max_files),
            ("max_added_lines", self.max_added_lines),
        ]
    }

    /// A validity rule of the policy (`POLICY_INVALID`): each limit is a positive integer. A
    /// negative, fractional or quoted value does not parse; zero is refused here.
    ///
    /// # Errors
    ///
    /// Why the table is invalid.
    pub fn validate(&self) -> Result<(), String> {
        for (key, limit) in self.keys() {
            if limit == Some(0) {
                return Err(format!(
                    "[limits] {key} = 0: each limit is a positive integer; leave the key out for no limit"
                ));
            }
        }
        Ok(())
    }
}

/// One file a change touches: its path, its new size (`None` when deleted or not a blob) and
/// the lines it adds (`None` for a binary file, or when lines are not counted).
struct Touched {
    path: String,
    bytes: Option<u64>,
    added: Option<u64>,
}

/// Each limit of `limits` that the change from tree-ish `from` to tree-ish `to` exceeds, as the
/// rows of a `CHANGE_TOO_LARGE` detail: `limit`, `max`, `value` (bytes for `max_file_mib`),
/// `unit`, and `paths`, the largest offenders first. Empty when within every limit, and no git
/// call is made when no limit is set.
///
/// # Errors
///
/// A git fault, or an answer this cannot read.
pub fn exceeded(git: &Git, limits: &Limits, from: &str, to: &str) -> Result<Vec<Value>, String> {
    if !limits.is_set() {
        return Ok(Vec::new());
    }
    let mut touched = touched(git, from, to, limits.max_file_mib.is_some())?;
    if limits.max_added_lines.is_some() {
        added_lines(git, from, to, &mut touched)?;
    }
    let mut rows = Vec::new();
    if let Some(max) = limits.max_file_mib {
        let over_bytes = max.saturating_mul(MIB);
        let mut over: Vec<&Touched> = touched
            .iter()
            .filter(|file| file.bytes.is_some_and(|bytes| bytes > over_bytes))
            .collect();
        over.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
        if let Some(largest) = over.first().and_then(|file| file.bytes) {
            rows.push(json!({
                "limit": "max_file_mib",
                "max": max,
                "value": largest,
                "unit": "bytes",
                "paths": over.iter().take(OFFENDERS).map(|file| &file.path).collect::<Vec<_>>(),
            }));
        }
    }
    if let Some(max) = limits.max_files {
        let count = u64::try_from(touched.len()).unwrap_or(u64::MAX);
        if count > max {
            rows.push(json!({
                "limit": "max_files",
                "max": max,
                "value": count,
                "unit": "files",
                "paths": touched.iter().take(OFFENDERS).map(|file| &file.path).collect::<Vec<_>>(),
            }));
        }
    }
    if let Some(max) = limits.max_added_lines {
        let total: u64 = touched
            .iter()
            .filter_map(|file| file.added)
            .fold(0, u64::saturating_add);
        if total > max {
            let mut most: Vec<&Touched> = touched
                .iter()
                .filter(|file| file.added.is_some_and(|added| added > 0))
                .collect();
            most.sort_by(|a, b| b.added.cmp(&a.added).then_with(|| a.path.cmp(&b.path)));
            rows.push(json!({
                "limit": "max_added_lines",
                "max": max,
                "value": total,
                "unit": "lines",
                "paths": most.iter().take(OFFENDERS).map(|file| &file.path).collect::<Vec<_>>(),
            }));
        }
    }
    Ok(rows)
}

/// The files the change from `from` to `to` touches, in path order, each with its new blob's
/// size when `sizes`.
fn touched(git: &Git, from: &str, to: &str, sizes: bool) -> Result<Vec<Touched>, String> {
    let raw = git
        .run(&["diff-tree", "-r", "-z", "--no-renames", "--raw", from, to])
        .map_err(|error| error.message())?;
    let mut parts = raw.split('\0').filter(|part| !part.is_empty());
    let mut touched = Vec::new();
    let mut blobs: Vec<(usize, String)> = Vec::new();
    while let Some(meta) = parts.next() {
        let path = parts
            .next()
            .ok_or_else(|| format!("diff-tree gave no path after {meta:?}"))?;
        // ":<old mode> <new mode> <old id> <new id> <status>"
        let fields: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let [_, new_mode, _, new_id, status] = fields.as_slice() else {
            return Err(format!(
                "diff-tree gave an entry this cannot read: {meta:?}"
            ));
        };
        // A deleted file, or a submodule, has no blob of its own here.
        if sizes && !status.starts_with('D') && *new_mode != "160000" {
            blobs.push((touched.len(), (*new_id).to_owned()));
        }
        touched.push(Touched {
            path: path.to_owned(),
            bytes: None,
            added: None,
        });
    }
    if !blobs.is_empty() {
        let mut input = blobs
            .iter()
            .map(|(_, id)| id.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        input.push('\n');
        let out = git
            .run_with(&["cat-file", "--batch-check"], &[], Some(input.as_bytes()))
            .map_err(|error| error.message())?;
        let mut lines = out.lines();
        for (index, id) in &blobs {
            let line = lines
                .next()
                .ok_or_else(|| format!("cat-file gave no size for {id}"))?;
            // "<id> <type> <size>", or "<id> missing".
            let size = line
                .rsplit(' ')
                .next()
                .and_then(|size| size.parse::<u64>().ok())
                .filter(|_| !line.ends_with(" missing"))
                .ok_or_else(|| format!("the size of {id} cannot be read: {line}"))?;
            touched[*index].bytes = Some(size);
        }
    }
    Ok(touched)
}

/// The lines each text file of `touched` adds from `from` to `to`; a binary file adds none
/// that count (`--numstat` gives it `-`), so it is judged by its size alone.
fn added_lines(git: &Git, from: &str, to: &str, touched: &mut [Touched]) -> Result<(), String> {
    let out = git
        .run(&[
            "diff-tree",
            "-r",
            "-z",
            "--no-renames",
            "--numstat",
            from,
            to,
        ])
        .map_err(|error| error.message())?;
    for entry in out.split('\0').filter(|entry| !entry.is_empty()) {
        // "<added>\t<deleted>\t<path>"
        let mut fields = entry.splitn(3, '\t');
        let (Some(added), Some(_), Some(path)) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(format!(
                "diff-tree gave a count this cannot read: {entry:?}"
            ));
        };
        let added = added.trim_start_matches(['\n', '\r']).parse::<u64>().ok();
        if let Some(file) = touched.iter_mut().find(|file| file.path == path) {
            file.added = added;
        }
    }
    Ok(())
}

/// The `CHANGE_TOO_LARGE` refusal (blocked): each limit of `limits` exceeded (`exceeded`'s
/// rows), judged by the policy at `base`, `at` submit or at the candidate's build.
#[must_use]
pub fn refusal(limits: &Limits, exceeded: &[Value], base: &str, at: &str) -> BayResult {
    BayResult::blocked(Reason::ChangeTooLarge).with_detail(json!({
        "exceeded": exceeded,
        "limits": limits.to_json(),
        "policy": crate::policy::POLICY_PATH,
        "base": base,
        "at": at,
        "recovery": "keep large files outside the repository (a shared drive, or a large-file store) or split the change, then bay land again",
    }))
}

#[cfg(test)]
mod tests {
    use super::Limits;

    #[test]
    fn a_zero_limit_is_invalid_and_no_key_is_no_limit() {
        assert!(!Limits::default().is_set());
        assert!(Limits::default().validate().is_ok());
        let zero = Limits {
            max_files: Some(0),
            ..Limits::default()
        };
        assert!(zero.validate().unwrap_err().contains("max_files"));
        let set = Limits {
            max_added_lines: Some(10),
            ..Limits::default()
        };
        assert!(set.is_set());
        assert_eq!(set.to_json(), serde_json::json!({"max_added_lines": 10}));
    }

    #[test]
    fn a_negative_fractional_or_quoted_limit_does_not_parse() {
        for text in [
            "max_files = -1",
            "max_files = 1.5",
            "max_files = \"10\"",
            "max_bytes = 10",
        ] {
            assert!(toml::from_str::<Limits>(text).is_err(), "{text}");
        }
        let parsed: Limits = toml::from_str("max_file_mib = 5\n").unwrap();
        assert_eq!(parsed.max_file_mib, Some(5));
    }
}
