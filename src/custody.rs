//! Doctor's `key-custody` row (G145): a file-based signing key that the account running agent
//! sessions can read.
//!
//! File modes separate OS accounts, not the sessions of one account: a key file that one
//! session can read, every session under that user can read, and a session that reads
//! untrusted text can be steered into signing with it. Passwordless sudo makes every file on
//! the box readable to such a session. So a release, producer, executor or registry private
//! key, or a token file beside one, that this user can read is red with `KEY_EXPOSED` when this
//! machine's session records show agent sessions under this user, or when this user has
//! passwordless sudo (`sudo -n true`, asked only when there is such a file). An agent key
//! belongs to the session account by design and is only listed. A hardware key's file
//! (`ed25519-sk`, its public half beside it) is a handle that signs only with a touch on the
//! device, so it is not red; a token beside it still is.
//!
//! The row reads paths, modes, ownership and public halves only. It never opens a private key
//! or a token.

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::git::Git;

/// The settings that name this box's private keys, and each one's key class. A registry key and
/// a release key are not used by bay itself: `bay.registryKey` and `bay.releaseKey` say where
/// the box keeps them, so this row can judge them.
#[cfg(unix)]
const SETTINGS: &[(&str, &str)] = &[
    ("bay.releaseKey", "release"),
    ("bay.gateProducerKey", "producer"),
    ("bay.producerKey", "producer"),
    ("bay.reviewerKey", "reviewer"),
    ("bay.approverKey", "approver"),
    ("bay.executorKey", "executor"),
    ("bay.registryKey", "registry"),
    ("bay.agentKey", "agent"),
];

#[cfg(unix)]
const RECOVERY: &str = "move the key to a hardware key (ssh-keygen -t ed25519-sk, which signs only with a touch) or to a key outside this account, move the token out of this account, then rotate both";

/// The row, and whether it is red.
pub struct Custody {
    pub row: Value,
    pub exposed: bool,
}

/// The row for the keys `git`'s config names, judged against the session records in
/// `sessions` (`<git-common-dir>/bay/sessions`, when doctor runs in a work repository).
#[cfg(unix)]
#[must_use]
pub fn row(git: &Git, sessions: Option<&Path>) -> Custody {
    let Some(account) = Account::here() else {
        return Custody {
            row: json!({"name": "key-custody", "required": true, "state": "unknown",
                "note": "id could not name this account"}),
            exposed: false,
        };
    };
    let keys: Vec<Key> = SETTINGS
        .iter()
        .filter_map(|(setting, class)| {
            let path = git
                .run(&["config", "--type=path", "--get", setting])
                .ok()
                .filter(|value| !value.is_empty())?;
            Some(Key::judge(setting, class, PathBuf::from(path), &account))
        })
        .collect();
    let candidates = keys.iter().any(Key::exposable);
    let records = sessions.map_or(0, |dir| account.session_records(dir));
    // Asked only when a file could be exposed: it is the one probe this row runs.
    let sudo = candidates.then(passwordless_sudo);
    let mut reasons = Vec::new();
    if candidates && records > 0 {
        reasons.push(format!(
            "this account runs agent sessions ({records} session records on this machine)"
        ));
    }
    if sudo == Some(true) {
        reasons.push(
            "this account has passwordless sudo, so every session under it can read any file"
                .to_owned(),
        );
    }
    let exposed = !reasons.is_empty();
    // G243: an agent key belongs to the session account, so when several actors run their
    // sessions under this one account, any of them can sign as any other: bay cannot prove
    // which actor signed. Advisory, never red: the box works, and the remedy is the site's.
    let actors = sessions.map(|dir| account.actors(dir)).unwrap_or_default();
    let actor_proof = proof_of(keys.iter().find(|key| key.class == "agent"), actors.len());
    let shared = actor_proof == ACCOUNT;
    let state = if exposed {
        "red"
    } else if shared {
        "degraded"
    } else {
        "green"
    };
    let mut row = json!({"name": "key-custody", "required": true,
        "state": state, "uid": account.uid,
        "keys": keys.iter().map(Key::to_json).collect::<Vec<_>>(),
        "session_records": records, "passwordless_sudo": sudo,
        "actors": actors, "actor_proof": actor_proof});
    if shared {
        row["actor_note"] = json!(format!(
            "{} actors run sessions under this one OS account, which can read the agent keys: bay cannot prove which actor signed; give each actor its own OS account, or a hardware agent key",
            actors.len()
        ));
    }
    if exposed {
        row["note"] = json!(format!(
            "a file-based signing key or a token beside it is readable by this account: {}",
            reasons.join("; ")
        ));
        row["recovery"] = json!(RECOVERY);
    } else if keys.is_empty() {
        row["note"] = json!("no private key is configured on this box");
    }
    Custody { row, exposed }
}

