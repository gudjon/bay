//! `bay update`: the binary's lifecycle (`DESIGN`, the § on `bay update`; `INTERFACES` § 2).
//!
//! The pin and the auto switch are the box's, never a repository's: global Git config
//! `bay.updateVersion` and `bay.updateAuto` (owner ruling 2026-10-18), or `--version` for one
//! run. A process's `BAY_UPDATE_VERSION` overrides the pin for that process, so one agent on a
//! shared login can choose its own. A cloned repository never moves the box.
//!
//! A pin is the oldest version the box runs, never a ceiling (a fleet finding, 2026-10-26: a pin
//! left behind held a box on an old release for days, and runners stayed on a release with a
//! known fault after its fix was out). The box follows every newer release; a build that was
//! never released never moves it ([`pin_allows`]).
//!
//! bay is 0.x, so every release is a pre-release, offered best effort, and every release is a
//! candidate: there are no update channels. Resolves a target (`--version`, else the
//! newest release or the pin when the pin is newer, else the newest release that has a build for this platform, skipping back past
//! a newer one that has none), downloads the release beside the installed binary
//! (`$HOME/.local/bin/bay`, bay's one install path, whichever copy runs), verifies it all or
//! nothing, unpacks only `bay` from the bytes it hashed, keeps it as that version's own file
//! (`$XDG_DATA_HOME/bay/versions/<version>/bay`), and swaps it in with one `rename(2)` (on
//! Windows, where a running `bay.exe` cannot be replaced, it moves aside to `bay.old.exe`
//! first). A miss anywhere leaves the old binary byte-identical (G50).
//!
//! An update never waits for a running executor (owner ruling 2026-10-20). The install path
//! stays a regular file that the rename replaces, so a version's kept file can be pruned without
//! breaking it; the kept file is a hard link where the file system allows one. A process starts
//! every child `bay` from its own version's file ([`own_binary`]), the updater keeps the version
//! it replaces, and pruning keeps the newest three, the running one and every one a live
//! executor names. The executor moves to the new version between operations (`crate::executor`).
//!
//! A release is `bay-<target>.tar.gz` per platform (`.zip` for a Windows target), `VERSION`,
//! `SHA256SUMS` over both, and `SHA256SUMS.sig`: an SSH signature by one of the release keys,
//! principal and namespace `bay-release`. The signed sums cover `VERSION`, so an older signed release cannot stand in
//! for the target.

use std::cmp::Ordering;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::git;
use crate::result::{BayResult, Decision, Reason};
use crate::version::BAY;

/// The public halves of the bay release keys. `SHA256SUMS.sig` must verify against one of them
/// (G151): a rollover lists the old and the new key for a stated transition, and a later
/// release drops the old one. A key may be a hardware key (`sk-ssh-ed25519@openssh.com`, or
/// `sk-ecdsa-sha2-nistp256@openssh.com` for one held in a secure enclave).
/// `scripts/release/allowed_signers` lists the same keys.
pub const RELEASE_KEYS: &[&str] = &[
    // The release key 0.1.1-0.1.4 are signed with; dropped by a later release.
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPnvMnsFVUR3lBcKXj2PkjHH3mIDFHKl2/Ov/lgVHMM7",
    // The 0.x pre-release key, kept on the build box and signing each 0.x release there
    // (owner ruling 2026-10-26); dropped before 1.0.
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICks1fBuvAPoSH96LnFu0M5ZSbaO6FQpAn2xITWLpuYL",
    // The hardware-held release key (SHA256:esVZKU2sltI199RAoWUorTk/Bw6t02pPUfHnNy3OFj0).
    "sk-ecdsa-sha2-nistp256@openssh.com AAAAInNrLWVjZHNhLXNoYTItbmlzdHAyNTZAb3BlbnNzaC5jb20AAAAIbmlzdHAyNTYAAABBBKhWxaQcv6q6F/FeEpLS8mDAnee+kArl4ZD9eoNC96RnbV+BxgA+VPEaaTgnguJTxEc5gta4F4h4v3h/sfrT00UAAAAEc3NoOg==",
];
const PRINCIPAL: &str = "bay-release";
const RELEASES: &str = "https://github.com/gudjon/bay/releases";
const RELEASE_LIST: &str = "https://api.github.com/repos/gudjon/bay/releases?per_page=100&page=";
/// A bound on the pages read, so a list that never ends cannot hold the command.
const MAX_PAGES: usize = 100;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_mins(5);

/// What `bay update` was asked for.
#[derive(Debug, Default)]
pub struct Request {
    pub version: Option<String>,
    pub check: bool,
}

/// Where releases come from, and the keys they must verify against.
struct Source {
    /// The list's URL up to its page number, and what follows the number.
    list: (String, String),
    download: String,
    keys: Vec<String>,
    https_only: bool,
}

impl Source {
    /// The public releases. A build that `scripts/release.sh` did not make (`BAY_RELEASE_VERSION`
    /// unset) also honours `BAY_TEST_RELEASE_URL` and `BAY_TEST_RELEASE_KEY` (one key per line),
    /// for G50's fixture; a release build never does.
    fn current() -> Self {
        let test = option_env!("BAY_RELEASE_VERSION").is_none();
        let keys = std::env::var("BAY_TEST_RELEASE_KEY")
            .ok()
            .filter(|_| test)
            .map_or_else(
                || RELEASE_KEYS.iter().map(|key| (*key).to_owned()).collect(),
                |keys| key_lines(&keys),
            );
        match std::env::var("BAY_TEST_RELEASE_URL").ok().filter(|_| test) {
            Some(base) => Self {
                list: (format!("{base}/releases-"), ".json".to_owned()),
                download: format!("{base}/download"),
                keys,
                https_only: false,
            },
            None => Self {
                list: (RELEASE_LIST.to_owned(), String::new()),
                download: format!("{RELEASES}/download"),
                keys,
                https_only: true,
            },
        }
    }

