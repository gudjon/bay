//! Completion dimensions and profiles (`DESIGN` § 7; `INTERFACES` § 7; ruling R12).
//!
//! A change is published, checked, running and effective, each established by its own
//! evidence. A profile, `[profile.<name>] require = [...]` in `.bay/policy.toml`, says which of
//! them a change needs; `[policy] profile` in `.bay/config.toml` selects one. `bay status`
//! reports each dimension apart and a profile verdict that never collapses into "done": a
//! required dimension that is missing is named ("merged and checked; runtime confirmation
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
    /// The change is on main; its word is `merged` (0.3.5).
    #[serde(rename = "merged")]
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
            Self::Published => "merged",
            Self::Checked => "checked",
            Self::Running => "running",
            Self::Effective => "effective",
        }
    }

    /// How a summary names this dimension when it is missing.
    const fn missing(self) -> &'static str {
        match self {
            Self::Published => "not merged",
            Self::Checked => "not checked",
            Self::Running => "runtime confirmation missing",
            Self::Effective => "outcome check missing",
        }
    }
}

/// One requirement of a profile: a dimension bay defines, or one the policy declares in a
/// `[[dimension]]` table (G150), such as `reviewed`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Need {
    Builtin(Dimension),
    Site(String),
}

impl Need {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Builtin(dimension) => dimension.as_str(),
            Self::Site(name) => name,
        }
    }

    fn missing(&self) -> String {
        match self {
            Self::Builtin(dimension) => dimension.missing().to_owned(),
            Self::Site(name) => format!("{name} missing"),
        }
    }
}

impl<'de> Deserialize<'de> for Need {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Ok(
            serde_json::from_value::<Dimension>(json!(name))
                .map_or(Self::Site(name), Self::Builtin),
        )
    }
}

/// A `[[dimension]]` table of `.bay/policy.toml` (G150): a completion dimension the site
/// defines, met for a landing by a record of `kind` from one of `producers` about that landing
/// commit. It is reported by `bay status` and may be required by a profile; it never enters the
/// landing verdict.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteDimension {
    pub name: String,
    pub kind: String,
    pub producers: Vec<String>,
}

/// The policy's `[[dimension]]` tables, once each is valid: a name bay does not define, used
/// once, with a kind and at least one producer.
///
/// # Errors
///
/// The first rule a table breaks, naming it.
pub fn site_dimensions(tables: Vec<SiteDimension>) -> Result<Vec<SiteDimension>, String> {
    let mut seen = Vec::new();
    for table in &tables {
        if serde_json::from_value::<Dimension>(json!(table.name)).is_ok() {
            return Err(format!(
                "[[dimension]] {} is a dimension bay defines; a site dimension needs a name of its own",
                table.name
            ));
        }
        if table.name.is_empty() || table.kind.is_empty() || table.producers.is_empty() {
            return Err(format!(
                "[[dimension]] {:?} needs a name, a kind and at least one producer",
                table.name
            ));
        }
        if seen.contains(&table.name) {
            return Err(format!("[[dimension]] {} is declared twice", table.name));
        }
        seen.push(table.name.clone());
    }
    Ok(tables)
}

/// The profile a policy without one of that name means by `default`: today's behaviour, a
/// change is complete once merged and checked; runtime is reported, not required.
pub const DEFAULT_PROFILE: &str = "default";
pub const DEFAULT_REQUIRE: [Dimension; 2] = [Dimension::Published, Dimension::Checked];

/// A `[profile.<name>]` table of `.bay/policy.toml`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileEntry {
    pub require: Vec<Need>,
    /// Whether only signed evidence may establish a dimension (G160). Off by default: unsigned
    /// records count, as unsigned check records do (owner ruling Q6).
    #[serde(default)]
    pub require_signed: bool,
}