/// `actor_proof` when several actors' sessions run under this one OS account, which can read
/// the file-based agent key: any of them can sign as any other (G243).
pub const ACCOUNT: &str = "account";

/// What proves which actor signs this box's agent requests (G243, G243b).
pub struct ActorProof {
    /// `key` (the agent key proves it), `account` (several actors share an account that can
    /// read the agent key), `none` (no agent key is configured) or `unknown` (not judged here).
    pub proof: &'static str,
    /// The actors this machine's session records under this account name.
    pub actors: Vec<String>,
}

impl ActorProof {
    /// Whether the agent key cannot prove the actor: `account`, or `unknown`, since a proof bay
    /// cannot judge is no proof. `none` is not judged here: with no agent key nothing is signed,
    /// and a repository with a key registry refuses an unsigned request already.
    #[must_use]
    pub fn unproven(&self) -> bool {
        matches!(self.proof, ACCOUNT | "unknown")
    }
}

/// [`ActorProof`] for the agent key `git`'s config names, judged against the session records
/// in `sessions` as the `key-custody` row judges it: the same reading, so a refusal and the
/// row never disagree.
#[cfg(unix)]
#[must_use]
pub fn actor_proof(git: &Git, sessions: Option<&Path>) -> ActorProof {
    let Some(account) = Account::here() else {
        return ActorProof {
            proof: "unknown",
            actors: Vec::new(),
        };
    };
    let agent = git
        .run(&["config", "--type=path", "--get", "bay.agentKey"])
        .ok()
        .filter(|value| !value.is_empty())
        .map(|path| Key::judge("bay.agentKey", "agent", PathBuf::from(path), &account));
    let actors = sessions.map(|dir| account.actors(dir)).unwrap_or_default();
    ActorProof {
        proof: proof_of(agent.as_ref(), actors.len()),
        actors: actors.into_iter().collect(),
    }
}

/// Off Unix file modes are not judged: a hardware agent key still proves the actor, and a
/// file-based one is `unknown`.
#[cfg(not(unix))]
#[must_use]
pub fn actor_proof(git: &Git, _sessions: Option<&Path>) -> ActorProof {
    let agent = git
        .run(&["config", "--type=path", "--get", "bay.agentKey"])
        .ok()
        .filter(|value| !value.is_empty());
    let proof = match agent {
        None => "none",
        Some(path) if is_hardware(Path::new(&path)) => "key",
        Some(_) => "unknown",
    };
    ActorProof {
        proof,
        actors: Vec::new(),
    }
}

/// The proof `agent` (the configured agent key) gives when `actors` actors run sessions under
/// this account: none without a key; the account alone when a file-based key this account can
/// read serves more than one actor; else the key.
#[cfg(unix)]
fn proof_of(agent: Option<&Key>, actors: usize) -> &'static str {
    match agent {
        None => "none",
        Some(key)
            if !key.hardware && key.file.is_some_and(|(_, _, readable)| readable) && actors > 1 =>
        {
            ACCOUNT
        }
        Some(_) => "key",
    }
}

/// Not judged off Unix: modes and owners are Unix's.
#[cfg(not(unix))]
#[must_use]
pub fn row(_git: &Git, _sessions: Option<&Path>) -> Custody {
    Custody {
        row: json!({"name": "key-custody", "required": false, "state": "not checked",
            "note": "file modes are judged on Unix only"}),
        exposed: false,
    }
}

/// The OS account doctor runs as: its uid and groups, from `id`.
#[cfg(unix)]
struct Account {
    uid: u32,
    groups: Vec<u32>,
}

#[cfg(unix)]
impl Account {
    fn here() -> Option<Self> {
        let id = |flag: &str| {
            let mut cmd = std::process::Command::new("id");
            cmd.arg(flag);
            let output = crate::git::run_bounded(cmd, "id").ok()?;
            output.status.success().then(|| {
                String::from_utf8_lossy(&output.stdout)
                    .split_whitespace()
                    .filter_map(|word| word.parse().ok())
                    .collect::<Vec<u32>>()
            })
        };
        let uid = *id("-u")?.first()?;
        Some(Self {
            uid,
            groups: id("-G").unwrap_or_default(),
        })
    }

