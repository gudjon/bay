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
/// The one schema a change record is written under (0.4.0, BACKLOG 53 D): a named input, a
/// lane and dependencies are optional fields of it. A bay before 0.4.0 refuses a `/1` record
/// that holds one of them (it expected `/2`, `/3` or `/4` there), so it never misreads one.
pub const RECORD_SCHEMA: &str = "bay.change/1";
/// The schemas a bay before 0.4.0 wrote a record with a named input, a lane or dependencies
/// under: a stored one is still read.
pub const STORED_SCHEMAS: &[&str] = &["bay.change/2", "bay.change/3", "bay.change/4"];

/// `record` as the pretty JSON a state commit holds, under [`RECORD_SCHEMA`].
fn record_text(record: &ChangeRecord) -> Result<String, ChainError> {
    let mut value =
        serde_json::to_value(record).map_err(|error| ChainError::BadRecord(error.to_string()))?;
    value["schema"] = serde_json::Value::String(RECORD_SCHEMA.to_owned());
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
    /// What the work is, in a few words (or a card id).
    pub reference: String,
    /// The revision of that reference (not a draft's `Revision-Id`).
    pub revision: String,
}

/// The longest an intent is shown in a sentence, in characters; a longer one is cut there.
const SHOWN_INTENT: usize = 60;

/// How a sentence names change `id`: its intent first, the name people know it by, and the id
/// a command takes in parentheses (owner ruling 2026-10-27). An intent that is empty or the id
/// itself shows the id alone.
#[must_use]
pub fn shown(intent: &str, id: &str) -> String {
    let intent = intent.trim();
    if intent.is_empty() || intent == id {
        return id.to_owned();
    }
    let mut short: String = intent.chars().take(SHOWN_INTENT).collect();
    if intent.chars().count() > SHOWN_INTENT {
        short.push('…');
    }
    format!("{short} ({id})")
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
pub fn current_actor(common: &std::path::Path) -> Result<Actor, String> {
    let name = actor_name();
    // BACKLOG 66 C: with no `BAY_SESSION`, the session a hook named for the agent running this
    // command (`crate::agent_pid`), else a fresh one.
    let session = match std::env::var("BAY_SESSION") {
        Ok(session) => session,
        Err(_) => match crate::agent_pid::session_of_caller(common) {
            Some(session) => session,
            None => format!(
                "s-{}",
                crate::ids::new_change_id().map_err(|error| error.to_string())?
            ),
        },
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
    /// The previous generation's change of the intent: landed, or ended without landing (§ 4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    /// The landed change this change undoes, for `bay undo` (§ 4): its `Reverts:` trailer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverts: Option<String>,
    /// The landed change, or the post check, this change fixes, for `start --fixes` (G161): its
    /// `Fixes:` trailer.
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
    /// The sandbox lane this change was started in (`bay start --lane`, NEW-4): its checks run,
    /// and it never lands. Written once at start, never changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    /// The branch made outside bay this change was started from (`bay start --from`, G268):
    /// its first draft is that branch's net change on the base. Written once at start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<StartedFrom>,
    /// The changes this one lands only after (`bay start --after`, G273): the executor publishes
    /// it once every one of them has landed on trunk, and its landing carries one `Bay-After:`
    /// trailer per change. Written at start.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
}

/// The branch made outside bay a change took in (`bay start --from`, G268): the ref as the
/// actor named it, and the commit it named then.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartedFrom {
    #[serde(rename = "ref")]
    pub reference: String,
    pub commit: String,
    /// Where `commit` was read (N-5): `remote`, the remote's tip of that branch or ref, which is
    /// taken whenever the remote has one; or `local`, this repository's, only when it has none.
    /// Absent in a record written before bay 0.2.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl ChangeRecord {
    /// What this change names as repaired: its `Fixes:`, `Reverts:` and `Supersedes:` (a red
    /// post check is answered by these, `crate::red::repairs`).
    #[must_use]
    pub fn repair_names(&self) -> Vec<&String> {
        [&self.fixes, &self.reverts, &self.supersedes]
            .into_iter()
            .flatten()
            .collect()
    }

    /// Whether operation `op_id` is finished for good: terminal, or gone from a chain the
    /// retention cut closed (§ 5), so it is never run or counted again. A blocked operation
    /// may still be repaired, so it is not.
    #[must_use]
    pub fn finished(&self, op_id: &str) -> bool {
        self.operations
            .iter()
            .find(|op| op.op_id == op_id)
            .map_or(self.retention_cut, |op| op.state.is_terminal())
    }
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
    /// The state an operation under way moves to next (G80); `None` for one that waits on a
    /// repair (its refusal says which) or has ended.
    #[must_use]
    pub const fn next(self) -> Option<&'static str> {
        match self {
            Self::Submitted => Some("admitted"),
            Self::Admitted => Some("running"),
            Self::Running => Some("checked"),
            Self::Checked => Some("published"),
            Self::Blocked | Self::Published | Self::Failed | Self::Superseded | Self::NoEffect => {
                None
            }
        }
    }

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
    /// end the operation as `MERGE_RETRY_EXHAUSTED`.
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
    /// Set while the operation waits behind the queue's head and its revision does not merge
    /// onto trunk (G177): what the executor's early probe found. The operation keeps its
    /// place; its own merge, once it is head, still decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub early_conflict: Option<EarlyConflict>,
    /// NEW-2, NEW-10: the checks an executor that was not running this operation made of it
    /// while it waited behind the queue's head (`crate::throughput`), newest last, at most
    /// `throughput::PRECHECKS_KEPT`. Their records count only through reuse.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prechecks: Vec<Precheck>,
    /// Batching (`crate::throughput`): the batch whose bisection found this operation the first
    /// member that fails. It then runs alone when it is head, judged by its own candidate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_culprit: Option<String>,
}