    fn fetch(&self, url: &str, out: &Path) -> Result<(), String> {
        let mut cmd = Command::new("curl");
        cmd.args(["-fsSL", "--max-time", "300"]);
        if self.https_only {
            cmd.args(["--proto", "=https", "--tlsv1.2"]);
        }
        cmd.arg("-o").arg(out).arg(url);
        let output = git::run_bounded_for(cmd, "curl", None, DOWNLOAD_TIMEOUT)
            .map_err(|error| error.message())?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "cannot download {url}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }
}

/// The keys of a test key list, one per line; blank lines are skipped.
fn key_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The allowed-signers file for `keys`: one line per key, each under the principal
/// `bay-release`, so a release signed by any listed key verifies (G151).
#[must_use]
pub fn allowed_signers(keys: &[String]) -> String {
    keys.iter().fold(String::new(), |mut file, key| {
        let _ = writeln!(file, "{PRINCIPAL} {key}");
        file
    })
}

/// This platform's release target, as `scripts/release.sh` names its archives. Windows has a
/// target name, so an update there looks for its archive and, while no release has one, is
/// refused as `UPDATE_UNAVAILABLE` naming the target.
#[must_use]
pub fn platform() -> Option<String> {
    let os = match std::env::consts::OS {
        "linux" => "unknown-linux-musl",
        "macos" => "apple-darwin",
        "windows" => "pc-windows-msvc",
        _ => return None,
    };
    Some(format!("{}-{os}", std::env::consts::ARCH))
}

/// The release archive of `platform`: `bay-<target>.zip` for a Windows target (plan W3), else
/// `bay-<target>.tar.gz`.
#[must_use]
pub fn archive_name(platform: &str) -> String {
    if platform.ends_with("-windows-msvc") {
        format!("bay-{platform}.zip")
    } else {
        format!("bay-{platform}.tar.gz")
    }
}

/// The name of bay's binary on this platform, in an archive and on disk: `bay`, or `bay.exe`
/// on Windows.
fn binary_name() -> String {
    format!("bay{}", std::env::consts::EXE_SUFFIX)
}

#[must_use]
pub fn update(request: &Request) -> BayResult {
    run(request).unwrap_or_else(|refusal| *refusal)
}

/// Where a box setting came from: a process's environment wins over global Git config.
pub const FROM_ENV: &str = "env";
pub const FROM_GIT_CONFIG: &str = "git config";

/// The environment variable that sets the pin for one process (and its children), over global
/// Git config: on a shared login every agent shares that config, so its supervisor sets its
/// choice here.
pub const VERSION_VAR: &str = "BAY_UPDATE_VERSION";
/// The update channel's old variable and key. They are no longer read; doctor names one that is
/// still set, so a setting is never ignored silently.
pub const OLD_CHANNEL_VAR: &str = "BAY_UPDATE_CHANNEL";
pub const OLD_CHANNEL_KEY: &str = "bay.updateChannel";

/// This box's update settings: the process's `BAY_UPDATE_VERSION`, else global Git config. A
/// repository's config is never read.
#[derive(Debug, Default)]
pub struct BoxSettings {
    pub version: Option<String>,
    /// `bay.updateAuto`; unset is on.
    pub auto: bool,
    /// Where `version` came from ([`FROM_ENV`] or [`FROM_GIT_CONFIG`]), when set.
    pub version_from: Option<&'static str>,
    /// The old channel variable or key that is still set, if any; it is not read.
    pub channel_ignored: Option<String>,
}

impl BoxSettings {
    /// # Errors
    ///
    /// An environment pin that is not a version, naming the variable: a set variable is never
    /// silently ignored.
    pub fn load() -> Result<Self, String> {
        let get = |args: &[&str]| {
            let mut cmd = Command::new("git");
            cmd.args(["config", "--global"]).args(args);
            git::run_bounded(cmd, "git config --global")
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let env = |name: &str| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(format!("{name} is not valid UTF-8")),
        };
        let env_version = env(VERSION_VAR)?;
        if let Some(version) = &env_version
            && !valid(version)
        {
            return Err(format!(
                "{VERSION_VAR} must be an exact version (X.Y.Z or X.Y.Z-suffix), not {version:?}"
            ));
        }
        let (version, version_from) = env_version.map_or_else(
            || {
                let value = get(&["--get", "bay.updateVersion"]);
                let from = value.as_ref().map(|_| FROM_GIT_CONFIG);
                (value, from)
            },
            |value| (Some(value), Some(FROM_ENV)),
        );
        let ignored: Vec<&str> = [
            std::env::var_os(OLD_CHANNEL_VAR).map(|_| OLD_CHANNEL_VAR),
            get(&["--get", OLD_CHANNEL_KEY]).map(|_| OLD_CHANNEL_KEY),
        ]
        .into_iter()
        .flatten()
        .collect();
        Ok(Self {
            version,
            auto: get(&["--type=bool", "--get", "bay.updateAuto"]).is_none_or(|on| on != "false"),
            version_from,
            channel_ignored: (!ignored.is_empty()).then(|| ignored.join(" and ")),
        })
    }

    /// How the pin is named in a refusal: the variable or the config key it came from.
    pub(crate) fn pin_name(&self) -> &'static str {
        if self.version_from == Some(FROM_ENV) {
            VERSION_VAR
        } else {
            "git config --global bay.updateVersion"
        }
    }
}

/// A result that ends the command early.
type Refusal = Box<BayResult>;

fn failed(message: impl Into<String>) -> Refusal {
    Box::new(BayResult::failed(message))
}

/// The resolved target and the fields every result carries.
struct Plan {
    target: String,
    pin: Option<String>,
    /// The newest release, when this plan steps to an older one first (G242).
    newest: Option<String>,
    /// Older releases, newest first and all newer than the installed one, that an unpinned
    /// update may step to when this binary cannot verify the target's signature (G242).
    steps: Vec<String>,
}

