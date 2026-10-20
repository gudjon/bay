//! The change record and its commit chain (`DESIGN` § 3.1, § 5).
//!
//! `refs/bay/changes/<id>` is a chain of state commits. Each state commit's tree holds
//! `change.json`, the change record. Its **last** parent is the latest draft commit, which
//! keeps every draft reachable by link (§ 5: a sha inside a blob or a note does not survive
//! `gc --prune=now`; a parent link does). When it has two or more parents, the first is the
//! previous state commit and the second the draft; a third, when present, is a candidate
//! commit the executor persisted before publishing (§ 6 step 5).

use serde::{Deserialize, Serialize};

use crate::git::{Git, GitError};
use crate::signing::Signer;

pub const CHANGES_PREFIX: &str = crate::names::CHANGES;
pub const RECORD_FILE: &str = "change.json";
pub const RECORD_SCHEMA: &str = "bay.change/1";
/// The schema of a record that declares a named input (`INTERFACES`: a consumer refuses a
/// major version it does not know). A bay that cannot read `{"input": …}` entries refuses such
/// a record, instead of rewriting it without its `observed_versions` and `validated_candidate`.
pub const RECORD_SCHEMA_INPUTS: &str = "bay.change/2";

/// The schema `record` is written under: [`RECORD_SCHEMA_INPUTS`] when it declares a named
/// input, else [`RECORD_SCHEMA`], so a record without one stays readable by every bay.
#[must_use]
pub fn schema_for(record: &ChangeRecord) -> &'static str {
    if crate::inputs::names(&record.declared_inputs).is_empty() {
        RECORD_SCHEMA
    } else {
        RECORD_SCHEMA_INPUTS
    }
}

/// `record` as the pretty JSON a state commit holds, under [`schema_for`].
fn record_text(record: &ChangeRecord) -> Result<String, ChainError> {
    let mut value =
        serde_json::to_value(record).map_err(|error| ChainError::BadRecord(error.to_string()))?;
    value["schema"] = serde_json::Value::String(schema_for(record).to_owned());
    let mut json = serde_json::to_string_pretty(&value)
        .map_err(|error| ChainError::BadRecord(error.to_string()))?;
    json.push('\n');
    Ok(json)
}

/// The identity bay commits its own records under. They are unsigned until signing lands
/// (kickstart item 11); `--no-gpg-sign` keeps a user's `commit.gpgSign` from deciding that.
pub const RECORD_IDENTITY: &[(&str, &str)] = &[
    ("GIT_AUTHOR_NAME", "bay"),
    ("GIT_AUTHOR_EMAIL", "bay@bay.invalid"),
    ("GIT_COMMITTER_NAME", "bay"),
    ("GIT_COMMITTER_EMAIL", "bay@bay.invalid"),
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    /// The external reference: a card, issue or ticket id.
    pub reference: String,
    /// The revision of that reference (not a draft's `Revision-Id`).
    pub revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    pub name: String,
    pub session: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentClaim {
    pub hash: String,
    pub generation: u64,
}

impl IntentClaim {
    #[must_use]
    pub fn intent_ref(&self) -> String {
        format!("{}{}/{}", crate::names::INTENT, self.hash, self.generation)
    }

    #[must_use]
    pub fn landed_ref(&self) -> String {
        format!(
            "{}{}/{}",
            crate::names::LANDED_INTENT,
            self.hash,
            self.generation
        )
    }
}

/// The hash an intent claim is named by: lower-case hex sha256 of `<reference>@<revision>`,
/// 32 characters.
#[must_use]
pub fn intent_hash(reference: &str, revision: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{reference}@{revision}").as_bytes());
    crate::agent_settings::hex(&digest)[..32].to_owned()
}

/// Where an actor's name came from (G152): `BAY_ACTOR` (`env`), the user name in its place
/// (`user-default`), or neither (`default`). On an account many sessions share, a default
/// names every session the same actor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActorSource {
    Env,
    UserDefault,
    Default,
}