/// One pre-check of a queued operation (`crate::throughput`): the base it was checked on (a
/// predicted commit or the trunk tip, never published), its tree, the merged tree and the
/// verdict. Nothing about it is an attempt: it is never published.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Precheck {
    pub base: String,
    pub base_tree: String,
    pub tree: String,
    pub verdict: String,
    /// When it was recorded, in Unix milliseconds.
    pub at_ms: u64,
}

/// A conflict between a queued operation's revision and trunk, found before it is head (G177).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EarlyConflict {
    /// The trunk tip the revision does not merge onto.
    pub trunk: String,
    /// The conflicting paths, as the publication merge names them (`DESIGN` § 6 step 2).
    pub paths: Vec<String>,
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
    /// Set when the candidate lands as a member of a batch (`crate::throughput`): its own tree
    /// was not checked alone, and its evidence is the batch tip's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<BatchMember>,
}

/// A batch member's attempt (`crate::throughput`): the batch, the member's place in it, and
/// what was checked: the tip's candidate and tree, on the batch's base, the trunk commit its
/// first member was merged onto.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchMember {
    pub id: String,
    pub position: usize,
    pub size: usize,
    pub tip: String,
    pub tree: String,
    pub base: String,
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
    /// G563: the ref `refs/bay/changes/<id>` holds the record of change `of`, a copy bay never
    /// wrote.
    Copy {
        id: String,
        of: String,
    },
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
            Self::Copy { id, of } => format!(
                "{} holds the record of change {of}: a copy bay never wrote, so it is not a \
                 change; `git push <remote> :{}` removes it",
                change_ref(id),
                change_ref(id)
            ),
        }
    }
}

#[must_use]
pub fn change_ref(id: &str) -> String {
    format!("{CHANGES_PREFIX}{id}")
}