impl Plan {
    fn detail(&self, updated: bool) -> Value {
        let mut detail = json!({"installed": BAY, "target": self.target, "pinned": self.pin,
            "updated": updated});
        if let Some(newest) = self
            .newest
            .as_ref()
            .filter(|newest| **newest != self.target)
        {
            detail["newest"] = json!(newest);
            detail["stepped"] = json!(true);
            detail["next"] = json!(format!(
                "{} trusts the key {newest} is signed with: run bay update again to reach it",
                self.target
            ));
        }
        detail
    }

    /// The plan that installs `version` on the way to this plan's target.
    fn step_to(&self, version: &str) -> Self {
        Self {
            target: version.to_owned(),
            pin: None,
            newest: Some(self.target.clone()),
            steps: Vec::new(),
        }
    }

    fn refuse(&self, decision: Decision, reason: Reason, why: impl Into<String>) -> Refusal {
        let mut detail = self.detail(false);
        detail["why"] = json!(why.into());
        Box::new(BayResult::refused(decision, reason).with_detail(detail))
    }

    fn unverified(&self, why: impl Into<String>) -> Refusal {
        self.refuse(Decision::Failed, Reason::UpdateUnverified, why)
    }

    fn unavailable(&self, why: impl Into<String>) -> Refusal {
        self.refuse(Decision::Blocked, Reason::UpdateUnavailable, why)
    }
}

fn run(request: &Request) -> Result<BayResult, Refusal> {
    let settings = BoxSettings::load().map_err(failed)?;
    let pin_name = settings.pin_name();
    let pin = settings.version;
    for version in [&pin, &request.version].into_iter().flatten() {
        if !valid(version) {
            return Err(failed(format!(
                "{version} is not a version (X.Y.Z or X.Y.Z-suffix)"
            )));
        }
    }
    let source = Source::current();
    let plan = resolve(&source, pin, request.version.clone())?;

    if let (Some(pin), Some(version)) = (&plan.pin, &request.version)
        && compare(version, pin) == Ordering::Less
    {
        return Err(plan.refuse(
            Decision::Blocked,
            Reason::UpdatePinned,
            format!(
                "this box runs bay {pin} or newer ({pin_name}); {version} is older, and only a lower pin moves it back"
            ),
        ));
    }
    match compare(&plan.target, BAY) {
        Ordering::Equal => {
            let mut detail = plan.detail(false);
            if request.check {
                detail["due"] = json!(false);
            }
            return Ok(BayResult::ok(detail));
        }
        Ordering::Less if plan.pin.as_ref() != Some(&plan.target) => {
            return Err(plan.refuse(
                Decision::Blocked,
                Reason::UpdatePinned,
                format!(
                    "{} is older than the installed {BAY}; only a pin moves a box back",
                    plan.target
                ),
            ));
        }
        _ => {}
    }
    if request.check {
        let mut detail = plan.detail(false);
        detail["due"] = json!(true);
        return Ok(BayResult::ok(detail));
    }
    // Never waits for a running executor (owner ruling 2026-10-20): the release goes to its own
    // file under `versions/`, an executor's operation starts its children from its own
    // version's file there, and the executor moves to the new version between operations.
    let installed = installed_path().map_err(failed)?;
    let bin_dir = installed
        .parent()
        .ok_or_else(|| failed("the install path has no directory"))?;
    std::fs::create_dir_all(bin_dir)
        .map_err(|error| failed(format!("cannot create {}: {error}", bin_dir.display())))?;
    // Beside the binary, so the swap is a rename within one filesystem.
    let tmp = TempDir::new(bin_dir).map_err(failed)?;
    match download_verified(&source, &plan, &tmp.0) {
        Ok(archive) => swap(&plan, &tmp.0, &archive, &installed),
        Err(refusal) if unsigned_for_us(&refusal) => {
            // G242: the newest release is signed by a key this binary does not trust yet, as
            // after a release-key rollover. Step to the newest release that verifies and is
            // newer than this one; it trusts the new key, and the next update reaches the
            // newest. Only a signature this binary cannot verify steps: a release whose bytes
            // do not match its signed sums is refused.
            for version in &plan.steps {
                let step = plan.step_to(version);
                let tmp = TempDir::new(bin_dir).map_err(failed)?;
                match download_verified(&source, &step, &tmp.0) {
                    Ok(archive) => return swap(&step, &tmp.0, &archive, &installed),
                    Err(next) if unsigned_for_us(&next) => {}
                    Err(next) => return Err(next),
                }
            }
            Err(refusal)
        }
        Err(refusal) => Err(refusal),
    }
}

/// Why a release's `SHA256SUMS.sig` fails: it verifies against no key this binary trusts.
const NOT_OUR_KEY: &str = "SHA256SUMS does not verify against a bay release key";

/// Whether `refusal` is [`NOT_OUR_KEY`], the one refusal an update steps past (G242).
fn unsigned_for_us(refusal: &Refusal) -> bool {
    refusal.reason == Some(Reason::UpdateUnverified)
        && refusal
            .detail
            .as_ref()
            .is_some_and(|detail| detail["why"] == NOT_OUR_KEY)
}

/// `--version` wins, then the newest release that has a build for this platform, or the pin
/// when no release is as new as it. A release list that cannot be read leaves a pinned box on
/// its pin. Every release is a candidate: whether GitHub marks it a pre-release plays no part.
fn resolve(source: &Source, pin: Option<String>, version: Option<String>) -> Result<Plan, Refusal> {
    let unavailable = |why: String| {
        Box::new(
            BayResult::blocked(Reason::UpdateUnavailable)
                .with_detail(json!({"installed": BAY, "why": why})),
        )
    };
    let on_pin = |pin: &Option<String>| {
        pin.clone().map(|target| Plan {
            target,
            pin: pin.clone(),
            newest: None,
            steps: Vec::new(),
        })
    };
    if let Some(target) = version {
        return Ok(Plan {
            target,
            pin,
            newest: None,
            steps: Vec::new(),
        });
    }
    let listed = list_newest(source);
    let (newest, steps) = match listed {
        Ok(listed) => listed,
        Err(why) => return on_pin(&pin).ok_or_else(|| unavailable(why)),
    };
    if let Some(floor) = &pin
        && compare(floor, &newest) == Ordering::Greater
    {
        return Ok(on_pin(&pin).expect("a pin"));
    }
    Ok(Plan {
        target: newest,
        pin,
        newest: None,
        steps,
    })
}