    /// Whether this account can read a file with `meta`'s owner, group and mode.
    fn reads(&self, meta: &std::fs::Metadata) -> bool {
        use std::os::unix::fs::MetadataExt;
        let mode = meta.mode();
        self.uid == 0
            || (meta.uid() == self.uid && mode & 0o400 != 0)
            || (self.groups.contains(&meta.gid()) && mode & 0o040 != 0)
            || mode & 0o004 != 0
    }

    /// The actors named in the session records in `dir` this account wrote (G243).
    fn actors(&self, dir: &Path) -> std::collections::BTreeSet<String> {
        use std::os::unix::fs::MetadataExt;
        let Ok(entries) = std::fs::read_dir(dir) else {
            return std::collections::BTreeSet::new();
        };
        entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
            .filter(|entry| {
                entry
                    .metadata()
                    .is_ok_and(|meta| meta.is_file() && meta.uid() == self.uid)
            })
            .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
            .flat_map(|text| {
                text.lines()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .filter_map(|line| line["actor"]["name"].as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// The session records in `dir` this account wrote: agent sessions run under it.
    fn session_records(&self, dir: &Path) -> usize {
        use std::os::unix::fs::MetadataExt;
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
            .filter(|entry| {
                entry
                    .metadata()
                    .is_ok_and(|meta| meta.is_file() && meta.uid() == self.uid)
            })
            .count()
    }
}

/// One configured key, judged by its path's metadata.
#[cfg(unix)]
struct Key {
    setting: &'static str,
    class: &'static str,
    path: PathBuf,
    /// `(mode, owner uid, readable)` when the file is there.
    file: Option<(u32, u32, bool)>,
    hardware: bool,
    /// Readable token files in the key's directory.
    tokens: Vec<PathBuf>,
}

#[cfg(unix)]
impl Key {
    fn judge(setting: &'static str, class: &'static str, path: PathBuf, account: &Account) -> Self {
        use std::os::unix::fs::MetadataExt;
        let file = std::fs::metadata(&path)
            .ok()
            .filter(std::fs::Metadata::is_file)
            .map(|meta| (meta.mode() & 0o7777, meta.uid(), account.reads(&meta)));
        let hardware = is_hardware(&path);
        let tokens = if class == "agent" {
            Vec::new()
        } else {
            tokens_beside(&path, account)
        };
        Self {
            setting,
            class,
            path,
            file,
            hardware,
            tokens,
        }
    }

    /// A key file (not a hardware handle) or a token this account can read, of a class the
    /// session account must not hold.
    fn exposable(&self) -> bool {
        self.class != "agent"
            && (self
                .file
                .is_some_and(|(_, _, readable)| readable && !self.hardware)
                || !self.tokens.is_empty())
    }

    fn to_json(&self) -> Value {
        let mut value = json!({"setting": self.setting, "class": self.class,
            "path": self.path, "present": self.file.is_some(), "hardware": self.hardware,
            "tokens": self.tokens});
        if let Some((mode, owner, readable)) = self.file {
            value["mode"] = json!(format!("{mode:04o}"));
            value["owner_uid"] = json!(owner);
            value["readable"] = json!(readable);
        }
        if self.class == "agent" {
            value["note"] = json!("an agent key belongs to the session account");
        }
        value
    }
}

/// Whether the key's public half (`<path>.pub`) names a hardware key type (`sk-…`). Only the
/// public half is read, and only its first bytes.
fn is_hardware(path: &Path) -> bool {
    use std::io::Read as _;
    let mut public = path.as_os_str().to_owned();
    public.push(".pub");
    let Ok(file) = std::fs::File::open(public) else {
        return false;
    };
    let mut head = String::new();
    file.take(64).read_to_string(&mut head).is_ok() && head.starts_with("sk-")
}

/// Regular files beside `key` whose name holds `token`, that this account can read.
#[cfg(unix)]
fn tokens_beside(key: &Path, account: &Account) -> Vec<PathBuf> {
    let Some(entries) = key.parent().and_then(|dir| std::fs::read_dir(dir).ok()) else {
        return Vec::new();
    };
    let mut tokens: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains("token")
        })
        .map(|entry| entry.path())
        .filter(|path| {
            std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && account.reads(&meta))
        })
        .collect();
    tokens.sort();
    tokens
}

/// `sudo -n true`: whether this account runs sudo without a password. It changes nothing.
#[cfg(unix)]
fn passwordless_sudo() -> bool {
    let mut cmd = std::process::Command::new("sudo");
    cmd.args(["-n", "true"]);
    crate::git::run_bounded_for(
        cmd,
        "sudo -n true",
        None,
        std::time::Duration::from_secs(10),
    )
    .is_ok_and(|output| output.status.success())
}
