use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, CommandFactory, Parser, Subcommand};

use bay::result::BayResult;

#[derive(Parser)]
#[command(version = bay::version::BAY, about, disable_help_subcommand = true)]
struct Cli {
    /// Run as if bay was started in <PATH> (like `git -C`)
    #[arg(short = 'C', global = true, value_name = "PATH", default_value = ".")]
    dir: PathBuf,

    /// Print only the `bay.result/1` document, with no human text on stderr
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Open a change: a fresh main, a sparse view of the declared paths, the change record;
    /// or, named by its id, pick up an existing change: checkpoint its draft, bring in newer main
    Start {
        /// What this work is, in a few words (or a card id), or an existing change's id to pick
        /// it up
        intent: String,
        /// The paths this change reads and writes (its declared inputs)
        #[arg(long, num_args = 1..)]
        paths: Vec<String>,
        /// A named input this change depends on, `<kind>:<name>` (repeatable)
        #[arg(long = "input", value_name = "KIND:NAME", value_parser = bay::inputs::InputName::parse)]
        inputs: Vec<bay::inputs::InputName>,
        /// After an abandon, claim the intent's next generation instead of adopting the abandoned
        /// change (after a merge, plain start already does)
        #[arg(long)]
        new: bool,
        /// The merged change this one fixes; its merge carries a Fixes: trailer
        #[arg(long, value_name = "CHANGE")]
        fixes: Option<String>,
        /// A sandbox lane of the config: the change's checks run, and it never merges
        #[arg(long, value_name = "LANE")]
        lane: Option<String>,
        /// A branch made outside bay (a branch name or a full ref): its net change is the draft
        #[arg(long, value_name = "REF")]
        from: Option<String>,
        /// A change this one merges only after (repeatable): it merges once that change merged
        #[arg(long, value_name = "CHANGE")]
        after: Vec<String>,
    },
    /// Freeze the draft, build the candidate, check it, merge it into main
    Merge {
        /// The change (default: the change of the view bay runs in)
        change: Option<String>,
        #[command(flatten)]
        wait: WaitArgs,
    },
    /// Give a change up: any actor may adopt it
    Abandon {
        /// The change (default: the change of the view bay runs in)
        change: Option<String>,
    },
    /// Merge the edits made in the main checkout as one change: start, merge --wait, then move
    /// the checkout to the new tip of main. Never in a view (use merge there)
    Save {
        /// What this work is, in a few words (or a card id)
        intent: String,
        /// The first draft's commit message, kept in the change's chain (main keeps bay's own)
        #[arg(long)]
        message: Option<String>,
    },
    /// The changes waiting for an approval this machine's approver (`bay.approverName`) can give,
    /// each with its candidate, paths and diff; with no approver set, every one, with who can
    /// approve it. Also each held candidate's waivable checks that did not pass
    Waiting,
    /// Approve a change that waits for an approval: this machine's approver signs one
    /// `approval` record of its exact candidate for each approval check it waits on, and
    /// pushes it; with --waive, one signed waiver of one waivable check instead
    Approve {
        /// The change that waits for the approval
        change: String,
        /// Waive this one check, marked `waivable = true`, on the pending candidate: a signed
        /// waiver, never a pass; the merge names it as a coverage gap. Needs --why
        #[arg(long, value_name = "CHECK")]
        waive: Option<String>,
        /// Why, in the approver's words: kept in the signed record
        #[arg(long, value_name = "TEXT")]
        why: Option<String>,
    },
    /// Say no to a change that waits for an approval: this machine's approver signs a
    /// `product_failure` record of its exact candidate, and the change is blocked
    Reject {
        /// The change that waits for the approval
        change: String,
        /// Why, in the approver's words: kept in the signed record
        #[arg(long, value_name = "TEXT")]
        why: Option<String>,
    },
    /// A new change that undoes one merge, checked and merged like any other; never a rewrite
    Undo {
        /// The merged change to undo
        change: String,
        #[command(flatten)]
        wait: WaitArgs,
    },
    /// Current state, derived on read from main and refs
    Status {
        /// One change (default: the view's change, else every local change)
        change: Option<String>,
        /// Every machine and runner of the repository, from the local cache `bay status` fetches;
        /// fails closed without that fetch
        #[arg(long, conflicts_with = "change")]
        all: bool,
        /// The Reverts:, Supersedes: and Fixes: pairs of a first-parent range (default: main);
        /// local refs only, never a fetch
        #[arg(long, value_name = "RANGE", num_args = 0..=1, default_missing_value = "",
            conflicts_with_all = ["change", "all"])]
        lineage: Option<String>,
        /// Every branch on the remote, classed `main`, `target`, `lane`, `merged` (with its proof),
        /// stale or unique, and the local branches no remote holds and worktrees whose folder
        /// is gone; one ls-remote and one fetch, nothing deleted
        #[arg(long, conflicts_with_all = ["change", "all", "lineage"])]
        branches: bool,
        /// With --branches: a branch with commits off main whose tip is older than this is stale
        /// (default 30)
        #[arg(long, value_name = "DAYS", requires = "branches")]
        stale_days: Option<u64>,
        /// What happened after a cursor: merges into main, log events, new invalidations of the
        /// view's change; with no cursor, from the start. Each answer names the next cursor
        /// (`detail.cursor`)
        #[arg(long, value_name = "CURSOR", num_args = 0..=1, default_missing_value = "",
            value_parser = since_cursor,
            conflicts_with_all = ["change", "all", "lineage", "branches"])]
        since: Option<String>,
        /// With --since: only the merges, each with its base, committer time and whether it
        /// merged outside bay, and the count of commits off the first-parent line
        #[arg(long, requires = "since")]
        merged: bool,
    },
    /// Delete the remote's branches merged by ancestor or patch, each under a lease of the tip
    /// judged, after archiving the tip on the remote and logging the push that restores it to
    /// bay/pruned-heads.log in the repository's common directory; then runs the config's
    /// `[plugin.prune]` plugin, when set, once per deleted branch
    Prune {
        /// A branch with commits off main whose tip is older than this is stale, and kept
        /// (default 30)
        #[arg(long, value_name = "DAYS")]
        stale_days: Option<u64>,
    },
    /// Send one talk to one agent: one action, one target, the time the reply is owed. Arrival is
    /// the recipient's ack, written when its bay fetches
    Talk(Box<TalkArgs>),
    /// Admin: add one key to the key registry: append it to its class file, sign the new
    /// registry with this machine's registry key (`bay.registryKey`) and push it. The first
    /// key enrolled is that registry key. With --remove, take a name's keys out instead
    Enrol(EnrolArgs),
    /// Admin: probe this machine (git and Claude Code floors, remote, hook entries, installed skill)
    Doctor {
        /// Judge this machine in this role instead of the one doctor infers
        #[arg(long, value_parser = ["runner"])]
        role: Option<String>,
    },
    /// Admin: place bay at $HOME/.local/bin/bay, its skill and its project hook entries
    Install(InstallArgs),
    /// Admin: create a bay-hosted main: a bare repository on reftable whose main is append-only
    Host(HostArgs),
    /// Admin: the runner that finishes operations after their agents leave
    Runner {
        #[command(subcommand)]
        action: RunnerAction,
    },
    /// This machine's live summary ref (started by the agent hook once per heartbeat)
    #[command(hide = true)]
    Live {
        #[command(subcommand)]
        action: LiveAction,
    },
    /// Admin: push a change and its evidence to a remote, or write them to a bundle
    Share {
        change: String,
        /// Write a bundle instead of pushing
        #[arg(long, value_name = "FILE")]
        bundle: Option<PathBuf>,
        /// The remote to push to (default: the configured remote)
        #[arg(long)]
        remote: Option<String>,
    },
    /// Admin: queries over evidence records for an outside producer
    Evidence {
        #[command(subcommand)]
        action: EvidenceAction,
    },
    /// Admin: replace this binary with a verified release (this machine's pin wins)
    Update {
        /// Move to this exact version (X.Y.Z or X.Y.Z-suffix)
        #[arg(long)]
        version: Option<String>,
        /// Report the installed version, the target and whether an update is due; change nothing
        #[arg(long)]
        check: bool,
    },
    /// Admin: ask about a large file's stored bytes; changes nothing
    Assets {
        #[command(subcommand)]
        action: AssetsAction,
    },
    /// Admin: prove a plugin keeps its contract, run as bay runs it; changes nothing
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },
    /// Admin: the agent hook entry point (`bay hook <event>`), which `bay install` writes into the
    /// agent's settings; no one runs it by hand, so `bay --help` does not list it (0.4.0)
    #[command(hide = true)]
    Hook {
        /// The agent product's event, for example `tool.before`
        event: String,
    },
    /// Every command and flag on one page, grouped by who runs it; or the help of one command.
    /// With --json: the same list for programs, every reason code, and the JSON Schema of
    /// bay.result/1
    Help {
        /// The command, for example `bay host` (default: bay itself)
        #[arg(value_name = "COMMAND")]
        command: Vec<String>,
    },
}