impl ActorSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::UserDefault => "user-default",
            Self::Default => "default",
        }
    }

    /// Whether the name came from a default rather than from `BAY_ACTOR`.
    #[must_use]
    pub const fn is_default(self) -> bool {
        !matches!(self, Self::Env)
    }
}

/// The acting identity: a stable name and one session (`DESIGN` § 3.1).
/// `BAY_ACTOR` names the actor and `BAY_SESSION` the session; without them the user name
/// and a fresh session id are used, so two sessions are two actors.
///
/// # Errors
///
/// The random source for a session id cannot be read.
pub fn current_actor() -> Result<Actor, String> {
    let name = actor_name();
    let session = match std::env::var("BAY_SESSION") {
        Ok(session) => session,
        Err(_) => format!(
            "s-{}",
            crate::ids::new_change_id().map_err(|error| error.to_string())?
        ),
    };
    Ok(Actor { name, session })
}

/// The acting identity's stable name: `BAY_ACTOR`, else the user name, else `agent`. A later
/// session of the same actor has this name and a session of its own.
#[must_use]
pub fn actor_name() -> String {
    named_actor().0
}

/// Where [`actor_name`] takes the name from (G152).
#[must_use]
pub fn actor_source() -> ActorSource {
    named_actor().1
}

/// The actor's name and its source. An empty variable names no one.
fn named_actor() -> (String, ActorSource) {
    let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    if let Some(name) = var("BAY_ACTOR") {
        (name, ActorSource::Env)
    } else if let Some(name) = var("USER") {
        (name, ActorSource::UserDefault)
    } else {
        ("agent".to_owned(), ActorSource::Default)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeState {
    Open,
    Invalidated,
    Repairing,
    Ready,
    Landed,
    Superseded,
    Abandoned,
    /// Its work is already on trunk: nothing was published (G213).
    NoEffect,
}

/// The change record of § 3.1. Fields of later increments are added when they are built.
/// `required_versions` and `coverage` are derived on read (`crate::inputs::judge`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRecord {
    pub schema: String,
    pub id: String,
    pub intent: Intent,
    pub actor: Actor,
    /// Where the actor's name came from when it took the change (G152); absent in a record
    /// written before it was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_source: Option<ActorSource>,
    /// The trunk commit at `start`. Written once, never moved.
    pub base_first: String,
    /// The trunk commit the current revision builds on.
    pub base: String,
    pub observed_remote: String,
    /// The declared paths, and the named inputs given to `start --input` (§ 3.1).
    pub declared_inputs: Vec<crate::inputs::DeclaredInput>,
    /// Paths the view fetched beyond its declared inputs, recorded at checkpoint and freeze (§ 10).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_inputs: Vec<String>,
    /// The version of each named input the actor read, written by `start` and `start --resume`
    /// at the change's base (§ 3.1, § 8). `required_versions` are derived on read, never stored.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub observed_versions: std::collections::BTreeMap<String, String>,
    /// The attempt last checked under the required versions (§ 3.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validated_candidate: Option<crate::inputs::Validated>,
    pub state: ChangeState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// The intent claim this change holds: `refs/bay/intent/<hash>/<generation>` (§ 4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<IntentClaim>,
    /// The landed change of the intent's previous generation, for `start --new` (§ 4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    /// The landed change this change undoes, for `bay undo` (§ 4): its `Reverts:` trailer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverts: Option<String>,
    /// The landed change this change fixes, for `start --fixes` (G161): its `Fixes:` trailer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixes: Option<String>,
    /// Actors this change was adopted from, oldest first (§ 4).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adopted_from: Vec<Actor>,
    /// `op_id → request digest → attempts → outcome` (§ 5, § 9).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<Operation>,
    /// The chain was cut by retention (§ 5): its head links only the landed revision, the
    /// published commit and the latest land request; superseded drafts may be collected.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retention_cut: bool,
    /// Outcome receipts synchronized from the machines that wrote them (§ 9).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub receipts: Vec<crate::receipts::Receipt>,
}

/// Operation states (`DESIGN` § 9a).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpState {
    Submitted,
    Admitted,
    Running,
    Checked,
    Published,
    Blocked,
    Failed,
    Superseded,
    /// The merged tree equals trunk's tree: the work is already on trunk, and nothing was
    /// published (G213).
    NoEffect,
}

