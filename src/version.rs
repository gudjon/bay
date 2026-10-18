//! Dotted version numbers and the floors bay checks them against.

use std::cmp::Ordering;
use std::fmt;

/// The numeric parts of a version, compared part by part; missing parts count as zero.
#[derive(Clone, Debug)]
pub struct Version(Vec<u64>);

impl Version {
    /// Takes the first token that starts with a digit and reads its leading numeric parts:
    /// `git version 2.54.0 (Apple Git-157)` → 2.54.0, `2.56.GIT` → 2.56,
    /// `2.1.302 (Claude Code)` → 2.1.302.
    #[must_use]
    pub fn find(text: &str) -> Option<Self> {
        let token = text
            .split_whitespace()
            .find(|token| token.starts_with(|c: char| c.is_ascii_digit()))?;
        let parts: Vec<u64> = token
            .split('.')
            .map_while(|part| part.parse().ok())
            .collect();
        (!parts.is_empty()).then_some(Self(parts))
    }

    #[must_use]
    pub fn parse(text: &str) -> Self {
        Self::find(text).unwrap_or_else(|| Self(vec![0]))
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let len = self.0.len().max(other.0.len());
        (0..len)
            .map(|i| {
                let a = self.0.get(i).copied().unwrap_or(0);
                let b = other.0.get(i).copied().unwrap_or(0);
                a.cmp(&b)
            })
            .find(|ordering| ordering.is_ne())
            .unwrap_or(Ordering::Equal)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self.0.iter().map(ToString::to_string).collect();
        f.write_str(&parts.join("."))
    }
}

/// Git floors (`GIT-FEATURES` § 1, `DESIGN` § 15.2).
pub const GIT_FLOOR_AGENT_VIEW: &str = "2.40";
pub const GIT_FLOOR_HOSTED_TRUNK: &str = "2.54";
pub const GIT_SHARED_CACHE: &str = "2.48";
pub const GIT_RECOMMENDED: &str = "2.50";
/// The Claude Code version the hook events were read against (`DESIGN` § 11a).
pub const CLAUDE_HOOK_MINIMUM: &str = "2.1.302";

#[cfg(test)]
mod tests {
    use super::Version;

    #[test]
    fn reads_git_and_claude_version_lines() {
        assert_eq!(
            Version::parse("git version 2.54.0 (Apple Git-157)").to_string(),
            "2.54.0"
        );
        assert_eq!(Version::parse("git version 2.56.GIT").to_string(), "2.56");
        assert_eq!(
            Version::parse("2.1.302 (Claude Code)").to_string(),
            "2.1.302"
        );
    }

    #[test]
    fn compares_by_number_not_text() {
        assert!(Version::parse("2.100") > Version::parse("2.54"));
        assert_eq!(Version::parse("2.40"), Version::parse("2.40.0"));
        assert!(Version::parse("2.39.9") < Version::parse("2.40"));
        assert!(Version::parse("2.1.200") < Version::parse("2.1.302"));
    }
}