/// `status --since [<CURSOR>]`: empty (from the start), or a cursor bay returned.
fn since_cursor(text: &str) -> Result<String, String> {
    if !text.is_empty() {
        bay::watch::Cursor::parse(text)?;
    }
    Ok(text.to_owned())
}

/// `bay enrol <class> <name> <public-key-file>`.
#[derive(Args)]
struct EnrolArgs {
    /// The key's class
    #[arg(value_parser = ["agent", "runner", "producer", "reviewer", "approver", "registry"])]
    class: String,
    /// The name the key is enrolled under
    name: String,
    /// The public key file (`<key>.pub`)
    #[arg(value_name = "PUBLIC_KEY_FILE", required_unless_present = "remove")]
    public_key: Option<PathBuf>,
    /// Remove the name's keys from its class instead, each recorded with --reason
    #[arg(long, requires = "reason", conflicts_with = "public_key")]
    remove: bool,
    /// With --remove: `rotated` keeps the key's past records verified; `revoked` (compromised)
    /// makes every record it signed unverified
    #[arg(long, value_name = "REASON", value_parser = ["rotated", "revoked"], requires = "remove")]
    reason: Option<String>,
}

/// `--wait[=SECONDS]`, the one flag `bay merge` and `bay undo` share. `Option<Option<_>>` is
/// clap's flag with an optional value: absent, present alone, or present with SECONDS.
#[derive(Args)]
#[allow(clippy::option_option)]
struct WaitArgs {
    /// Run the merge here and return its outcome, instead of returning at once. With
    /// SECONDS: watch it that long, then return while the run goes on
    #[arg(long, value_name = "SECONDS", num_args = 0..=1, require_equals = true)]
    wait: Option<Option<u64>>,
}