/// The newest release that has a build for this platform, and the older ones, newest first,
/// that are still newer than this bay (G242's steps).
fn list_newest(source: &Source) -> Result<(String, Vec<String>), String> {
    let platform = platform().ok_or_else(|| "no release build for this platform".to_owned())?;
    let archive = archive_name(&platform);
    let built = |release: &Release| release.assets.contains(&archive);
    // Read until a built release no newer than this one, so every release an update may step
    // through is listed (G242); on GitHub that is almost always the first page of 100.
    let releases = release_list(source, |releases| {
        releases
            .iter()
            .any(|release| built(release) && compare(&release.version, BAY) != Ordering::Greater)
    })?;
    let mut versions: Vec<String> = releases
        .into_iter()
        .filter(built)
        .map(|release| release.version)
        .collect();
    versions.sort_by(|a, b| compare(b, a));
    State::record(versions.first().map(String::as_str));
    let mut versions = versions.into_iter();
    let newest = versions
        .next()
        .ok_or_else(|| format!("no release has a build for {platform}"))?;
    let steps = versions
        .filter(|version| compare(version, BAY) == Ordering::Greater)
        .collect();
    Ok((newest, steps))
}

/// Whether a box whose pin is `pin` runs bay `version`: the pin itself, or a newer version that
/// is the newest release the last check recorded. A build that was never released is never the
/// newest release, so it never moves a pinned box.
#[must_use]
pub fn pin_allows(pin: &str, version: &str) -> bool {
    version == pin
        || compare(version, pin) == Ordering::Greater
            && State::read().and_then(|state| state.newest).as_deref() == Some(version)
}

/// A published release: its version and the names of its assets.
struct Release {
    version: String,
    assets: Vec<String>,
}

/// The published releases, read page by page until `enough` holds for what was read or the
/// list ends; drafts and tags that are not versions are skipped.
fn release_list(
    source: &Source,
    enough: impl Fn(&[Release]) -> bool,
) -> Result<Vec<Release>, String> {
    let dir = TempDir::new(&std::env::temp_dir())?;
    let file = dir.0.join("releases.json");
    let mut releases = Vec::new();
    for page in 1..=MAX_PAGES {
        let (prefix, suffix) = &source.list;
        source.fetch(&format!("{prefix}{page}{suffix}"), &file)?;
        let text = std::fs::read_to_string(&file).map_err(|error| error.to_string())?;
        let list: Vec<Value> =
            serde_json::from_str(&text).map_err(|error| format!("the release list: {error}"))?;
        if list.is_empty() {
            break;
        }
        releases.extend(
            list.iter()
                .filter(|release| release["draft"] != true)
                .filter_map(|release| {
                    let version = release["tag_name"].as_str()?.trim_start_matches('v');
                    let assets = release["assets"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|asset| asset["name"].as_str().map(str::to_owned))
                        .collect();
                    valid(version).then(|| Release {
                        version: version.to_owned(),
                        assets,
                    })
                }),
        );
        if enough(&releases) {
            break;
        }
    }
    Ok(releases)
}

/// bay's one install path, `$HOME/.local/bin/bay` (`DESIGN` § 15.1; `bay.exe` under
/// `USERPROFILE` on Windows). An update writes only this path, whichever copy of bay runs it.
fn installed_path() -> Result<PathBuf, String> {
    let home = crate::platform::home().ok_or("HOME is not set; update cannot name its path")?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("bin")
        .join(binary_name()))
}

/// Steps 3–4: downloads the release into `tmp` and verifies it, all or nothing. Returns the
/// archive's bytes, whose digest the signed sums list; only these bytes are unpacked.
fn download_verified(source: &Source, plan: &Plan, tmp: &Path) -> Result<Vec<u8>, Refusal> {
    let platform =
        platform().ok_or_else(|| plan.unavailable("no release build for this platform"))?;
    let archive = archive_name(&platform);
    let base = format!("{}/v{}", source.download, plan.target);
    let get = |name: &str| {
        let path = tmp.join(name);
        source
            .fetch(&format!("{base}/{name}"), &path)
            .map(|()| path)
    };
    let read =
        |path: &Path| std::fs::read(path).map_err(|error| plan.unverified(error.to_string()));

    let sums = read(&get("SHA256SUMS").map_err(|why| plan.unavailable(why))?)?;
    let signature = read(&get("SHA256SUMS.sig").map_err(|why| plan.unverified(why))?)?;
    let version = read(&get("VERSION").map_err(|why| plan.unverified(why))?)?;
    let allowed = tmp.join("allowed_signers");
    std::fs::write(&allowed, allowed_signers(&source.keys))
        .map_err(|error| failed(error.to_string()))?;
    let signature = String::from_utf8_lossy(&signature);
    if !crate::signing::verify_data(&allowed, PRINCIPAL, PRINCIPAL, &sums, &signature) {
        return Err(plan.unverified(NOT_OUR_KEY));
    }
    let sums = String::from_utf8_lossy(&sums);
    if sum_of(&sums, "VERSION") != Some(sha256(&version)) {
        return Err(plan.unverified("VERSION does not match the signed SHA256SUMS"));
    }
    let said = String::from_utf8_lossy(&version).trim().to_owned();
    if said != plan.target {
        return Err(plan.unverified(format!(
            "the signed release says {said}, not {}",
            plan.target
        )));
    }
    let Some(want) = sum_of(&sums, &archive) else {
        return Err(plan.unavailable(format!("the release has no {archive}")));
    };
    let bytes = read(&get(&archive).map_err(|why| plan.unavailable(why))?)?;
    if sha256(&bytes) != want {
        return Err(plan.unverified(format!("{archive} does not match the signed SHA256SUMS")));
    }
    Ok(bytes)
}

