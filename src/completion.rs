//! Completion dimensions and profiles (`DESIGN` § 7; `INTERFACES` § 7; ruling R12).
//!
//! A change is published, checked, running and effective, each established by its own
//! evidence. A profile, `[profile.<name>] require = [...]` in `.bay/policy.toml`, says which of
//! them a change needs; `[policy] profile` in `.bay/config.toml` selects one. `bay status`
//! reports each dimension apart and a profile verdict that never collapses into "done": a
//! required dimension that is missing is named ("published and checked; runtime confirmation
//! missing"), and a profile bay cannot read is `unknown`, never `complete`. A profile that sets
//! `require_signed = true` never counts unsigned evidence (G160, owner ruling Q6): a required
//! dimension that rests only on unsigned records is `unknown`, never satisfied.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// One completion dimension (`DESIGN` § 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    /// The change is on trunk.
    Published,
    /// Named checks passed for identified inputs.
    Checked,
    /// A producer-signed runtime observation names an artifact built from the landed commit.
    Running,
    /// A producer-signed outcome check passed in that runtime.
    Effective,
}

impl Dimension {
    /// Every dimension, in the order a summary names them.
    pub const ALL: [Self; 4] = [
        Self::Published,
        Self::Checked,
        Self::Running,
        Self::Effective,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::Checked => "checked",
            Self::Running => "running",
            Self::Effective => "effective",
        }
    }

    /// How a summary names this dimension when it is missing.
    const fn missing(self) -> &'static str {
        match self {
            Self::Published => "not published",
            Self::Checked => "not checked",
            Self::Running => "runtime confirmation missing",
            Self::Effective => "outcome check missing",
        }
    }
}

/// The profile a policy without one of that name means by `default`: today's behaviour, a
/// change is complete once published and checked; runtime is reported, not required.
pub const DEFAULT_PROFILE: &str = "default";
pub const DEFAULT_REQUIRE: [Dimension; 2] = [Dimension::Published, Dimension::Checked];

/// A `[profile.<name>]` table of `.bay/policy.toml`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileEntry {
    pub require: Vec<Dimension>,
    /// Whether only signed evidence may establish a dimension (G160). Off by default: unsigned
    /// records count, as unsigned check records do (owner ruling Q6).
    #[serde(default)]
    pub require_signed: bool,
}

/// What one profile asks: the dimensions it requires, and whether only signed evidence counts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub require: Vec<Dimension>,
    pub require_signed: bool,
}

/// What each `[profile.<name>]` table asks, once each is valid: a name, and a `require`
/// list that is not empty and names each dimension at most once. A dimension bay does not
/// know fails to parse before this, so a typo is never a profile that requires less.
///
/// # Errors
///
/// The first rule a profile breaks, naming it.
pub fn requirements(
    profiles: BTreeMap<String, ProfileEntry>,
) -> Result<BTreeMap<String, Profile>, String> {
    validate(&profiles)?;
    Ok(profiles
        .into_iter()
        .map(|(name, entry)| {
            let profile = Profile {
                require: entry.require,
                require_signed: entry.require_signed,
            };
            (name, profile)
        })
        .collect())
}

fn validate(profiles: &BTreeMap<String, ProfileEntry>) -> Result<(), String> {
    for (name, entry) in profiles {
        if name.is_empty() {
            return Err("a [profile] table has an empty name".to_owned());
        }
        if entry.require.is_empty() {
            return Err(format!(
                "[profile.{name}] requires nothing; a profile names at least one dimension"
            ));
        }
        let mut seen = entry.require.clone();
        seen.sort();
        seen.dedup();
        if seen.len() != entry.require.len() {
            return Err(format!("[profile.{name}] names a dimension twice"));
        }
    }
    Ok(())
}

/// The selected profile's requirement: the policy's table of that name, `default`'s stated
/// requirement when the policy declares none of that name, and otherwise the reason it cannot
/// be read.
///
/// # Errors
///
/// The name is selected but no profile of that name exists: the verdict is `unknown`.
pub fn requirement(name: &str, profiles: &BTreeMap<String, Profile>) -> Result<Profile, String> {
    match profiles.get(name) {
        Some(profile) => Ok(profile.clone()),
        None if name == DEFAULT_PROFILE => Ok(Profile {
            require: DEFAULT_REQUIRE.to_vec(),
            require_signed: false,
        }),
        None => Err(format!(
            "[policy] profile = \"{name}\" names no [profile.{name}] in the policy"
        )),
    }
}