impl WaitArgs {
    /// Whether to wait, and for how long at most: `None` waits for the outcome.
    fn wait(&self) -> (bool, Option<std::time::Duration>) {
        (
            self.wait.is_some(),
            self.wait.flatten().map(std::time::Duration::from_secs),
        )
    }
}

#[derive(Args)]
struct InstallArgs {
    /// Only the machine's part: the binary and the skill. It never writes hook entries, a
    /// repository's agent settings or its Git config: hooks reach a clone with the project's
    /// tracked settings file
    #[arg(long)]
    machine: bool,
    /// The repository's main branch, written to git config bay.main (default: what it holds,
    /// else the remote's default branch)
    #[arg(long = "main", value_name = "BRANCH", conflicts_with = "machine")]
    trunk: Option<String>,
    /// Main's remote, written to git config bay.remote (default: what it holds, else
    /// origin)
    #[arg(long, conflicts_with = "machine")]
    remote: Option<String>,
}

#[derive(Args)]
struct TalkArgs {
    /// The agent who owes the reply (a person's name works too)
    #[arg(value_name = "AGENT")]
    actor: Option<String>,
    /// Send to the agent that merged this change (a change id or its commit on main), read from
    /// bay's merge record; then the one argument is the action
    #[arg(long, value_name = "CHANGE")]
    to_change: Option<String>,
    /// The action: the decision, the question or the change asked for
    what: Option<String>,
    /// delivery (one change or artifact), discovery (one decision) or reliability (one check)
    #[arg(long, value_name = "KIND")]
    kind: Option<String>,
    /// Delivery: the change asked for
    #[arg(long, value_name = "CHANGE")]
    change: Vec<String>,
    /// Delivery: the artifact asked for
    #[arg(long, value_name = "PATH")]
    artifact: Vec<String>,
    /// Discovery: the decision asked for
    #[arg(long, value_name = "DECISION")]
    decision: Vec<String>,
    /// Reliability: the check asked for
    #[arg(long, value_name = "CHECK")]
    check: Vec<String>,
    /// When the reply is owed, a UTC time after now (2026-10-19T12:00:00Z)
    #[arg(long, value_name = "TIME")]
    follow_up: Option<String>,
    /// The agent who decides if the two disagree (a person's name works too)
    #[arg(long, value_name = "AGENT")]
    decides: Option<String>,
    /// The sender's reading of the fact
    #[arg(long)]
    context: Option<String>,
    /// A pointer to evidence already in Git: a commit, a path or a change id
    #[arg(long)]
    evidence: Option<String>,
    /// The talk this one replaces (with --answer: the standing answer's commit)
    #[arg(long, value_name = "ID")]
    supersedes: Option<String>,
    /// After this UTC time the ask is void
    #[arg(long, value_name = "TIME")]
    expires: Option<String>,
    /// The result the sender intends (at most one of --target, --forecast, --commitment)
    #[arg(long)]
    target: Option<String>,
    /// The result the sender expects from evidence already in Git
    #[arg(long)]
    forecast: Option<String>,
    /// The result the recipient promises
    #[arg(long)]
    commitment: Option<String>,
    /// What the sender owns in this ask
    #[arg(long)]
    owns: Option<String>,
    /// What the sender promises in return
    #[arg(long)]
    promises: Option<String>,
    /// What the sender needs to go on
    #[arg(long)]
    needs: Option<String>,
    /// The evidence that will show the promise kept
    #[arg(long)]
    promise_evidence: Option<String>,
    /// Mark the talk urgent
    #[arg(long)]
    urgent: bool,
    /// Run every refusal and commit nothing
    #[arg(long)]
    dry_run: bool,
    /// Answer the discovery talk with this id instead of sending one
    #[arg(long, value_name = "ID", conflicts_with_all = ["actor", "what", "to_change", "kind", "change",
        "artifact", "decision", "check", "follow_up", "decides", "context", "evidence",
        "expires", "target", "forecast", "commitment", "owns", "promises", "needs",
        "promise_evidence", "urgent", "dry_run"])]
    answer: Option<String>,
    /// With --answer: the decision value
    #[arg(long, requires = "answer")]
    value: Option<String>,
}