/// Step 5: unpack only `bay` from the verified bytes, run the new binary's `--version`, keep it
/// as its version's file, `rename(2)` it over the installed path, then run its `install`. On
/// Windows the archive is a zip, unpacked by the system's `tar` from a file, and the running
/// `bay.exe` moves aside first (`crate::durable::replace_program`).
fn swap(plan: &Plan, tmp: &Path, archive: &[u8], installed: &Path) -> Result<BayResult, Refusal> {
    let unpacked = tmp.join("x");
    std::fs::create_dir_all(&unpacked).map_err(|error| failed(error.to_string()))?;
    let name = binary_name();
    let output = if cfg!(windows) {
        // A zip is read from a file, where its directory at the end can be found.
        let zip = tmp.join("archive.zip");
        std::fs::write(&zip, archive).map_err(|error| failed(error.to_string()))?;
        // The system's own `tar` reads a zip; a GNU `tar` earlier on `PATH` does not.
        let system = std::env::var_os("SystemRoot")
            .map(|root| PathBuf::from(root).join("System32").join("tar.exe"))
            .filter(|tar| tar.is_file());
        let mut tar = Command::new(system.unwrap_or_else(|| PathBuf::from("tar")));
        tar.arg("-xf").arg(&zip).arg("-C").arg(&unpacked).arg(&name);
        git::run_bounded(tar, "tar")
    } else {
        let mut tar = Command::new("tar");
        tar.args(["-xzf", "-", "--no-same-owner", "-C"])
            .arg(&unpacked)
            .arg(&name);
        git::run_bounded_with_input(tar, "tar", archive)
    }
    .map_err(|error| failed(error.message()))?;
    let new = unpacked.join(&name);
    // A link or anything else that is not a regular file is refused, never followed.
    let regular = std::fs::symlink_metadata(&new).is_ok_and(|meta| meta.file_type().is_file());
    if !output.status.success() || !regular {
        return Err(plan.unverified(format!("the archive holds no regular file named {name}")));
    }
    set_executable(&new).map_err(failed)?;
    let mut probe = Command::new(&new);
    probe.arg("--version");
    let printed = git::run_bounded(probe, "bay --version")
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    if printed.split_whitespace().nth(1) != Some(plan.target.as_str()) {
        return Err(plan.unverified(format!(
            "the new binary prints {:?}, not bay {}",
            printed.trim(),
            plan.target
        )));
    }
    // The version this update runs keeps its own file when it is the one being replaced, so
    // a process it came from still starts its children from it.
    if std::env::current_exe().is_ok_and(|running| same_path(&running, installed)) {
        let _ = pin(installed);
    }
    let versioned = version_path(&plan.target)
        .ok_or_else(|| failed("neither XDG_DATA_HOME nor HOME is set"))?;
    place(&new, &versioned).map_err(failed)?;
    crate::durable::replace_program(&new, installed)
        .map_err(|error| failed(format!("cannot replace {}: {error}", installed.display())))?;
    prune(&[plan.target.as_str(), BAY]);

    // The new binary re-copies what belongs to its version (G49), for the box only: an
    // update never writes a repository's agent settings or Git config. A failure is recorded
    // in the state file, so doctor shows it after a background update.
    let mut install = Command::new(installed);
    install
        .args(["--json", "install", "--machine"])
        .current_dir(installed.parent().unwrap_or(installed));
    let mut detail = plan.detail(true);
    let error = match git::run_bounded(install, "bay install") {
        Ok(output) if output.status.success() => None,
        Ok(output) => {
            detail["install"] = serde_json::from_slice(&output.stdout)
                .unwrap_or_else(|_| json!(String::from_utf8_lossy(&output.stderr)));
            Some(format!("bay install exited {}", output.status))
        }
        Err(error) => Some(error.message()),
    };
    State::install_error(error.clone());
    match error {
        None => Ok(BayResult::ok(detail)),
        Some(error) => {
            detail["why"] = json!(format!("the binary is updated; run bay install ({error})"));
            Ok(BayResult::new(Decision::Failed).with_detail(detail))
        }
    }
}

/// What the last check saw, in `$XDG_STATE_HOME/bay/update.json` (else
/// `~/.local/state/bay/update.json`): when it ran, and the newest release with a build for
/// this platform. Unknown fields are ignored, so a file that still holds the old `channel`
/// reads.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Seconds since the Unix epoch.
    pub checked_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest: Option<String>,
    /// Why the new binary's `install` failed after the last update, until one succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_error: Option<String>,
}

/// How often a box asks for a new release: hourly, so a fix reaches the fleet the hour it is
/// out (a fleet finding, 2026-10-26: a daily check left boxes a day behind).
const CHECK_EVERY_S: u64 = 3600;

impl State {
    fn path() -> Option<PathBuf> {
        Some(state_dir()?.join("update.json"))
    }