impl OpState {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Published | Self::Failed | Self::Superseded | Self::NoEffect
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    /// `<change id>.<n>`; `n` is allocated by the compare-and-swap that appends the request.
    pub op_id: String,
    /// The frozen draft's commit id: the `Revision-Id`.
    pub revision: String,
    pub request_digest: String,
    /// The state commit that carries the request: the `Request-Id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub state: OpState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<Attempt>,
    /// The reason code of a blocked or failed outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// The executor's durable job handle, once an external executor admitted the operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    /// The condition a blocked operation was blocked on (revision, trunk, evidence), so an
    /// unchanged retry is refused without work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_on: Option<String>,
    /// The ownership epoch: one higher with every claim (§ 9a). Candidates carry it as
    /// `Bay-Epoch`; an executor whose epoch is not current never pushes trunk.
    #[serde(default)]
    pub epoch: u64,
    /// The executor that holds the current epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// How many publish pushes were refused because trunk moved.
    #[serde(default)]
    pub trunk_moved: u32,
    /// How many candidates were rechecked because the acceptance policy changed on trunk while
    /// they were checked (`POLICY_CHANGED`, § 7); these are not `trunk_moved` refusals.
    #[serde(default)]
    pub policy_changed: u32,
    /// Uncounted publish retries: pushes the trunk hook refused with `HOOK_TIMEOUT`, and
    /// releases the progress deadline created while this operation was publishing (§ 6). Five
    /// end the operation as `PUBLISH_RETRY_EXHAUSTED`.
    #[serde(default)]
    pub publish_retries: u32,
    /// The queue places whose deadline release came while this operation was publishing. Such a
    /// release is a publish retry and never counts toward `QUEUE_DEADLINE_EXCEEDED` (§ 6); the
    /// release ref is create-only and cannot say so itself, so the chain records it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mid_publish_releases: Vec<u64>,
    /// Set while the operation waits for a worker (`CHECK_UNAVAILABLE`, `DESIGN` § 6, § 7):
    /// what no worker answered for. Cleared once a check of it runs to an answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<Unavailable>,
}

/// Why an operation waits for a worker, and since when (`DESIGN` § 6, § 7).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unavailable {
    /// The required environments no worker answered for, sorted.
    pub environments: Vec<String>,
    /// The check adapter's reason, when it answered `unavailable`; absent when the box that
    /// ran the operation does not serve those environments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    /// When it was recorded, in Unix milliseconds: the clock of the rejoin gap after an
    /// adapter's answer.
    pub at_ms: u64,
    /// How many adapter answers in a row before this one: the gap doubles with each.
    #[serde(default)]
    pub repeats: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    /// `<op id>.<attempt>`: the `Candidate-Id`.
    pub candidate_id: String,
    pub base: String,
    pub commit: String,
    pub tree: String,
    /// `pass`, `product_failure`, `infra_failure`, `unknown` or `flaky`; `unchecked` when every
    /// judging policy declares `unchecked = true`, so no check ran (never a pass).
    pub verdict: String,
    /// The epoch the candidate was built under; only the current epoch may publish it.
    #[serde(default)]
    pub epoch: u64,
}

#[derive(Debug)]
pub enum ChainError {
    Git(GitError),
    /// `create` found the change ref already present.
    Exists,
    /// The chain head is not the value the caller read; nothing was written.
    Moved {
        expected: String,
        actual: Option<String>,
    },
    NotFound,
    BadRecord(String),
}

impl From<GitError> for ChainError {
    fn from(error: GitError) -> Self {
        Self::Git(error)
    }
}

