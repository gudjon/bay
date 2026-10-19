//! `bay update`: the binary's lifecycle (`DESIGN`, the § on `bay update`; `INTERFACES` § 2).
//!
//! The pin, the channel and the auto switch are the box's, never a repository's: global Git
//! config `bay.updateVersion`, `bay.updateChannel` and `bay.updateAuto` (owner ruling
//! 2026-10-18), or `--version` and `--channel` for one run. A process's `BAY_UPDATE_VERSION` and
//! `BAY_UPDATE_CHANNEL` override the first two for that process, so one seat on a shared login
//! can choose its own (flags, then the environment, then global config, then defaults). A cloned
//! repository never moves the box.
//!
//! Resolves a target (an exact pin wins, then `--version`, then the channel's newest release),
//! downloads the release beside the installed binary (`$HOME/.local/bin/bay`, bay's one install
//! path, whichever copy runs), verifies it all or nothing, unpacks only `bay` from the bytes it
//! hashed, and swaps it in with one `rename(2)`. A miss anywhere leaves the old binary
//! byte-identical (G50).
//!
//! A release is `bay-<target>.tar.gz` per platform, `VERSION`, `SHA256SUMS` over both, and
//! `SHA256SUMS.sig`: an SSH signature by the release key, principal and namespace
//! `bay-release`. The signed sums cover `VERSION`, so an older signed release cannot stand in
//! for the target.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::git;
use crate::result::{BayResult, Decision, Reason};
use crate::version::BAY;

/// The public half of the bay release key. `SHA256SUMS.sig` must verify against it.
pub const RELEASE_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPnvMnsFVUR3lBcKXj2PkjHH3mIDFHKl2/Ov/lgVHMM7";
const PRINCIPAL: &str = "bay-release";
const RELEASES: &str = "https://github.com/gudjon/bay/releases";
const RELEASE_LIST: &str = "https://api.github.com/repos/gudjon/bay/releases?per_page=100&page=";
/// A bound on the pages read, so a list that never ends cannot hold the command.
const MAX_PAGES: usize = 100;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_mins(5);

/// What `bay update` was asked for.
#[derive(Debug, Default)]
pub struct Request {
    pub channel: Option<String>,
    pub version: Option<String>,
    pub check: bool,
}

/// Where releases come from, and the key they must verify against.
struct Source {
    /// The list's URL up to its page number, and what follows the number.
    list: (String, String),
    download: String,
    key: String,
    https_only: bool,
}