#[derive(Subcommand)]
enum HostAction {
    /// The host hook Git runs on a push (configured by `bay host`)
    #[command(hide = true)]
    Hook { event: String, args: Vec<String> },
}

/// `bay host <path>` (0.4.0, BACKLOG 51 R7: one word for one step), and the hidden host hook
/// that a host's own hook files call.
#[derive(Args)]
#[command(
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true,
    override_usage = "bay host <PATH> [--main <BRANCH>] [--object-format <FORMAT>]"
)]
struct HostArgs {
    #[command(subcommand)]
    action: Option<HostAction>,
    /// The bare repository to create
    #[arg(required = true)]
    path: Option<PathBuf>,
    /// The name of main
    #[arg(long = "main", value_name = "BRANCH", default_value = "main")]
    trunk: String,
    /// By default the object format of the repository bay runs in, else Git's default
    #[arg(long, value_parser = ["sha1", "sha256"])]
    object_format: Option<String>,
}

#[derive(Subcommand)]
enum LiveAction {
    /// Write this machine's summary of its sessions and push refs/bay/live/<machine>
    Push,
    /// Checkpoint a change's view and push its chain (started by the agent hook at a session's
    /// end and on its idle timer)
    Checkpoint { change: String },
}

#[derive(Subcommand)]
enum EvidenceAction {
    /// Main's first-parent commits with no record of the kind from the producer about that
    /// commit (by commit, or by base tree and tree; never by tree alone)
    Missing {
        /// The record kind, for example `check` or `review`
        #[arg(long, value_parser = clap::builder::NonEmptyStringValueParser::new())]
        kind: String,
        /// The producer whose records count
        #[arg(long, value_parser = bay::coverage::producer_name)]
        producer: String,
        /// List only the commits after this rev
        #[arg(long, value_name = "REV")]
        since: Option<String>,
    },
    /// Write a signed record about merges into main, as an outside producer or reviewer, or about
    /// a candidate, as an approver: one record per --commit, pushed once (a rerun with the same
    /// content writes nothing)
    Record {
        /// The record kind, for example `review` or `approval`; a `review` is signed by
        /// bay.reviewerKey, an `approval` or a `waiver` by bay.approverKey
        #[arg(long, value_parser = clap::builder::NonEmptyStringValueParser::new())]
        kind: String,
        /// `pass`, `product_failure`, `infra_failure` or `unknown`; there is no default
        #[arg(long)]
        verdict: String,
        /// A merge the record is about (for an approval, the candidate commit the merge names);
        /// repeat for a range review
        #[arg(long = "commit", value_name = "COMMIT", required = true)]
        commits: Vec<String>,
        /// The check id the record answers; the kind when not named
        #[arg(long)]
        check: Option<String>,
        /// The environment of that check, as the policy names it (for a post check's record);
        /// an approval takes its approval check's, and any other record `any`, when not named
        #[arg(long)]
        environment: Option<String>,
        /// A `key=value` the record keeps as its inputs; repeatable
        #[arg(long = "input", value_name = "KEY=VALUE")]
        inputs: Vec<String>,
        /// A JSON object whose fields each record keeps inside its signed bytes (for example
        /// findings)
        #[arg(long, value_name = "FILE")]
        body: Option<std::path::PathBuf>,
        /// The writer's own version, part of the record's id; bay's version when not named
        #[arg(long)]
        version: Option<String>,
        /// Why, in the writer's words: kept in the signed record; a `waiver` needs one
        #[arg(long, value_name = "TEXT")]
        why: Option<String>,
    },
}