impl ChainError {
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Git(error) => error.message(),
            Self::Exists => "the change ref already exists".to_owned(),
            Self::Moved { expected, actual } => format!(
                "the change chain moved: expected {expected}, found {}",
                actual.as_deref().unwrap_or("nothing")
            ),
            Self::NotFound => "no such change".to_owned(),
            Self::BadRecord(why) => format!("bad change record: {why}"),
        }
    }
}

#[must_use]
pub fn change_ref(id: &str) -> String {
    format!("{CHANGES_PREFIX}{id}")
}

/// Operations on change chains in one repository.
pub struct Chain<'a> {
    git: &'a Git,
    /// Signs every state commit this chain writes, when set (§ 10a).
    signer: Option<Signer>,
}

impl<'a> Chain<'a> {
    #[must_use]
    pub const fn new(git: &'a Git) -> Self {
        Self { git, signer: None }
    }

    /// Signs the state commits this chain writes from now on.
    pub fn sign_with(&mut self, signer: Option<Signer>) {
        self.signer = signer;
    }

    /// Creates `refs/bay/changes/<id>` with its first state commit. Create-only.
    ///
    /// # Errors
    ///
    /// [`ChainError::Exists`] when the ref is already there; a git fault otherwise.
    pub fn create(
        &self,
        record: &ChangeRecord,
        draft: &str,
        message: &str,
    ) -> Result<String, ChainError> {
        let commit = self.state_commit(record, None, draft, None, message)?;
        let name = change_ref(&record.id);
        let command = format!("create {name} {commit}\n");
        match self
            .git
            .run_with(&["update-ref", "--stdin"], &[], Some(command.as_bytes()))
        {
            Ok(_) => Ok(commit),
            Err(GitError::Failed { .. }) if self.git.ref_target(&name)?.is_some() => {
                Err(ChainError::Exists)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Appends one state commit, compare-and-swap on the head the caller read.
    ///
    /// # Errors
    ///
    /// [`ChainError::Moved`] when the head is no longer `expected_head`; nothing is written.
    pub fn append(
        &self,
        expected_head: &str,
        record: &ChangeRecord,
        draft: &str,
        message: &str,
    ) -> Result<String, ChainError> {
        self.append_linking(expected_head, record, draft, None, message)
    }

    /// [`Chain::append`], also linking `candidate` so it stays reachable before it is on trunk.
    ///
    /// # Errors
    ///
    /// See [`Chain::append`].
    pub fn append_linking(
        &self,
        expected_head: &str,
        record: &ChangeRecord,
        draft: &str,
        candidate: Option<&str>,
        message: &str,
    ) -> Result<String, ChainError> {
        let commit = self.state_commit(record, Some(expected_head), draft, candidate, message)?;
        let name = change_ref(&record.id);
        let command = format!("update {name} {commit} {expected_head}\n");
        match self
            .git
            .run_with(&["update-ref", "--stdin"], &[], Some(command.as_bytes()))
        {
            Ok(_) => Ok(commit),
            Err(GitError::Failed { .. }) => {
                let actual = self.git.ref_target(&name)?;
                if actual.as_deref() == Some(expected_head) {
                    Err(ChainError::Git(GitError::UnexpectedOutput(
                        "update-ref failed with the head unchanged",
                    )))
                } else {
                    Err(ChainError::Moved {
                        expected: expected_head.to_owned(),
                        actual,
                    })
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    /// The chain head, or `None` when the change does not exist here.
    ///
    /// # Errors
    ///
    /// A git fault.
    pub fn head(&self, id: &str) -> Result<Option<String>, ChainError> {
        Ok(self.git.ref_target(&change_ref(id))?)
    }

    /// The head and the record it carries.
    ///
    /// # Errors
    ///
    /// [`ChainError::NotFound`], a git fault, or a record that does not parse.
    pub fn read(&self, id: &str) -> Result<(String, ChangeRecord), ChainError> {
        let head = self.head(id)?.ok_or(ChainError::NotFound)?;
        let record = self.record_at(&head)?;
        Ok((head, record))
    }

    /// The record carried by one state commit.
    ///
    /// # Errors
    ///
    /// A git fault, or a record that does not parse.
    pub fn record_at(&self, state: &str) -> Result<ChangeRecord, ChainError> {
        let text = self
            .git
            .run(&["cat-file", "blob", &format!("{state}:{RECORD_FILE}")])?;
        let record: ChangeRecord = serde_json::from_str(&text)
            .map_err(|error| ChainError::BadRecord(error.to_string()))?;
        if record.schema != schema_for(&record) {
            return Err(ChainError::BadRecord(format!(
                "schema {} for a record {} named inputs",
                record.schema,
                if schema_for(&record) == RECORD_SCHEMA {
                    "without"
                } else {
                    "with"
                }
            )));
        }
        Ok(record)
    }

    /// Every draft the chain links, newest first.
    ///
    /// # Errors
    ///
    /// [`ChainError::NotFound`] or a git fault.
    pub fn drafts(&self, id: &str) -> Result<Vec<String>, ChainError> {
        let mut state = self.head(id)?.ok_or(ChainError::NotFound)?;
        let mut drafts = Vec::new();
        loop {
            let parents = self.parents(&state)?;
            if self
                .record_at(&state)
                .is_ok_and(|record| record.retention_cut)
            {
                // A cut head is a root: its first parent is the landed revision.
                if let Some(revision) = parents.first() {
                    drafts.push(revision.clone());
                }
                return Ok(drafts);
            }
            match parents.as_slice() {
                [draft] => {
                    drafts.push(draft.clone());
                    return Ok(drafts);
                }
                [previous, draft] | [previous, draft, _] => {
                    drafts.push(draft.clone());
                    state.clone_from(previous);
                }
                _ => {
                    return Err(ChainError::BadRecord(format!(
                        "state commit {state} has {} parents",
                        parents.len()
                    )));
                }
            }
        }
    }

    /// The land request: an agent-signed commit whose tree holds `request.json` and whose one
    /// parent is the revision it asks to land. It is linked into the chain and named by
    /// `refs/bay/requests/<id>`, so a retention cut can keep it without keeping the chain.
    ///
    /// # Errors
    ///
    /// A git fault.
    pub fn request_commit(
        &self,
        request: &serde_json::Value,
        revision: &str,
        message: &str,
    ) -> Result<String, ChainError> {
        let mut json = serde_json::to_string_pretty(request)
            .map_err(|error| ChainError::BadRecord(error.to_string()))?;
        json.push('\n');
        let blob = self.git.run_with(
            &["hash-object", "-w", "--stdin"],
            &[],
            Some(json.as_bytes()),
        )?;
        let entry = format!("100644 blob {blob}\trequest.json\n");
        let tree = self
            .git
            .run_with(&["mktree"], &[], Some(entry.as_bytes()))?;
        self.commit(&tree, &[revision], message)
    }

    /// The retention cut (§ 5): replaces the chain with one head linking the landed revision,
    /// the published commit and the latest land request, compare-and-swap on `expected_head`.
    ///
    /// # Errors
    ///
    /// [`ChainError::Moved`] when the head moved; a git fault.
    pub fn cut(
        &self,
        expected_head: &str,
        record: &ChangeRecord,
        links: &[&str],
        message: &str,
    ) -> Result<String, ChainError> {
        let json = record_text(record)?;
        let blob = self.git.run_with(
            &["hash-object", "-w", "--stdin"],
            &[],
            Some(json.as_bytes()),
        )?;
        let entry = format!("100644 blob {blob}\t{RECORD_FILE}\n");
        let tree = self
            .git
            .run_with(&["mktree"], &[], Some(entry.as_bytes()))?;
        let commit = self.commit(&tree, links, message)?;
        let name = change_ref(&record.id);
        let command = format!("update {name} {commit} {expected_head}\n");
        match self
            .git
            .run_with(&["update-ref", "--stdin"], &[], Some(command.as_bytes()))
        {
            Ok(_) => Ok(commit),
            Err(GitError::Failed { .. }) => Err(ChainError::Moved {
                expected: expected_head.to_owned(),
                actual: self.git.ref_target(&name)?,
            }),
            Err(error) => Err(error.into()),
        }
    }

    /// A commit of `tree` on `parents`, signed when this chain has a signer.
    fn commit(&self, tree: &str, parents: &[&str], message: &str) -> Result<String, ChainError> {
        let mut args: Vec<String> = Vec::new();
        if let Some(signer) = &self.signer {
            args.extend(signer.config_args());
        }
        args.extend(["commit-tree".to_owned(), tree.to_owned()]);
        for parent in parents {
            args.extend(["-p".to_owned(), (*parent).to_owned()]);
        }
        args.push(
            if self.signer.is_some() {
                "-S"
            } else {
                "--no-gpg-sign"
            }
            .to_owned(),
        );
        args.extend(["-m".to_owned(), message.to_owned()]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let identity: Vec<(String, String)> = self.signer.as_ref().map_or_else(
            || {
                RECORD_IDENTITY
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                    .collect()
            },
            Signer::identity,
        );
        let envs: Vec<(&str, &str)> = identity
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        Ok(self.git.run_with(&args, &envs, None)?)
    }

    fn parents(&self, commit: &str) -> Result<Vec<String>, ChainError> {
        let line = self
            .git
            .run(&["rev-list", "--parents", "-n", "1", commit])?;
        Ok(line.split_whitespace().skip(1).map(str::to_owned).collect())
    }

    fn state_commit(
        &self,
        record: &ChangeRecord,
        previous: Option<&str>,
        draft: &str,
        candidate: Option<&str>,
        message: &str,
    ) -> Result<String, ChainError> {
        let json = record_text(record)?;
        let blob = self.git.run_with(
            &["hash-object", "-w", "--stdin"],
            &[],
            Some(json.as_bytes()),
        )?;
        let entry = format!("100644 blob {blob}\t{RECORD_FILE}\n");
        let tree = self
            .git
            .run_with(&["mktree"], &[], Some(entry.as_bytes()))?;
        let mut args: Vec<String> = Vec::new();
        if let Some(signer) = &self.signer {
            args.extend(signer.config_args());
        }
        args.extend(["commit-tree".to_owned(), tree.clone()]);
        if let Some(previous) = previous {
            args.extend(["-p".to_owned(), previous.to_owned()]);
        }
        args.extend(["-p".to_owned(), draft.to_owned()]);
        if let Some(candidate) = candidate {
            args.extend(["-p".to_owned(), candidate.to_owned()]);
        }
        args.push(
            if self.signer.is_some() {
                "-S"
            } else {
                "--no-gpg-sign"
            }
            .to_owned(),
        );
        args.extend(["-m".to_owned(), message.to_owned()]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match &self.signer {
            Some(signer) => {
                let identity = signer.identity();
                let envs: Vec<(&str, &str)> = identity
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str()))
                    .collect();
                Ok(self.git.run_with(&args, &envs, None)?)
            }
            None => Ok(self.git.run_with(&args, RECORD_IDENTITY, None)?),
        }
    }
}

/// Freezes a view's work into a draft commit on `base`: every edit in the view, staged
/// through the view's own index (sparse-aware), written as one commit. The view keeps
/// its index; nothing outside the view changes.
///
/// # Errors
///
/// A git fault.
pub fn freeze_draft(view: &Git, base: &str, message: &str) -> Result<String, GitError> {
    view.run(&["add", "--all", "--sparse"])?;
    let tree = view.run(&["write-tree"])?;
    view.run_with(
        &["commit-tree", &tree, "-p", base, "-m", message],
        RECORD_IDENTITY,
        None,
    )
}

/// An unsigned commit of `tree` on `parent` under bay's record identity.
///
/// # Errors
///
/// A git fault.
pub fn commit_on(git: &Git, tree: &str, parent: &str, message: &str) -> Result<String, GitError> {
    git.run_with(
        &[
            "commit-tree",
            tree,
            "-p",
            parent,
            "--no-gpg-sign",
            "-m",
            message,
        ],
        RECORD_IDENTITY,
        None,
    )
}