    /// The recorded state; nothing recorded, or a file that does not parse, is `None`.
    #[must_use]
    pub fn read() -> Option<Self> {
        let text = std::fs::read_to_string(Self::path()?).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Writes the state through a temporary file and a rename, so a reader never sees half.
    fn write(&self) -> Result<(), String> {
        let path = Self::path().ok_or("neither XDG_STATE_HOME nor HOME is set")?;
        let dir = path.parent().ok_or("the state path has no directory")?;
        std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
        let tmp = dir.join(format!("update.json.{}", std::process::id()));
        let text = serde_json::to_string(self).map_err(|error| error.to_string())?;
        std::fs::write(&tmp, text).map_err(|error| error.to_string())?;
        crate::durable::rename(&tmp, &path).map_err(|error| error.to_string())
    }

    /// A check that found `newest`.
    fn record(newest: Option<&str>) {
        let mut state = Self::read().unwrap_or_default();
        state.checked_at = now_s();
        state.newest = newest.map(str::to_owned);
        let _ = state.write();
    }

    fn install_error(error: Option<String>) {
        let mut state = Self::read().unwrap_or_default();
        state.install_error = error;
        let _ = state.write();
    }

    /// Claims the check that is due after the one recorded at `seen`: the claim file is created
    /// with `O_EXCL`, so of all the commands that read the same state, one wins.
    fn claim(seen: u64) -> bool {
        let Some(dir) = state_dir() else {
            return false;
        };
        if std::fs::create_dir_all(&dir).is_err() {
            return false;
        }
        let name = format!("update.claim.{seen}");
        let won = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(&name))
            .is_ok();
        if won && let Ok(entries) = std::fs::read_dir(&dir) {
            // Older claims belong to checks long done.
            for entry in entries.flatten() {
                let other = entry.file_name().to_string_lossy().into_owned();
                if other.starts_with("update.claim.") && other != name {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        won
    }
}

/// `$XDG_STATE_HOME/bay`, else `~/.local/state/bay`: this box's bay state.
#[must_use]
pub fn state_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| crate::platform::home().map(|home| PathBuf::from(home).join(".local/state")))?;
    Some(base.join("bay"))
}

/// `$XDG_DATA_HOME/bay`, else `~/.local/share/bay`: this box's bay data.
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| crate::platform::home().map(|home| PathBuf::from(home).join(".local/share")))?;
    Some(base.join("bay"))
}

/// How many versions `versions/` keeps, newest first, besides the running one and every one a
/// live executor names.
const KEEP_VERSIONS: usize = 3;

/// Where `version`'s binary is kept: `<data>/versions/<version>/bay` (owner ruling
/// 2026-10-20). A file there is placed whole by a rename and never written in place, so a
/// process that starts it starts the version it names for as long as it is kept.
#[must_use]
pub fn version_path(version: &str) -> Option<PathBuf> {
    Some(
        data_dir()?
            .join("versions")
            .join(version)
            .join(binary_name()),
    )
}

static OWN: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

/// The file every child `bay` this process starts runs: this version's own, so a child runs
/// the version its parent runs even after an update replaced the install path. The first call,
/// in `main` at start, resolves it: this version's kept file when it is this binary's
/// length, else the running file. `bay runner run` pins it first ([`pin_own_binary`]). A kept
/// file pruned since falls back to the running file.
#[must_use]
pub fn own_binary() -> Option<PathBuf> {
    OWN.get_or_init(resolve_own)
        .clone()
        .filter(|path| path.exists())
        .or_else(|| std::env::current_exe().ok())
}

fn resolve_own() -> Option<PathBuf> {
    let running = std::env::current_exe().ok()?;
    let len = |path: &Path| std::fs::metadata(path).ok().map(|meta| meta.len());
    // A running file with no length was replaced already (Linux names it "<path> (deleted)").
    if let Some(kept) = version_path(BAY)
        && let Some(kept_len) = len(&kept)
        && len(&running).is_none_or(|running_len| running_len == kept_len)
    {
        return Some(kept);
    }
    Some(running)
}

/// Keeps the running binary as this version's file under `versions/` and makes that file
/// [`own_binary`], for the process whose children must start the version it began with: `bay
/// executor run`, whose operation may outlast an update. Where nothing can be kept, children
/// start the running file. Call it before anything resolves [`own_binary`].
pub fn pin_own_binary() {
    let pinned = std::env::current_exe()
        .ok()
        .and_then(|running| pin(&running).ok());
    let _ = OWN.set(pinned.or_else(resolve_own));
}

/// Keeps `running`, this version's binary, as [`version_path`] of this version: a file there
/// that holds the same bytes stays; any other is replaced whole.
fn pin(running: &Path) -> Result<PathBuf, String> {
    let kept = version_path(BAY).ok_or("neither XDG_DATA_HOME nor HOME is set")?;
    if !same_bytes(running, &kept) {
        place(running, &kept)?;
    }
    Ok(kept)
}

/// Places a copy of `from` at `to` by a temporary name beside `to` and a rename: a hard link
/// where the file system allows one (no second copy of the bytes; neither name is ever written
/// in place), else a copy.
fn place(from: &Path, to: &Path) -> Result<(), String> {
    let dir = to.parent().ok_or("the version path has no directory")?;
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let tmp = dir.join(format!(".bay.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    if std::fs::hard_link(from, &tmp).is_err() {
        std::fs::copy(from, &tmp)
            .map_err(|error| format!("cannot write {}: {error}", tmp.display()))?;
        set_executable(&tmp)?;
    }
    crate::durable::rename(&tmp, to).map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot place {}: {error}", to.display())
    })
}

/// Whether `a` and `b` hold the same bytes: one file, or two of one length and content.
fn same_bytes(a: &Path, b: &Path) -> bool {
    let (Ok(first), Ok(second)) = (std::fs::metadata(a), std::fs::metadata(b)) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if (first.dev(), first.ino()) == (second.dev(), second.ino()) {
            return true;
        }
    }
    first.len() == second.len()
        && matches!((std::fs::read(a), std::fs::read(b)), (Ok(x), Ok(y)) if x == y)
}

/// Whether `a` and `b` name one file.
fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Removes kept versions past the newest [`KEEP_VERSIONS`], never one of `keep` (the one just
/// installed and the running one) nor one a live executor on this box names: its operation's
/// children start that version's file. Best effort: a version that cannot be removed stays.
fn prune(keep: &[&str]) {
    let Some(root) = data_dir().map(|dir| dir.join("versions")) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    let mut versions: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| valid(name))
        .collect();
    versions.sort_by(|a, b| compare(b, a));
    let live = crate::executor::live_versions();
    for (index, version) in versions.iter().enumerate() {
        if index < KEEP_VERSIONS || keep.contains(&version.as_str()) || live.contains(version) {
            continue;
        }
        let _ = std::fs::remove_dir_all(root.join(version));
    }
}

fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Auto-update, from `status` and `doctor` only (never `land`, `executor run` or the hook,
/// which hold or may hold an operation): at most one check a day, run as a detached
/// `bay update` so the command's result and exit code never change. `BAY_DISABLE_AUTOUPDATE=1`
/// and the box's `bay.updateAuto = false` turn it off, and a pinned box never moves on its own;
/// a repository's settings play no part. A build
/// `scripts/release.sh` did not make checks only against a test release
/// (`BAY_TEST_RELEASE_URL`), so a development binary never replaces itself.
pub fn auto() {
    if std::env::var("BAY_DISABLE_AUTOUPDATE").is_ok_and(|value| value == "1") {
        return;
    }
    if option_env!("BAY_RELEASE_VERSION").is_none()
        && std::env::var_os("BAY_TEST_RELEASE_URL").is_none()
    {
        return;
    }
    let mut state = State::read().unwrap_or_default();
    let now = now_s();
    // A check recorded in the future (a clock set back) does not hold off the next one.
    if state.checked_at <= now && now - state.checked_at < CHECK_EVERY_S {
        return;
    }
    // An invalid environment value starts no check; the next `bay update` names it.
    let Ok(settings) = BoxSettings::load() else {
        return;
    };
    if !settings.auto {
        return;
    }
    // Claimed and recorded before the check starts, so concurrent commands start at most one.
    if !State::claim(state.checked_at) {
        return;
    }
    state.checked_at = now;
    if state.write().is_err() {
        return;
    }
    let Some(bay) = own_binary() else {
        return;
    };
    // No repository: the update and its install are the box's alone.
    let mut cmd = Command::new(bay);
    cmd.args(["--json", "update"])
        .current_dir(state_dir().unwrap_or_else(std::env::temp_dir))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::platform::detach(&mut cmd);
    let _ = cmd.spawn();
}

/// Doctor's `bay` row: the installed version, the box's pin, and the newest release from the
/// last recorded check. Doctor makes no network call. A pin is a floor, so a pinned box has an
/// update available whenever a newer release is out. An old channel setting that is still set
/// is named, and the row stays green.
#[must_use]
pub fn doctor_row() -> Value {
    let settings = match BoxSettings::load() {
        Ok(settings) => settings,
        Err(why) => {
            return json!({"name": "bay", "version": BAY, "state": "red", "why": why,
                "recovery": format!("set {VERSION_VAR} to a valid version, or unset it")});
        }
    };
    let state = State::read();
    let pin = settings.version;
    let mut row = json!({"name": "bay", "version": BAY, "pin": pin,
        "pin_source": settings.version_from, "state": "green"});
    if let Some(ignored) = settings.channel_ignored {
        row["channel_ignored"] = json!(ignored);
        row["note"] = json!(format!(
            "every release is a candidate; {ignored} is no longer read"
        ));
    }
    if let Some(newest) = state.and_then(|state| state.newest) {
        row["update_available"] = json!(compare(&newest, BAY) == Ordering::Greater);
        row["newest"] = json!(newest);
    }
    if let Some(error) = State::read().and_then(|state| state.install_error) {
        row["install_error"] = json!(error);
        row["recovery"] = json!("run bay install");
    }
    row
}

fn sha256(bytes: &[u8]) -> String {
    crate::agent_settings::hex(&Sha256::digest(bytes))
}

/// The digest `SHA256SUMS` lists for `name` (`shasum -a 256` lines, text or binary mode).
fn sum_of(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let digest = fields.next()?;
        let file = fields.next()?;
        (file.trim_start_matches('*') == name && digest.len() == 64)
            .then(|| digest.to_ascii_lowercase())
    })
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|error| error.to_string())
}

/// Without Unix modes a file runs by its name, so there is no bit to set; the file must still
/// be a file, as the Unix call would require.
#[cfg(not(unix))]
fn set_executable(path: &Path) -> Result<(), String> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => Ok(()),
        Ok(_) => Err(format!("{} is not a file", path.display())),
        Err(error) => Err(error.to_string()),
    }
}

/// `X.Y.Z` or `X.Y.Z-suffix`, as `scripts/release.sh` accepts.
#[must_use]
pub fn valid(version: &str) -> bool {
    let (core, suffix) = version
        .split_once('-')
        .map_or((version, None), |(core, suffix)| (core, Some(suffix)));
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        && suffix.is_none_or(|suffix| {
            !suffix.is_empty()
                && suffix
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.')
        })
}

/// Semantic-version order: the numbers, then a pre-release below its release, then the
/// pre-release identifiers (numbers numerically, below words).
#[must_use]
pub fn compare(a: &str, b: &str) -> Ordering {
    let split = |version: &str| -> (Vec<u64>, Option<String>) {
        let (core, suffix) = version
            .split_once('-')
            .map_or((version, None), |(core, suffix)| {
                (core, Some(suffix.to_owned()))
            });
        (
            core.split('.')
                .map(|part| part.parse().unwrap_or(0))
                .collect(),
            suffix,
        )
    };
    let ((a_core, a_pre), (b_core, b_pre)) = (split(a), split(b));
    a_core.cmp(&b_core).then_with(|| match (a_pre, b_pre) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => {
            let ident = |x: &str, y: &str| match (x.parse::<u64>(), y.parse::<u64>()) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                (Err(_), Err(_)) => x.cmp(y),
            };
            let (a, b): (Vec<&str>, Vec<&str>) = (a.split('.').collect(), b.split('.').collect());
            a.iter()
                .zip(&b)
                .map(|(x, y)| ident(x, y))
                .find(|ordering| ordering.is_ne())
                .unwrap_or_else(|| a.len().cmp(&b.len()))
        }
    })
}

/// A directory removed when dropped, whatever the outcome.
struct TempDir(PathBuf);