/// The record a state commit carries, from the text of its record file.
///
/// # Errors
///
/// A record that does not parse, or one under a schema no bay wrote a change record with.
fn parse_record(text: &str) -> Result<ChangeRecord, ChainError> {
    let record: ChangeRecord =
        serde_json::from_str(text).map_err(|error| ChainError::BadRecord(error.to_string()))?;
    if record.schema != RECORD_SCHEMA && !STORED_SCHEMAS.contains(&record.schema.as_str()) {
        return Err(ChainError::BadRecord(format!(
            "schema {} is not a change record's",
            record.schema
        )));
    }
    Ok(record)
}

/// One chain as [`Chain::read_all`] found it.
#[derive(Clone, Debug)]
pub struct HeldChain {
    /// The change id its ref names.
    pub id: String,
    /// The chain head.
    pub head: String,
    /// The head's committer time, in Unix seconds (0 when Git gave none).
    pub committed: u64,
    pub record: ChangeRecord,
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
    /// [`ChainError::NotFound`], a git fault, a record that does not parse, or a record of
    /// another change: G563 (a fleet finding, 2026-10-29: `refs/bay/changes/hvvrfutj-r` held a
    /// copy of `hvvrfutj`'s chain, so it took a live place, and `bay abandon hvvrfutj-r` wrote to
    /// `hvvrfutj` and failed). bay names each chain by its record's id, so a ref of any other name
    /// is a copy, never a change.
    pub fn read(&self, id: &str) -> Result<(String, ChangeRecord), ChainError> {
        let head = self.head(id)?.ok_or(ChainError::NotFound)?;
        let record = self.record_at(&head)?;
        if record.id != id {
            return Err(ChainError::Copy {
                id: id.to_owned(),
                of: record.id,
            });
        }
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
        parse_record(&text)
    }

    /// G200b: every chain this repository holds, in ref order, read by one `for-each-ref` and
    /// one `git cat-file --batch` of their records, however many chains there are. A record
    /// the batch cannot give is read by [`Chain::read`] on its own, and a chain that cannot be
    /// read is left out, as a caller of [`Chain::read`] per chain would skip it.
    ///
    /// # Errors
    ///
    /// A git fault listing the chains or reading the batch.
    pub fn read_all(&self) -> Result<Vec<HeldChain>, ChainError> {
        let listed = self.git.run(&[
            "for-each-ref",
            "--format=%(objectname) %(committerdate:unix) %(refname)",
            CHANGES_PREFIX,
        ])?;
        let heads: Vec<(String, u64, String)> = listed
            .lines()
            .filter_map(|line| {
                let mut words = line.splitn(3, ' ');
                let head = words.next()?.to_owned();
                let committed: u64 = words.next()?.parse().unwrap_or(0);
                let id = words.next()?.strip_prefix(CHANGES_PREFIX)?.to_owned();
                Some((head, committed, id))
            })
            .collect();
        let names: Vec<String> = heads
            .iter()
            .map(|(head, _, _)| format!("{head}:{RECORD_FILE}"))
            .collect();
        let texts = self.git.objects(&names)?;
        Ok(heads
            .into_iter()
            .zip(texts)
            .filter_map(|((head, committed, id), text)| {
                let parsed = text
                    .and_then(|text| String::from_utf8(text).ok())
                    .and_then(|text| parse_record(&text).ok());
                // G563: a copy of another change's chain is left out, as [`Chain::read`] refuses it.
                let (head, record) = match parsed {
                    Some(record) if record.id == id => (head, record),
                    _ => self.read(&id).ok()?,
                };
                Some(HeldChain {
                    id,
                    head,
                    committed,
                    record,
                })
            })
            .collect())
    }