#[derive(Subcommand)]
enum AssetsAction {
    /// Whether a store may purge an object: held when main's `[assets] hold` covers any path
    /// main's history ever saved it under. A store asks before every purge
    Held {
        /// The object's identity, `sha256:<64 hex>`
        id: String,
    },
}

#[derive(Subcommand)]
enum PluginAction {
    /// Run the contract cases of one plugin kind against a command; exits 0 only when every
    /// case passes
    Test(AdapterTestArgs),
}

#[derive(Args)]
struct AdapterTestArgs {
    /// The plugin kind: `check` (a policy's `[[check]]`), `wake` (`[plugin.wake] command`),
    /// `notify` (`[plugin.notify] command`) or `assets` (`[plugin.assets] command`)
    #[arg(value_parser = bay::adapter::KINDS)]
    kind: String,
    /// Check or assets: the entry's `timeout_s` (default: the policy's, or the plugin's)
    #[arg(long, value_name = "SECONDS")]
    timeout_s: Option<u64>,
    /// Check: the entry's `expected_minimum`
    #[arg(long, value_name = "N", default_value = "1")]
    expected_minimum: u64,
    /// Check: a directory of the entry's `path` (repeatable; default: the policy's default)
    #[arg(long = "path", value_name = "DIR")]
    path: Vec<String>,
    /// Check: a variable name of the entry's `env`, passed from this environment (repeatable)
    #[arg(long = "env", value_name = "NAME")]
    env: Vec<String>,
    /// Check: another plugin path the command reads, relative to the repository (repeatable)
    #[arg(long = "with", value_name = "PATH")]
    with: Vec<String>,
    /// The plugin's command, as the config or policy names it: a program path relative to
    /// the repository, then its arguments
    #[arg(last = true, required = true, value_name = "COMMAND")]
    command: Vec<String>,
}

#[derive(Subcommand)]
enum RunnerAction {
    /// Drive queued operations to an outcome on this machine
    Run {
        /// Make one pass and exit
        #[arg(long)]
        once: bool,
        /// Seconds between passes
        #[arg(long, default_value = "10")]
        interval_s: u64,
    },
    /// Write a service that keeps `bay runner run` running for this repository (a launchd
    /// agent on macOS, a systemd user unit on Linux), and print the one command that starts it
    Install {
        /// Show the service file and write nothing
        #[arg(long)]
        print: bool,
        /// Replace a service file that is already there
        #[arg(long)]
        force: bool,
    },
}

/// The `CONFIG_MISSING` refusal of a command that needs the trunk or policy, so needs the
/// `.bay/config.toml` committed at trunk (ruling: config location), before anything else the
/// command does.
/// The host, doctor's box rows, install, update and the agent hook do not.
fn config_refusal(cli: &Cli) -> Option<BayResult> {
    // `status --all` never fetches (G49): it reads the config without a fetch itself.
    let needs_config = !matches!(
        cli.command,
        Command::Status { all: true, .. }
            | Command::Host(_)
            | Command::Doctor { .. }
            | Command::Install(_)
            | Command::Update { .. }
            | Command::Hook { .. }
            | Command::Plugin { .. }
            | Command::Help { .. }
    );
    needs_config
        .then(|| bay::repo::Repo::config_missing(&cli.dir))
        .flatten()
}

fn install(dir: &std::path::Path, args: InstallArgs) -> BayResult {
    let trunk = bay::install::Trunk {
        trunk: args.trunk,
        remote: args.remote,
    };
    bay::install::install_with(dir, args.machine, &trunk)
}

/// The command line. The file every child `bay` starts from is resolved now, before an update
/// can replace the running one; the executor pins its own first (`bay::update::pin_own_binary`).
fn parse() -> Cli {
    // R6 (ruling 2026-10-22): `bay --help` is the page `bay help` prints (G303), grouped for
    // people, agents and admins; `bay <command> --help` stays clap's help of that command.
    if asks_for_the_page() {
        print!("{}", bay::guide::guide(Cli::command()));
        std::process::exit(0);
    }
    let cli = Cli::parse();
    if !matches!(cli.command, Command::Runner { .. }) {
        let _ = bay::update::own_binary();
    }
    cli
}