impl TempDir {
    fn new(parent: &Path) -> Result<Self, String> {
        let id = crate::ids::new_change_id().map_err(|error| error.to_string())?;
        let path = parent.join(format!(".bay-update-{id}"));
        std::fs::create_dir_all(&path)
            .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::{RELEASE_KEYS, TempDir, allowed_signers, compare, key_lines, sum_of, valid};
    use std::cmp::Ordering;
    use std::process::Command;

    /// A hardware key held in a secure enclave, as OpenSSH writes its public line
    /// (`sk-ecdsa-sha2-nistp256@openssh.com`): a well-formed fixture with no device behind it.
    const SK_ECDSA: &str = "sk-ecdsa-sha2-nistp256@openssh.com AAAAInNrLWVjZHNhLXNoYTItbmlzdHAyNTZAb3BlbnNzaC5jb20AAAAIbmlzdHAyNTYAAABBBGsX0fLhLEJH+Lzm5WOkQPJ3A32BLeszoPShOUXYmMKWT+NC4v4af5uO5+tKfA+eFivOM1drMV7Oy7ZAaDe/UfUAAAAEc3NoOg==";

    #[test]
    fn the_pinned_list_and_the_release_scripts_allowed_signers_agree() {
        // scripts/release.sh verifies against this file; bay update against RELEASE_KEYS.
        let file = include_str!("../scripts/release/allowed_signers");
        let listed: Vec<String> = file
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let words: Vec<&str> = line.split_whitespace().collect();
                assert_eq!(words[0], "bay-release", "{line}");
                words[words.len() - 2..].join(" ")
            })
            .collect();
        assert_eq!(listed, RELEASE_KEYS);
    }

    #[test]
    fn ssh_keygen_takes_an_sk_ecdsa_allowed_signers_line() {
        let keys = key_lines(&format!("{SK_ECDSA}\n"));
        assert_eq!(keys, [SK_ECDSA]);
        assert_eq!(allowed_signers(&keys), format!("bay-release {SK_ECDSA}\n"));

        // A file key B signs; the list is [the enclave key, B]. ssh-keygen skips a line it
        // cannot parse with "invalid key" and goes on, so a clean stderr is the format check.
        let dir = TempDir::new(&std::env::temp_dir()).unwrap();
        let key = dir.0.join("b");
        let run = |cmd: &mut Command| {
            let out = cmd.output().unwrap();
            assert!(out.status.success(), "{cmd:?}: {out:?}");
            out
        };
        run(Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key));
        let data = dir.0.join("data");
        std::fs::write(&data, "sums\n").unwrap();
        run(Command::new("ssh-keygen")
            .args(["-Y", "sign", "-q", "-n", "bay-release", "-f"])
            .arg(&key)
            .arg(&data));
        let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
        let b: String = public
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
        let allowed = dir.0.join("allowed_signers");
        std::fs::write(&allowed, allowed_signers(&[SK_ECDSA.to_owned(), b])).unwrap();
        let out = run(Command::new("ssh-keygen")
            .args([
                "-Y",
                "verify",
                "-n",
                "bay-release",
                "-I",
                "bay-release",
                "-f",
            ])
            .arg(&allowed)
            .arg("-s")
            .arg(dir.0.join("data.sig"))
            .stdin(std::fs::File::open(&data).unwrap()));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.contains("invalid key"), "{stderr}");
    }

    #[test]
    fn every_listed_key_gets_its_own_allowed_signers_line() {
        // A hardware key's public line, as `ssh-keygen -t ed25519-sk` writes one; no device is
        // needed to list it.
        let sk = "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29tAAAAIDgOPZsmzgIilFlX+FEXSGHrntiIAzexLc6I0f+I9/4qAAAAD3NzaDpiYXktcmVsZWFzZQ==";
        let keys = key_lines(&format!("{}\n\n  {sk}  \n", RELEASE_KEYS[0]));
        assert_eq!(keys, [RELEASE_KEYS[0], sk]);
        assert_eq!(
            allowed_signers(&keys),
            format!("bay-release {}\nbay-release {sk}\n", RELEASE_KEYS[0])
        );
    }

    #[test]
    fn versions_order_like_semver() {
        assert_eq!(compare("0.1.1", "0.1.0"), Ordering::Greater);
        assert_eq!(compare("0.1.0", "0.1.0-pilot.1"), Ordering::Greater);
        assert_eq!(
            compare("0.1.0-pilot.10", "0.1.0-pilot.2"),
            Ordering::Greater
        );
        assert_eq!(compare("0.10.0", "0.9.0"), Ordering::Greater);
        assert_eq!(compare("1.0.0", "1.0.0"), Ordering::Equal);
    }

    #[test]
    fn only_release_versions_are_valid() {
        assert!(valid("0.1.0") && valid("0.1.0-pilot.1"));
        assert!(!valid("0.1") && !valid("v0.1.0") && !valid("0.1.0-") && !valid("0.1.0-a/b"));
    }

    #[test]
    fn a_state_file_with_the_old_channel_still_reads() {
        let state: super::State =
            serde_json::from_str(r#"{"checked_at":1,"channel":"stable","newest":"9.1.0"}"#)
                .unwrap();
        assert_eq!(state.newest.as_deref(), Some("9.1.0"));
    }

    #[test]
    fn sums_are_read_in_text_and_binary_mode() {
        let digest = "a".repeat(64);
        let sums = format!("{digest}  VERSION\n{digest} *bay-x.tar.gz\n");
        assert_eq!(sum_of(&sums, "VERSION"), Some(digest.clone()));
        assert_eq!(sum_of(&sums, "bay-x.tar.gz"), Some(digest));
        assert_eq!(sum_of(&sums, "other"), None);
    }

    #[test]
    fn each_platform_has_its_archive() {
        assert_eq!(
            super::archive_name("x86_64-unknown-linux-musl"),
            "bay-x86_64-unknown-linux-musl.tar.gz"
        );
        assert_eq!(
            super::archive_name("aarch64-apple-darwin"),
            "bay-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            super::archive_name("x86_64-pc-windows-msvc"),
            "bay-x86_64-pc-windows-msvc.zip"
        );
        let here = super::platform().unwrap();
        assert!(here.starts_with(std::env::consts::ARCH));
        assert_eq!(
            std::path::Path::new(&super::archive_name(&here))
                .extension()
                .is_some_and(|ext| ext == "zip"),
            cfg!(windows)
        );
    }
}