    /// The chain's newest draft: what [`Chain::drafts`] lists first, read from the head state
    /// alone. A long-lived chain holds thousands of states (heartbeats), and `bay status` reads
    /// the newest draft of every view (a fleet finding, 2026-10-26: 43 views, 197 s).
    ///
    /// # Errors
    ///
    /// [`ChainError::NotFound`] or a git fault.
    pub fn latest_draft(&self, id: &str) -> Result<String, ChainError> {
        let head = self.head(id)?.ok_or(ChainError::NotFound)?;
        let line = self.git.run(&["rev-list", "--parents", "-n", "1", &head])?;
        let parents: Vec<&str> = line.split_whitespace().skip(1).collect();
        if self
            .record_at(&head)
            .is_ok_and(|record| record.retention_cut)
        {
            // A cut head is a root: its first parent is the landed revision.
            if let Some(revision) = parents.first() {
                return Ok((*revision).to_owned());
            }
        }
        match parents.as_slice() {
            [draft] | [_, draft] | [_, draft, _] => Ok((*draft).to_owned()),
            _ => Err(ChainError::BadRecord(format!(
                "state commit {head} has {} parents",
                parents.len()
            ))),
        }
    }

    /// Every draft the chain links, newest first.
    ///
    /// # Errors
    ///
    /// [`ChainError::NotFound`] or a git fault.
    pub fn drafts(&self, id: &str) -> Result<Vec<String>, ChainError> {
        // One state at a time, never `rev-list --first-parent` of the head: past the chain's
        // first state that walk runs on through the trunk's whole history (0.5.6 read some
        // 17,000 commits per chain that way, and `bay status` took minutes).
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

/// The base a view really stands on, when it is newer than the one its change records: the
/// newest commit the view's `HEAD` shares with `trunk`, if `recorded` is behind it (fleet
/// findings, 2026-10-27: a refresh's new base was recorded only in a chain that was then set
/// aside, and a hand merge of trunk into a view moved it too; either way the change's draft
/// against the recorded base counted other changes' work as its own, and every refresh
/// conflicted on files it never touched). `None` when the view stands on the recorded base.
#[must_use]
pub fn newer_base(view: &Git, recorded: &str, trunk: &str) -> Option<String> {
    let head = view.run(&["rev-parse", "--verify", "HEAD"]).ok()?;
    let shared = view.run(&["merge-base", &head, trunk]).ok()?;
    (shared != recorded && view.is_ancestor(recorded, &shared).unwrap_or(false)).then_some(shared)
}

/// Freezes a view's work into a draft commit on `base`: every edit in the view, staged
/// through the view's own index (sparse-aware), written as one commit. The view keeps
/// its index; nothing outside the view changes. BACKLOG 58: when the agent committed in the
/// view (its `HEAD` is past `base`), the draft carries the latest of those commits' message,
/// so main's commit can say what the agent said ([`agent_message`]); else `message`.
///
/// # Errors
///
/// A git fault.
pub fn freeze_draft(view: &Git, base: &str, message: &str) -> Result<String, GitError> {
    view.run(&["add", "--all", "--sparse"])?;
    let tree = view.run(&["write-tree"])?;
    let head = view.run(&["rev-parse", "HEAD"])?;
    let own = if head != base && view.is_ancestor(base, &head).unwrap_or(false) {
        Some(view.run(&["log", "-1", "--format=%B", "HEAD"])?)
    } else {
        None
    };
    let message = own
        .as_deref()
        .filter(|own| !own.trim().is_empty())
        .unwrap_or(message);
    view.run_with(
        &["commit-tree", &tree, "-p", base, "-m", message],
        RECORD_IDENTITY,
        None,
    )
}

/// Whether drafts `a` and `b` say the same: the same tree and the same message, so an amend
/// that changes only the message is a new draft (BACKLOG 58: main's commit says it).
///
/// # Errors
///
/// A git fault.
pub fn same_draft(git: &Git, a: &str, b: &str) -> Result<bool, GitError> {
    let read = |commit: &str| git.run(&["log", "-1", "--format=%T%x00%B", commit]);
    Ok(read(a)? == read(b)?)
}

/// `commit_on` with the author of `author` (a commit of a branch made outside bay), so a
/// draft taken in keeps who wrote it; bay stays the committer.
///
/// # Errors
///
/// A git fault.
pub fn commit_on_by(
    git: &Git,
    tree: &str,
    parent: &str,
    message: &str,
    author: &str,
) -> Result<String, GitError> {
    let who = git.run(&[
        "log",
        "-1",
        "--format=%an%x00%ae%x00%ad",
        "--date=raw",
        author,
    ])?;
    let mut parts = who.splitn(3, '\0');
    let (name, email, date) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    let mut envs: Vec<(&str, &str)> = RECORD_IDENTITY
        .iter()
        .filter(|(key, _)| key.starts_with("GIT_COMMITTER"))
        .copied()
        .collect();
    envs.extend([
        ("GIT_AUTHOR_NAME", name),
        ("GIT_AUTHOR_EMAIL", email),
        ("GIT_AUTHOR_DATE", date),
    ]);
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
        &envs,
        None,
    )
}

/// Whether `email` is bay's own record identity: a draft authored by it names no person.
#[must_use]
pub fn is_record_author(email: &str) -> bool {
    RECORD_IDENTITY
        .iter()
        .any(|(key, value)| *key == "GIT_AUTHOR_EMAIL" && *value == email)
}

/// The messages bay itself gives a draft: none of them is the agent's.
const BAY_DRAFT_MESSAGES: &[&str] = &["draft", "checkpoint"];

/// The trailers bay writes on main's commit: an agent's message never sets one.
const BAY_TRAILERS: &[&str] = &[
    "change-id",
    "revision-id",
    "request-id",
    "candidate-id",
    "registry-id",
    "bay-epoch",
    "bay-session",
    "supersedes",
    "reverts",
    "fixes",
    "bay-after",
    "bay-batch",
];

/// BACKLOG 58 (owner ruling 2026-10-23): the agent's own words in draft `revision`'s message,
/// as main's commit carries them: its text (subject and body) and its trailers (such as
/// `Open:`), without any trailer bay writes itself (`reserved`, plus the intent and actor
/// keys). `None` for a message bay gave the draft. bay never judges the words.
///
/// # Errors
///
/// A git fault.
pub fn agent_message(
    git: &Git,
    revision: &str,
    reserved: &[&str],
) -> Result<Option<(String, Vec<String>)>, GitError> {
    let message = git.run(&["log", "-1", "--format=%B", revision])?;
    let message = message.trim();
    if message.is_empty()
        || BAY_DRAFT_MESSAGES.contains(&message)
        || message.starts_with("bay save ")
    {
        return Ok(None);
    }
    let is_trailer = |line: &str| {
        line.split_once(": ").is_some_and(|(key, _)| {
            !key.is_empty()
                && key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    };
    let (text, trailers) = match message.rsplit_once("\n\n") {
        Some((text, last)) if last.lines().all(is_trailer) => (text.trim(), last),
        _ if message.lines().all(is_trailer) => ("", message),
        _ => (message, ""),
    };
    let kept: Vec<String> = trailers
        .lines()
        .filter(|line| {
            let key = line
                .split_once(": ")
                .map_or("", |(key, _)| key)
                .to_ascii_lowercase();
            !BAY_TRAILERS.contains(&key.as_str())
                && !reserved.iter().any(|own| own.eq_ignore_ascii_case(&key))
        })
        .map(str::to_owned)
        .collect();
    if text.is_empty() && kept.is_empty() {
        return Ok(None);
    }
    Ok(Some((text.to_owned(), kept)))
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

#[cfg(test)]
mod shown_tests {
    use super::shown;

    #[test]
    fn a_change_is_named_by_its_intent_then_its_id() {
        assert_eq!(
            shown("CARD-7 the ball lane", "abcd1234"),
            "CARD-7 the ball lane (abcd1234)"
        );
        assert_eq!(shown("", "abcd1234"), "abcd1234");
        assert_eq!(shown("abcd1234", "abcd1234"), "abcd1234");
        let long = "x".repeat(80);
        assert_eq!(
            shown(&long, "abcd1234"),
            format!("{}… (abcd1234)", "x".repeat(60))
        );
    }
}