/// Whether the command line is `bay --help` (or `-h`) with no command: only bay's global
/// options (`-C <PATH>`, `--json`) beside it.
fn asks_for_the_page() -> bool {
    let mut words = std::env::args_os().skip(1);
    let mut help = false;
    while let Some(word) = words.next() {
        if word == "--help" || word == "-h" {
            help = true;
        } else if word == "-C" {
            let _ = words.next();
        } else if word != "--json" && !word.to_string_lossy().starts_with("-C") {
            return false;
        }
    }
    help
}

/// Whether stdout is a terminal a person reads. A debug build's tests say so with
/// `BAY_TEST_STDOUT_TERMINAL`, since a test holds no terminal; a release build never reads it.
fn stdout_is_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
        || (option_env!("BAY_RELEASE_VERSION").is_none()
            && std::env::var_os("BAY_TEST_STDOUT_TERMINAL").is_some())
}

/// `bay runner run`: its result, then, between operations, the new version in its place.
fn executor_run(dir: &std::path::Path, json: bool, once: bool, interval_s: u64) -> ExitCode {
    let result = bay::executor::run(dir, once, std::time::Duration::from_secs(interval_s));
    bay::coord::after_command();
    let code = render(&result, json);
    // Returns only when the executor does not move, or cannot.
    bay::executor::reexec(&result);
    code
}

/// `bay approve` or `bay reject`: the approval its arguments ask for.
fn approve_change(dir: &std::path::Path, command: Command) -> BayResult {
    let (change, reject, waive, why) = match command {
        Command::Approve { change, waive, why } => (change, false, waive, why),
        Command::Reject { change, why } => (change, true, None, why),
        _ => unreachable!("approve_change takes an approve or reject command"),
    };
    bay::approve::approve(
        dir,
        &bay::approve::Approve {
            change,
            reject,
            waive,
            why,
        },
    )
}

/// `bay abandon [CHANGE]` (G301): the abandon request; with no change, the change of the view
/// bay runs in.
fn abandon(dir: &std::path::Path, change: Option<&str>) -> BayResult {
    bay::publish::land(dir, change, false, None, true)
}

/// `bay save`: the request its arguments make.
fn save(dir: &std::path::Path, intent: String, message: Option<String>) -> BayResult {
    bay::save::save(dir, &bay::save::SaveRequest { intent, message })
}

/// `bay enrol`: the key its arguments name.
fn enrol(dir: &std::path::Path, args: EnrolArgs) -> BayResult {
    bay::enrol::enrol(
        dir,
        &bay::enrol::Enrol {
            class: args.class,
            name: args.name,
            public_key: args.public_key,
            remove: args
                .reason
                .as_deref()
                .and_then(bay::registry::Removed::named),
        },
    )
}

/// `bay start`: the request its arguments make.
fn start_change(dir: &std::path::Path, command: Command) -> BayResult {
    let Command::Start {
        intent,
        paths,
        inputs,
        new,
        fixes,
        lane,
        from,
        after,
    } = command
    else {
        unreachable!("start_change takes a start command");
    };
    bay::start::start(
        dir,
        &bay::start::StartRequest {
            intent: Some(intent),
            new,
            intent_revision: "1".to_owned(),
            paths: bay::start::declared_paths(paths),
            inputs,
            resume: None,
            reverts: None,
            fixes,
            lane,
            from,
            after,
            saved: None,
        },
    )
}