impl Source {
    /// The public releases. A build that `scripts/release.sh` did not make (`BAY_RELEASE_VERSION`
    /// unset) also honours `BAY_TEST_RELEASE_URL` and `BAY_TEST_RELEASE_KEY`, for G50's fixture;
    /// a release build never does.
    fn current() -> Self {
        let test = option_env!("BAY_RELEASE_VERSION").is_none();
        let key = std::env::var("BAY_TEST_RELEASE_KEY")
            .ok()
            .filter(|_| test)
            .unwrap_or_else(|| RELEASE_KEY.to_owned());
        match std::env::var("BAY_TEST_RELEASE_URL").ok().filter(|_| test) {
            Some(base) => Self {
                list: (format!("{base}/releases-"), ".json".to_owned()),
                download: format!("{base}/download"),
                key,
                https_only: false,
            },
            None => Self {
                list: (RELEASE_LIST.to_owned(), String::new()),
                download: format!("{RELEASES}/download"),
                key,
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

/// This platform's release target, as `scripts/release.sh` names its archives.
#[must_use]
pub fn platform() -> Option<String> {
    let os = match std::env::consts::OS {
        "linux" => "unknown-linux-musl",
        "macos" => "apple-darwin",
        _ => return None,
    };
    Some(format!("{}-{os}", std::env::consts::ARCH))
}

#[must_use]
pub fn update(request: &Request) -> BayResult {
    run(request).unwrap_or_else(|refusal| *refusal)
}

/// Where a box setting came from: a process's environment wins over global Git config.
pub const FROM_ENV: &str = "env";
pub const FROM_GIT_CONFIG: &str = "git config";

/// The environment variables that set the pin and the channel for one process (and its
/// children), over global Git config: on a shared login every seat shares that config, so a
/// seat's supervisor sets its choice here.
pub const VERSION_VAR: &str = "BAY_UPDATE_VERSION";
pub const CHANNEL_VAR: &str = "BAY_UPDATE_CHANNEL";

/// This box's update settings: the process's `BAY_UPDATE_VERSION` and `BAY_UPDATE_CHANNEL`,
/// else global Git config. A repository's config is never read.
#[derive(Debug, Default)]
pub struct BoxSettings {
    pub channel: Option<String>,
    pub version: Option<String>,
    /// `bay.updateAuto`; unset is on.
    pub auto: bool,
    /// Where `channel` came from ([`FROM_ENV`] or [`FROM_GIT_CONFIG`]), when set.
    pub channel_from: Option<&'static str>,
    /// Where `version` came from, when set.
    pub version_from: Option<&'static str>,
}

impl BoxSettings {
    /// # Errors
    ///
    /// An environment value that is not a version or a channel, naming the variable: a set
    /// variable is never silently ignored.
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
        let env_channel = env(CHANNEL_VAR)?;
        if let Some(channel) = &env_channel
            && channel != "stable"
            && channel != "latest"
        {
            return Err(format!(
                "{CHANNEL_VAR} must be stable or latest, not {channel:?}"
            ));
        }
        let pick = |env: Option<String>, key: &str| {
            env.map_or_else(
                || {
                    let value = get(&["--get", key]);
                    let from = value.as_ref().map(|_| FROM_GIT_CONFIG);
                    (value, from)
                },
                |value| (Some(value), Some(FROM_ENV)),
            )
        };
        let (channel, channel_from) = pick(env_channel, "bay.updateChannel");
        let (version, version_from) = pick(env_version, "bay.updateVersion");
        Ok(Self {
            channel,
            version,
            auto: get(&["--type=bool", "--get", "bay.updateAuto"]).is_none_or(|on| on != "false"),
            channel_from,
            version_from,
        })
    }

    /// How the pin is named in a refusal: the variable or the config key it came from.
    fn pin_name(&self) -> &'static str {
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
    channel: String,
    pin: Option<String>,
}

impl Plan {
    fn detail(&self, updated: bool) -> Value {
        json!({"installed": BAY, "target": self.target, "channel": self.channel,
            "pinned": self.pin, "updated": updated})
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
    let channel = request.channel.clone().or(settings.channel);
    if let Some(channel) = &channel
        && channel != "stable"
        && channel != "latest"
    {
        return Err(failed(format!(
            "bay.updateChannel must be stable or latest, not {channel}"
        )));
    }
    for version in [&pin, &request.version].into_iter().flatten() {
        if !valid(version) {
            return Err(failed(format!(
                "{version} is not a version (X.Y.Z or X.Y.Z-suffix)"
            )));
        }
    }
    let source = Source::current();
    let plan = resolve(&source, pin, channel, request.version.clone())?;

    if let (Some(pin), Some(version)) = (&plan.pin, &request.version)
        && pin != version
    {
        return Err(plan.refuse(
            Decision::Blocked,
            Reason::UpdatePinned,
            format!("this box is pinned to {pin}; only its pin ({pin_name}) moves it"),
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
    // A running executor finishes its operation first, then restarts on the new binary.
    if let Some(pid) = crate::executor::busy() {
        return Err(plan.refuse(
            Decision::Blocked,
            Reason::UpdateInUse,
            format!("executor {pid} on this box holds an operation"),
        ));
    }
    let installed = installed_path().map_err(failed)?;
    let bin_dir = installed
        .parent()
        .ok_or_else(|| failed("the install path has no directory"))?;
    std::fs::create_dir_all(bin_dir)
        .map_err(|error| failed(format!("cannot create {}: {error}", bin_dir.display())))?;
    // Beside the binary, so the swap is a rename within one filesystem.
    let tmp = TempDir::new(bin_dir).map_err(failed)?;
    let archive = download_verified(&source, &plan, &tmp.0)?;
    swap(&plan, &tmp.0, &archive, &installed)
}

/// A pin wins, then `--version`, then the channel's newest release. With no channel given, a
/// box running a pre-release follows `latest` (`stable` stays empty until the first one).
fn resolve(
    source: &Source,
    pin: Option<String>,
    channel: Option<String>,
    version: Option<String>,
) -> Result<Plan, Refusal> {
    let unavailable = |channel: &str, why: String| {
        Box::new(
            BayResult::blocked(Reason::UpdateUnavailable)
                .with_detail(json!({"installed": BAY, "channel": channel, "why": why})),
        )
    };
    if let Some(target) = pin.clone().or(version) {
        let channel = channel.unwrap_or_else(|| default_channel(false).to_owned());
        return Ok(Plan {
            target,
            channel,
            pin,
        });
    }
    // With no channel given, the channel depends on whether the installed version is itself a
    // pre-release, so it is decided again with every page read.
    let decide = |releases: &[(String, bool)]| {
        channel.clone().unwrap_or_else(|| {
            let pre = releases.iter().any(|(version, pre)| *pre && version == BAY);
            default_channel(pre).to_owned()
        })
    };
    let releases = release_list(source, |releases| {
        let channel = decide(releases);
        releases.iter().any(|(_, pre)| channel == "latest" || !pre)
    })
    .map_err(|why| unavailable(channel.as_deref().unwrap_or(default_channel(false)), why))?;
    let channel = decide(&releases);
    let newest = releases
        .into_iter()
        .filter(|(_, pre)| channel == "latest" || !pre)
        .map(|(version, _)| version)
        .max_by(|a, b| compare(a, b));
    State::record(&channel, newest.as_deref());
    let newest = newest
        .ok_or_else(|| unavailable(&channel, format!("no release on the {channel} channel")))?;
    Ok(Plan {
        target: newest,
        channel,
        pin,
    })
}

fn default_channel(installed_is_pre_release: bool) -> &'static str {
    if installed_is_pre_release || BAY.contains('-') {
        "latest"
    } else {
        "stable"
    }
}

/// The published releases as `(version, prerelease)`, read page by page until `enough` holds
/// for what was read or the list ends; drafts and tags that are not versions are skipped.
fn release_list(
    source: &Source,
    enough: impl Fn(&[(String, bool)]) -> bool,
) -> Result<Vec<(String, bool)>, String> {
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
                    valid(version).then(|| (version.to_owned(), release["prerelease"] == true))
                }),
        );
        if enough(&releases) {
            break;
        }
    }
    Ok(releases)
}

/// bay's one install path, `$HOME/.local/bin/bay` (`DESIGN` § 15.1). An update writes only
/// this path, whichever copy of bay runs it.
fn installed_path() -> Result<PathBuf, String> {
    let home = crate::platform::home().ok_or("HOME is not set; update cannot name its path")?;
    Ok(PathBuf::from(home).join(".local").join("bin").join("bay"))
}

/// Steps 3–4: downloads the release into `tmp` and verifies it, all or nothing. Returns the
/// archive's bytes, whose digest the signed sums list; only these bytes are unpacked.
fn download_verified(source: &Source, plan: &Plan, tmp: &Path) -> Result<Vec<u8>, Refusal> {
    let platform =
        platform().ok_or_else(|| plan.unavailable("no release build for this platform"))?;
    let archive = format!("bay-{platform}.tar.gz");
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
    std::fs::write(&allowed, format!("{PRINCIPAL} {}\n", source.key))
        .map_err(|error| failed(error.to_string()))?;
    let signature = String::from_utf8_lossy(&signature);
    if !crate::signing::verify_data(&allowed, PRINCIPAL, PRINCIPAL, &sums, &signature) {
        return Err(plan.unverified("SHA256SUMS does not verify against the bay release key"));
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

/// Step 5: unpack only `bay` from the verified bytes, run the new binary's `--version`,
/// `rename(2)` it over the installed path, then run its `install`.
fn swap(plan: &Plan, tmp: &Path, archive: &[u8], installed: &Path) -> Result<BayResult, Refusal> {
    let unpacked = tmp.join("x");
    std::fs::create_dir_all(&unpacked).map_err(|error| failed(error.to_string()))?;
    let mut tar = Command::new("tar");
    tar.args(["-xzf", "-", "--no-same-owner", "-C"])
        .arg(&unpacked)
        .arg("bay");
    let output = git::run_bounded_with_input(tar, "tar", archive)
        .map_err(|error| failed(error.message()))?;
    let new = unpacked.join("bay");
    // A link or anything else that is not a regular file is refused, never followed.
    let regular = std::fs::symlink_metadata(&new).is_ok_and(|meta| meta.file_type().is_file());
    if !output.status.success() || !regular {
        return Err(plan.unverified("the archive holds no regular file named bay"));
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
    std::fs::rename(&new, installed)
        .map_err(|error| failed(format!("cannot replace {}: {error}", installed.display())))?;

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
/// `~/.local/state/bay/update.json`): when it ran, and the channel's newest release.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Seconds since the Unix epoch.
    pub checked_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest: Option<String>,
    /// Why the new binary's `install` failed after the last update, until one succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_error: Option<String>,
}

const DAY_S: u64 = 24 * 3600;

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
        std::fs::rename(&tmp, &path).map_err(|error| error.to_string())
    }

    /// A check of `channel` that found `newest`.
    fn record(channel: &str, newest: Option<&str>) {
        let mut state = Self::read().unwrap_or_default();
        state.checked_at = now_s();
        state.channel = Some(channel.to_owned());
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

fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Auto-update, from `status` and `doctor` only (never `land`, `executor run` or the hook,
/// which hold or may hold an operation, nor while an executor on this box holds one): at most one check a day, run as a detached
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
    if state.checked_at <= now && now - state.checked_at < DAY_S {
        return;
    }
    // An invalid environment value starts no check; the next `bay update` names it.
    let Ok(settings) = BoxSettings::load() else {
        return;
    };
    if !settings.auto || settings.version.is_some() {
        return;
    }
    // Not while an executor holds an operation; the day is not used up, so the next command
    // checks again.
    if crate::executor::busy().is_some() {
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
    let Ok(bay) = std::env::current_exe() else {
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

/// Doctor's `bay` row: the installed version, the box's channel and pin, and the channel's
/// newest release when the last recorded check was of this channel. Doctor makes no network
/// call. A pinned box has no update available: only its pin moves it.
#[must_use]
pub fn doctor_row() -> Value {
    let settings = match BoxSettings::load() {
        Ok(settings) => settings,
        Err(why) => {
            return json!({"name": "bay", "version": BAY, "state": "red", "why": why,
                "recovery": format!("set {VERSION_VAR} and {CHANNEL_VAR} to valid values, or unset them")});
        }
    };
    let state = State::read();
    let channel_from = settings.channel_from.unwrap_or("default");
    let channel = settings
        .channel
        .unwrap_or_else(|| default_channel(false).to_owned());
    let pin = settings.version;
    let mut row = json!({"name": "bay", "version": BAY, "channel": channel,
        "channel_source": channel_from, "pin": pin, "pin_source": settings.version_from,
        "state": "green"});
    if let Some(newest) = state
        .filter(|state| state.channel.as_deref() == Some(channel.as_str()))
        .and_then(|state| state.newest)
    {
        row["update_available"] =
            json!(pin.is_none() && compare(&newest, BAY) == Ordering::Greater);
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
    use super::{compare, sum_of, valid};
    use std::cmp::Ordering;

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
    fn sums_are_read_in_text_and_binary_mode() {
        let digest = "a".repeat(64);
        let sums = format!("{digest}  VERSION\n{digest} *bay-x.tar.gz\n");
        assert_eq!(sum_of(&sums, "VERSION"), Some(digest.clone()));
        assert_eq!(sum_of(&sums, "bay-x.tar.gz"), Some(digest));
        assert_eq!(sum_of(&sums, "other"), None);
    }
}