/// The profile verdict as JSON: its name, what it requires, what is missing, `complete` or
/// `incomplete` (or `unknown` when the profile cannot be read), and a summary that names every
/// required dimension that is met and every one that is missing. `met` lists the dimensions
/// the change's evidence establishes, and `unsigned` those of them that rest only on unsigned
/// records (G160). The required ones among those are named in `unsigned`; under
/// `require_signed` they are not met, and a verdict with nothing missing is then `unknown`.
#[must_use]
pub fn verdict(
    name: &str,
    profile: Result<Profile, String>,
    met: &[Dimension],
    unsigned: &[Dimension],
) -> Value {
    let profile = match profile {
        Ok(profile) => profile,
        Err(why) => {
            return json!({
                "name": name,
                "require": null,
                "require_signed": null,
                "missing": null,
                "unsigned": null,
                "verdict": "unknown",
                "summary": format!("completion unknown: {why}"),
            });
        }
    };
    let require = &profile.require;
    let (have, missing): (Vec<Dimension>, Vec<Dimension>) = Dimension::ALL
        .into_iter()
        .filter(|dimension| require.contains(dimension))
        .partition(|dimension| met.contains(dimension));
    let resting: Vec<Dimension> = have
        .iter()
        .copied()
        .filter(|dimension| unsigned.contains(dimension))
        .collect();
    // Under `require_signed`, a dimension on unsigned records alone is neither met nor missing.
    let (have, unknown): (Vec<Dimension>, Vec<Dimension>) = have
        .into_iter()
        .partition(|dimension| !profile.require_signed || !resting.contains(dimension));
    let mut parts = Vec::new();
    if !have.is_empty() {
        parts.push(and_list(have.iter().map(|dimension| dimension.as_str())));
    }
    if !unknown.is_empty() {
        parts.push(format!(
            "{} unknown: only unsigned evidence, and [profile.{name}] sets require_signed",
            and_list(unknown.iter().map(|dimension| dimension.as_str()))
        ));
    }
    parts.extend(
        missing
            .iter()
            .map(|dimension| dimension.missing().to_owned()),
    );
    let word = if !missing.is_empty() {
        "incomplete"
    } else if !unknown.is_empty() {
        "unknown"
    } else {
        "complete"
    };
    json!({
        "name": name,
        "require": require.iter().map(|dimension| dimension.as_str()).collect::<Vec<_>>(),
        "require_signed": profile.require_signed,
        "missing": missing.iter().map(|dimension| dimension.as_str()).collect::<Vec<_>>(),
        "unsigned": resting.iter().map(|dimension| dimension.as_str()).collect::<Vec<_>>(),
        "verdict": word,
        "summary": parts.join("; "),
    })
}

/// "a", "a and b", "a, b and c".
fn and_list<'a>(words: impl Iterator<Item = &'a str>) -> String {
    let words: Vec<&str> = words.collect();
    match words.split_last() {
        Some((last, [])) => (*last).to_owned(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{Dimension, Profile, ProfileEntry, requirement, validate, verdict};

    const PUBLISHED_AND_CHECKED: [Dimension; 2] = [Dimension::Published, Dimension::Checked];

    #[test]
    fn a_missing_runtime_dimension_is_named_never_done() {
        let all = Profile {
            require: Dimension::ALL.to_vec(),
            require_signed: false,
        };
        let out = verdict("deploy", Ok(all), &PUBLISHED_AND_CHECKED, &[]);
        assert_eq!(out["verdict"], "incomplete");
        assert_eq!(
            out["summary"],
            "published and checked; runtime confirmation missing; outcome check missing"
        );
        assert_eq!(out["missing"], serde_json::json!(["running", "effective"]));
        assert!(!out.to_string().contains("done"));
    }

    #[test]
    fn the_default_profile_requires_published_and_checked() {
        let require = requirement("default", &BTreeMap::new()).unwrap();
        assert_eq!(require.require, [Dimension::Published, Dimension::Checked]);
        assert!(!require.require_signed);
        let out = verdict("default", Ok(require), &PUBLISHED_AND_CHECKED, &[]);
        assert_eq!(out["verdict"], "complete");
        assert_eq!(out["summary"], "published and checked");
    }

    #[test]
    fn an_unknown_profile_is_unknown_never_complete() {
        let require = requirement("deploy", &BTreeMap::new());
        let out = verdict("deploy", require, &[], &[]);
        assert_eq!(out["verdict"], "unknown");
    }

    #[test]
    fn a_profile_that_requires_nothing_or_one_dimension_twice_is_invalid() {
        let parse = |text: &str| toml::from_str::<ProfileEntry>(text);
        assert!(
            parse("require = [\"runing\"]\n").is_err(),
            "a typo never parses"
        );
        assert!(parse("require = [\"published\"]\nrequires = []\n").is_err());
        let mut profiles = BTreeMap::new();
        profiles.insert("p".to_owned(), parse("require = []\n").unwrap());
        assert!(validate(&profiles).is_err());
        profiles.insert(
            "p".to_owned(),
            parse("require = [\"checked\", \"checked\"]\n").unwrap(),
        );
        assert!(validate(&profiles).is_err());
        profiles.insert("p".to_owned(), parse("require = [\"running\"]\n").unwrap());
        assert!(validate(&profiles).is_ok());
    }
}