fn main() -> ExitCode {
    let cli = parse();
    if let Some(refusal) = config_refusal(&cli) {
        return render(&refusal, cli.json);
    }
    let result = match cli.command {
        start @ Command::Start { .. } => start_change(&cli.dir, start),
        Command::Save { intent, message } => save(&cli.dir, intent, message),
        Command::Waiting => bay::approve::waiting(&cli.dir),
        approve @ (Command::Approve { .. } | Command::Reject { .. }) => {
            approve_change(&cli.dir, approve)
        }
        Command::Undo { change, wait } => {
            let (wait, observe) = wait.wait();
            bay::undo::undo(&cli.dir, &change, wait, observe)
        }
        Command::Share {
            change,
            bundle,
            remote,
        } => bay::share::share(&cli.dir, &change, bundle.as_deref(), remote.as_deref()),
        Command::Merge { change, wait } => {
            let (wait, observe) = wait.wait();
            bay::publish::land(&cli.dir, change.as_deref(), wait, observe, false)
        }
        Command::Abandon { change } => abandon(&cli.dir, change.as_deref()),
        Command::Host(HostArgs {
            action: Some(HostAction::Hook { event, args }),
            ..
        }) => return ExitCode::from(bay::host::hook(&event, &args)),
        Command::Host(HostArgs {
            path,
            trunk,
            object_format,
            ..
        }) => {
            let path = path.expect("clap requires the path without a subcommand");
            bay::host::init(&cli.dir, &path, &trunk, object_format.as_deref())
        }
        Command::Runner {
            action: RunnerAction::Run { once, interval_s },
        } => return executor_run(&cli.dir, cli.json, once, interval_s),
        Command::Runner {
            action: RunnerAction::Install { print, force },
        } => bay::service::install(&cli.dir, &bay::service::Request { print, force }),
        Command::Live { action } => live(&cli.dir, action),
        command @ Command::Status { .. } => status(&cli.dir, command),
        // The one prune: the request `status --branches` makes, deleting (G302).
        Command::Prune { stale_days } => heads(&cli.dir, true, stale_days),
        Command::Talk(args) => talk(&cli.dir, *args),
        Command::Evidence { action } => evidence(&cli.dir, action),
        Command::Enrol(args) => enrol(&cli.dir, args),
        Command::Doctor { role } => doctor(&cli.dir, role.as_deref()),
        Command::Install(args) => install(&cli.dir, args),
        Command::Update { version, check } => {
            bay::update::update(&bay::update::Request { version, check })
        }
        Command::Assets {
            action: AssetsAction::Held { id },
        } => bay::assets::held_answer(&cli.dir, &id),
        Command::Plugin { action } => plugin(&cli.dir, action),
        Command::Help { command } => return help(&command, cli.json),
        Command::Hook { event } => {
            // The agent hook follows the agent product's exit protocol (INTERFACES § 1): for
            // Claude Code, exit 2 denies. The payload adapter decides (agent_hook.rs).
            let mut input = String::new();
            let _ = std::io::stdin().read_to_string(&mut input);
            let outcome = bay::agent_hook::dispatch(&event, &input, &cli.dir);
            bay::live::after_event(&cli.dir, &input);
            print!("{}", outcome.stdout);
            eprint!("{}", outcome.stderr);
            return ExitCode::from(outcome.exit);
        }
    };
    // A command that fetched trunk leaves the coordination cache the `tool.before` hook reads
    // (`DESIGN` § 11); a hook never writes it.
    bay::coord::after_command();
    render(&result, cli.json)
}

/// `bay help [COMMAND]`: with no command, every command, subcommand and flag on one page
/// (`bay::guide`, G303); with one, the help `bay COMMAND --help` prints; with `--json`, the
/// contract (`bay::contract`). All three are read from this command line.
fn help(command: &[String], json: bool) -> ExitCode {
    if json {
        return render(&bay::contract::contract(Cli::command()), true);
    }
    if command.is_empty() {
        print!("{}", bay::guide::guide(Cli::command()));
        return ExitCode::SUCCESS;
    }
    let args = std::iter::once("bay".to_owned())
        .chain(command.iter().cloned())
        .chain(std::iter::once("--help".to_owned()));
    // Prints the help and exits 0, as `--help` does; an unknown command is a usage error.
    let _ = Cli::command().get_matches_from(args);
    ExitCode::SUCCESS
}

/// `bay doctor`, then the daily update check.
fn doctor(dir: &std::path::Path, role: Option<&str>) -> BayResult {
    let result = bay::doctor::doctor_as(dir, role == Some("runner"));
    bay::update::auto();
    result
}

/// `bay plugin test <kind> -- <command...>`: the conformance kit, which changes nothing.
fn plugin(dir: &std::path::Path, action: PluginAction) -> BayResult {
    let PluginAction::Test(args) = action;
    bay::adapter::test(
        dir,
        &bay::adapter::Test {
            kind: args.kind,
            command: args.command,
            timeout_s: args.timeout_s,
            expected_minimum: args.expected_minimum,
            path: args.path,
            env: args.env,
            with: args.with,
        },
    )
}

/// `bay evidence <action>`.
/// The hidden `bay live` commands a hook starts: push the live summary, or checkpoint a draft.
fn live(dir: &std::path::Path, action: LiveAction) -> BayResult {
    match action {
        LiveAction::Push => bay::live::push(dir),
        LiveAction::Checkpoint { change } => bay::start::checkpoint(dir, &change),
    }
}