/// What one profile asks: the dimensions it requires, and whether only signed evidence counts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub require: Vec<Need>,
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
    declared: &[SiteDimension],
) -> Result<BTreeMap<String, Profile>, String> {
    validate(&profiles)?;
    // A name neither bay nor the policy defines is refused: a typo never requires less.
    for (name, entry) in &profiles {
        for need in &entry.require {
            if let Need::Site(site) = need
                && !declared.iter().any(|table| table.name == *site)
            {
                return Err(format!(
                    "[profile.{name}] requires {site}, which is neither a dimension bay defines nor a [[dimension]] of the policy"
                ));
            }
        }
    }
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
            require: DEFAULT_REQUIRE.into_iter().map(Need::Builtin).collect(),
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
    met: &[Need],
    unsigned: &[Need],
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
    // Bay's dimensions in their own order, then the site's in the profile's order.
    let ordered: Vec<Need> = Dimension::ALL
        .into_iter()
        .map(Need::Builtin)
        .filter(|need| require.contains(need))
        .chain(
            require
                .iter()
                .filter(|need| matches!(need, Need::Site(_)))
                .cloned(),
        )
        .collect();
    let (have, missing): (Vec<Need>, Vec<Need>) =
        ordered.into_iter().partition(|need| met.contains(need));
    let resting: Vec<Need> = have
        .iter()
        .filter(|need| unsigned.contains(need))
        .cloned()
        .collect();
    // Under `require_signed`, a dimension on unsigned records alone is neither met nor missing.
    let (have, unknown): (Vec<Need>, Vec<Need>) = have
        .into_iter()
        .partition(|need| !profile.require_signed || !resting.contains(need));
    let mut parts = Vec::new();
    if !have.is_empty() {
        parts.push(and_list(have.iter().map(Need::as_str)));
    }
    if !unknown.is_empty() {
        parts.push(format!(
            "{} unknown: only unsigned evidence, and [profile.{name}] sets require_signed",
            and_list(unknown.iter().map(Need::as_str))
        ));
    }
    parts.extend(missing.iter().map(Need::missing));
    let word = if !missing.is_empty() {
        "incomplete"
    } else if !unknown.is_empty() {
        "unknown"
    } else {
        "complete"
    };
    json!({
        "name": name,
        "require": require.iter().map(Need::as_str).collect::<Vec<_>>(),
        "require_signed": profile.require_signed,
        "missing": missing.iter().map(Need::as_str).collect::<Vec<_>>(),
        "unsigned": resting.iter().map(Need::as_str).collect::<Vec<_>>(),
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

    use super::{
        Dimension, Need, Profile, ProfileEntry, SiteDimension, requirement, requirements,
        site_dimensions, validate, verdict,
    };

    fn needs(dimensions: &[Dimension]) -> Vec<Need> {
        dimensions.iter().copied().map(Need::Builtin).collect()
    }

    fn published_and_checked() -> Vec<Need> {
        needs(&[Dimension::Published, Dimension::Checked])
    }

    fn reviewed() -> SiteDimension {
        SiteDimension {
            name: "reviewed".to_owned(),
            kind: "review".to_owned(),
            producers: vec!["review-1".to_owned()],
        }
    }

    #[test]
    fn a_missing_runtime_dimension_is_named_never_done() {
        let all = Profile {
            require: needs(&Dimension::ALL),
            require_signed: false,
        };
        let out = verdict("deploy", Ok(all), &published_and_checked(), &[]);
        assert_eq!(out["verdict"], "incomplete");
        assert_eq!(
            out["summary"],
            "merged and checked; runtime confirmation missing; outcome check missing"
        );
        assert_eq!(out["missing"], serde_json::json!(["running", "effective"]));
        assert!(!out.to_string().contains("done"));
    }

    #[test]
    fn the_default_profile_requires_published_and_checked() {
        let require = requirement("default", &BTreeMap::new()).unwrap();
        assert_eq!(require.require, published_and_checked());
        assert!(!require.require_signed);
        let out = verdict("default", Ok(require), &published_and_checked(), &[]);
        assert_eq!(out["verdict"], "complete");
        assert_eq!(out["summary"], "merged and checked");
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
        let typo: BTreeMap<String, ProfileEntry> =
            [("p".to_owned(), parse("require = [\"runing\"]\n").unwrap())].into();
        assert!(
            requirements(typo, &[]).is_err(),
            "a name neither bay nor the policy defines is refused: a typo never requires less"
        );
        assert!(parse("require = [\"merged\"]\nrequires = []\n").is_err());
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

    /// G150: a site dimension the policy declares is required by name, named when missing, and
    /// a dimension bay defines is no site name.
    #[test]
    fn a_site_dimension_is_required_by_name_and_named_when_missing() {
        let parse = |text: &str| toml::from_str::<ProfileEntry>(text).unwrap();
        let release: BTreeMap<String, ProfileEntry> = [(
            "release".to_owned(),
            parse("require = [\"merged\", \"checked\", \"reviewed\"]\n"),
        )]
        .into();
        let profiles = requirements(release, &[reviewed()]).unwrap();
        let profile = requirement("release", &profiles).unwrap();
        let out = verdict(
            "release",
            Ok(profile.clone()),
            &published_and_checked(),
            &[],
        );
        assert_eq!(out["verdict"], "incomplete");
        assert_eq!(out["summary"], "merged and checked; reviewed missing");
        assert_eq!(out["missing"], serde_json::json!(["reviewed"]));
        let mut met = published_and_checked();
        met.push(Need::Site("reviewed".to_owned()));
        assert_eq!(
            verdict("release", Ok(profile), &met, &[])["verdict"],
            "complete"
        );

        let mut shadow = reviewed();
        shadow.name = "checked".to_owned();
        assert!(site_dimensions(vec![shadow]).is_err());
        assert!(site_dimensions(vec![reviewed(), reviewed()]).is_err());
    }
}