/// The census of the remote's branches, and with `prune` the prune of the landed ones: the one
/// path of `bay status --branches` and `bay prune` (G302).
fn heads(dir: &std::path::Path, prune: bool, stale_days: Option<u64>) -> BayResult {
    bay::heads::heads(dir, &bay::heads::Request { prune, stale_days })
}

/// `bay status` and its read-only views: the fleet map, stranded work, lineage pairs, every
/// head, or one change's state (which alone may start the daily update check).
fn status(dir: &std::path::Path, command: Command) -> BayResult {
    let Command::Status {
        change,
        all,
        lineage,
        branches: every_branch,
        stale_days,
        since,
        merged,
    } = command
    else {
        unreachable!("status takes a status command");
    };
    if every_branch {
        return heads(dir, false, stale_days);
    }
    if all {
        return bay::fleet::fleet(dir);
    }
    // 0.3.0: what `bay status --since` was; an empty cursor reads from the start.
    if let Some(since) = since {
        // `since_cursor` parsed it already: a bad cursor is a usage error.
        let cursor = Some(since.as_str())
            .filter(|text| !text.is_empty())
            .and_then(|text| bay::watch::Cursor::parse(text).ok());
        return if merged {
            bay::watch::landed(dir, cursor.as_ref())
        } else {
            bay::watch::watch(dir, cursor.as_ref())
        };
    }
    if let Some(range) = lineage {
        return bay::lineage::lineage(dir, Some(range.as_str()));
    }
    let result = bay::status::status(dir, change.as_deref());
    bay::update::auto();
    result
}

fn evidence(dir: &std::path::Path, action: EvidenceAction) -> BayResult {
    match action {
        EvidenceAction::Missing {
            kind,
            producer,
            since,
        } => bay::coverage::missing(
            dir,
            &bay::coverage::Missing {
                kind,
                producer,
                since,
            },
        ),
        EvidenceAction::Record {
            kind,
            verdict,
            commits,
            check,
            environment,
            inputs,
            body,
            version,
            why,
        } => bay::outside::record(
            dir,
            &bay::outside::Record {
                kind,
                verdict,
                commits,
                check,
                environment,
                inputs,
                body,
                version,
                why,
            },
        ),
    }
}

/// `bay talk`: an answer with `--answer`, else one ask.
fn talk(dir: &std::path::Path, args: TalkArgs) -> BayResult {
    if let Some(talk) = args.answer {
        return bay::talk::answer(
            dir,
            &bay::talk::Answer {
                talk,
                value: args.value,
                supersedes: args.supersedes,
            },
        );
    }
    // With --to-change the recipient is resolved, so a single argument is the action.
    let (who, what) = match (&args.to_change, args.what) {
        (Some(_), None) => (None, args.actor),
        (_, what) => (args.actor, what),
    };
    bay::talk::talk(
        dir,
        &bay::talk::Ask {
            who,
            what,
            kind: args.kind,
            change: args.change,
            artifact: args.artifact,
            decision: args.decision,
            check: args.check,
            follow_up: args.follow_up,
            decides: args.decides,
            context: args.context,
            evidence: args.evidence,
            supersedes: args.supersedes,
            expires: args.expires,
            target: args.target,
            forecast: args.forecast,
            commitment: args.commitment,
            owns: args.owns,
            promises: args.promises,
            needs: args.needs,
            promise_evidence: args.promise_evidence,
            urgent: args.urgent,
            dry_run: args.dry_run,
            to_change: args.to_change,
            ..bay::talk::Ask::default()
        },
    )
}

fn render(result: &BayResult, json: bool) -> ExitCode {
    // G313 (ruling 2026-10-22, ch. 35 R5): a person at a terminal reads the plain sentence
    // first, with no document; a program reads stdout through a pipe, or passes `--json`, and
    // always gets the result document, with human text on stderr (`INTERFACES` § 1).
    if !json && stdout_is_terminal() {
        print!("{}", result.to_terminal());
        return ExitCode::from(u8::try_from(result.decision.exit_code()).unwrap_or(20));
    }
    match result.to_json() {
        Ok(text) => print!("{text}"),
        Err(error) => {
            eprintln!("result serialization failed: {error}");
            return ExitCode::from(20);
        }
    }
    if !json {
        eprint!("{}", result.to_human());
    }
    ExitCode::from(u8::try_from(result.decision.exit_code()).unwrap_or(20))
}
